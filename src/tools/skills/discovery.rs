//! Bounded workspace discovery and loading of explicit skill manifests.

use super::manifest::{
    parse_manifest, valid_skill_name, ParsedSkill, SkillInstructions, SkillLoadError,
    SkillManifestError, MAX_SKILL_MANIFEST_BYTES,
};
use crate::tools::{list_task_directory_names_at, read_text_file_at, WorkspaceRoot};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Project-relative directory holding one explicit manifest per JSON file.
pub(crate) const SKILL_MANIFEST_DIRECTORY: &str = ".krypper/skills";

/// Maximum number of manifests in one load, including host-supplied paths.
pub(crate) const MAX_SKILL_MANIFESTS: usize = 64;

const MAX_MANIFEST_PATH_BYTES: usize = 4 * 1024;
const MAX_MANIFEST_PATH_DEPTH: usize = 16;
const MANIFEST_EXTENSION: &str = ".json";

/// Loaded skill data. Loading neither registers a tool nor grants any authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) instructions: SkillInstructions,
    pub(crate) manifest_path: PathBuf,
}

/// A malformed JSON file skipped while loading the remaining manifests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkippedSkillManifest {
    pub(crate) path: PathBuf,
    pub(crate) kind: SkippedSkillManifestKind,
}

/// Bounded categories reported to the host without exposing manifest contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkippedSkillManifestKind {
    MalformedJson,
}

/// The successfully loaded skills and per-file syntax diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SkillLoadReport {
    pub(crate) skills: Vec<LoadedSkill>,
    pub(crate) skipped_manifests: Vec<SkippedSkillManifest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SkillFileError {
    MalformedJson,
    Fatal(SkillLoadError),
}

impl From<SkillLoadError> for SkillFileError {
    fn from(error: SkillLoadError) -> Self {
        Self::Fatal(error)
    }
}

/// Discover project manifests and load them with the host's explicit paths.
///
/// Results are ordered by skill name then manifest path. Duplicate names fail
/// closed. Every path is read through the existing workspace containment walk.
pub(crate) fn load_skills(
    root: &WorkspaceRoot,
    explicit_paths: &[PathBuf],
) -> Result<SkillLoadReport, SkillLoadError> {
    let mut paths = discover_manifest_paths(root)?;
    for path in explicit_paths {
        paths.push(normalize_manifest_path(path)?);
    }
    paths.sort();
    paths.dedup();
    if paths.len() > MAX_SKILL_MANIFESTS {
        return Err(SkillLoadError::TooManyManifests);
    }

    let mut names = BTreeSet::new();
    let mut report = SkillLoadReport {
        skills: Vec::with_capacity(paths.len()),
        skipped_manifests: Vec::new(),
    };
    for path in paths {
        let skill = match load_one(root, &path) {
            Ok(skill) => skill,
            Err(SkillFileError::MalformedJson) => {
                report.skipped_manifests.push(SkippedSkillManifest {
                    path,
                    kind: SkippedSkillManifestKind::MalformedJson,
                });
                continue;
            }
            Err(SkillFileError::Fatal(error)) => return Err(error),
        };
        if !names.insert(skill.name.clone()) {
            return Err(SkillLoadError::DuplicateName);
        }
        report.skills.push(skill);
    }
    report.skills.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.manifest_path.cmp(&right.manifest_path))
    });
    Ok(report)
}

fn discover_manifest_paths(root: &WorkspaceRoot) -> Result<Vec<PathBuf>, SkillLoadError> {
    let directory = Path::new(SKILL_MANIFEST_DIRECTORY);
    if !project_directory_exists(root, directory)? {
        return Ok(Vec::new());
    }
    reject_symlinks(root, directory)?;
    let entries = list_task_directory_names_at(root, directory.as_os_str()).map_err(|error| {
        // CliError exposes messages only; these are the boundary's stable denial strings.
        map_workspace_message(error.message())
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let Some(entry) = entry.to_str() else {
            continue;
        };
        if !entry.ends_with(MANIFEST_EXTENSION) {
            continue;
        }
        let stem = &entry[..entry.len() - MANIFEST_EXTENSION.len()];
        if !valid_skill_name(stem) {
            return Err(SkillLoadError::InvalidManifest);
        }
        let path = directory.join(entry);
        reject_symlinks(root, &path)?;
        paths.push(path);
    }
    Ok(paths)
}

fn project_directory_exists(
    root: &WorkspaceRoot,
    directory: &Path,
) -> Result<bool, SkillLoadError> {
    match fs::symlink_metadata(root.path().join(directory)) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SkillLoadError::Symlink),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(SkillLoadError::InvalidManifest),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(SkillLoadError::ReadFailed),
    }
}

fn reject_symlinks(root: &WorkspaceRoot, relative: &Path) -> Result<(), SkillLoadError> {
    let mut current = root.path().to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|_| SkillLoadError::NotFound)?;
        if metadata.file_type().is_symlink() {
            return Err(SkillLoadError::Symlink);
        }
    }
    Ok(())
}

fn normalize_manifest_path(path: &Path) -> Result<PathBuf, SkillLoadError> {
    if path.as_os_str().is_empty() || path.as_os_str().len() > MAX_MANIFEST_PATH_BYTES {
        return Err(SkillLoadError::InvalidPath);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => return Err(SkillLoadError::OutsideWorkspace),
            Component::RootDir | Component::Prefix(_) => return Err(SkillLoadError::InvalidPath),
        }
    }
    if normalized.as_os_str().is_empty()
        || normalized.components().count() > MAX_MANIFEST_PATH_DEPTH
    {
        return Err(SkillLoadError::InvalidPath);
    }
    Ok(normalized)
}

fn load_one(root: &WorkspaceRoot, path: &Path) -> Result<LoadedSkill, SkillFileError> {
    reject_symlinks(root, path)?;
    let contents = read_text_file_at(root, path.as_os_str())
        .map_err(|error| map_workspace_message(error.message()))?;
    if contents.len() > MAX_SKILL_MANIFEST_BYTES {
        return Err(SkillFileError::Fatal(SkillLoadError::TooLarge));
    }
    let name = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or(SkillLoadError::InvalidManifest)?;
    let ParsedSkill {
        name,
        description,
        instructions,
    } = match parse_manifest(&contents, name) {
        Ok(manifest) => manifest,
        Err(SkillManifestError::MalformedJson) => return Err(SkillFileError::MalformedJson),
        Err(SkillManifestError::InvalidManifest) => {
            return Err(SkillFileError::Fatal(SkillLoadError::InvalidManifest));
        }
    };
    Ok(LoadedSkill {
        name,
        description,
        instructions,
        manifest_path: path.to_path_buf(),
    })
}

fn map_workspace_message(message: &str) -> SkillLoadError {
    match message {
        "absolute paths are not allowed" | "invalid path" => SkillLoadError::InvalidPath,
        "path must stay inside the current directory" => SkillLoadError::OutsideWorkspace,
        "file not found" => SkillLoadError::NotFound,
        "file exceeds 1 MiB" => SkillLoadError::TooLarge,
        "file is not valid UTF-8" | "not a regular file" | "not a directory" => {
            SkillLoadError::InvalidManifest
        }
        _ => SkillLoadError::ReadFailed,
    }
}
