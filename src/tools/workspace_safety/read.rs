//! Reading text files and AGENTS.md guidance through the containment walk.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use super::paths::{open_cwd_relative, open_workspace_relative, verify_root_descriptor};
use super::syscalls::{
    duplicate_directory, fstat_directory, fstat_file, is_directory, is_not_found, is_regular_file,
    is_symlink, is_symlink_loop, open_at, open_current_directory, open_directory_path,
    same_file_snapshot, stat_at, OpenRequest,
};
use super::types::{
    CliError, DirHandle, FileStatus, RootGuidanceReadError, WorkspaceRoot, MAX_FILE_SIZE,
    MAX_ROOT_AGENTS_BYTES,
};

const NESTED_GUIDANCE_MARKER: &str = "\n\n--- AGENTS.md: ";

pub(crate) fn capture_workspace_root() -> Result<WorkspaceRoot, CliError> {
    let descriptor = open_current_directory()
        .map_err(|error| CliError::from_io(&error, "unable to open current directory"))?;
    let path = fs::canonicalize(".")
        .map_err(|error| CliError::new(format!("unable to resolve current directory: {error}")))?;
    verify_root_descriptor(&descriptor, &path).map_err(|error| {
        CliError::new(format!(
            "unable to verify current directory: {}",
            error.message()
        ))
    })?;
    Ok(WorkspaceRoot { descriptor, path })
}

pub(crate) fn capture_workspace_root_at(path: &Path) -> Result<WorkspaceRoot, CliError> {
    capture_workspace_root_at_with_error(path)
}

pub(crate) fn capture_workspace_root_at_with_error(path: &Path) -> Result<WorkspaceRoot, CliError> {
    let path = fs::canonicalize(path)
        .map_err(|error| CliError::new(format!("unable to resolve workspace path: {error}")))?;
    let descriptor = open_directory_path(&path)
        .map_err(|error| CliError::from_io(&error, "unable to open workspace path"))?;
    verify_root_descriptor(&descriptor, &path).map_err(|error| {
        CliError::new(format!(
            "unable to verify workspace path: {}",
            error.message()
        ))
    })?;
    Ok(WorkspaceRoot { descriptor, path })
}

pub(crate) fn read_text_file(path: &OsStr) -> Result<String, CliError> {
    read_open_text_file(open_cwd_relative(path, false)?)
}

pub(crate) fn read_text_file_at(root: &WorkspaceRoot, path: &OsStr) -> Result<String, CliError> {
    read_open_text_file(open_workspace_relative(root, path, false)?)
}

pub(crate) fn read_root_agents_md_at(
    root: &WorkspaceRoot,
) -> Result<Option<String>, RootGuidanceReadError> {
    read_agents_md_at(&root.descriptor, |file| {
        let mut contents = Vec::with_capacity(MAX_ROOT_AGENTS_BYTES + 1);
        file.take((MAX_ROOT_AGENTS_BYTES + 1) as u64)
            .read_to_end(&mut contents)?;
        Ok(contents)
    })
}

#[cfg(test)]
pub(super) fn read_root_agents_md_at_with_reader(
    root: &WorkspaceRoot,
    read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    read_agents_md_at(&root.descriptor, read)
}

pub(crate) fn read_project_guidance_at(
    root: &WorkspaceRoot,
    targets: &[PathBuf],
) -> Result<Option<String>, RootGuidanceReadError> {
    read_project_guidance_at_with_reader_and_hook(
        root,
        targets,
        |file| {
            let mut contents = Vec::with_capacity(MAX_ROOT_AGENTS_BYTES + 1);
            file.take((MAX_ROOT_AGENTS_BYTES + 1) as u64)
                .read_to_end(&mut contents)?;
            Ok(contents)
        },
        |_| Ok(()),
    )
}

#[cfg(test)]
pub(super) fn read_project_guidance_at_with_reader(
    root: &WorkspaceRoot,
    targets: &[PathBuf],
    read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    read_project_guidance_at_with_reader_and_hook(root, targets, read, |_| Ok(()))
}

pub(super) fn read_project_guidance_at_with_reader_and_hook(
    root: &WorkspaceRoot,
    targets: &[PathBuf],
    mut read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
    mut after_directory_read: impl FnMut(&[OsString]) -> io::Result<()>,
) -> Result<Option<String>, RootGuidanceReadError> {
    // Every planned edit target must be safely inspected or fail this preflight. Omitting an
    // unsafe target could let another operation in the same plan mutate without full guidance.
    let mut normalized_targets = Vec::with_capacity(targets.len());
    for target in targets {
        normalized_targets.push(normalize_guidance_target(target)?);
    }

    let mut safe_targets = Vec::new();
    for (parent, name) in normalized_targets {
        let parent_fd = match open_guidance_directory(root, &parent) {
            Ok(Some(parent_fd)) => parent_fd,
            Ok(None) => {
                safe_targets.push((parent, name));
                continue;
            }
            Err(error) => return Err(error),
        };
        match stat_at(&parent_fd, &name) {
            Ok(status) if is_symlink(&status) => {
                return Err(RootGuidanceReadError::Symlink);
            }
            Ok(status) if is_regular_file(&status) || is_directory(&status) => {
                safe_targets.push((parent, name));
            }
            Ok(_) => return Err(RootGuidanceReadError::InvalidTargetPath),
            Err(error) if is_not_found(&error) => {
                safe_targets.push((parent, name));
            }
            Err(_) => return Err(RootGuidanceReadError::ReadFailed),
        }
    }

    let mut directories = BTreeSet::from([Vec::<OsString>::new()]);
    for (parent, _) in &safe_targets {
        for depth in 1..=parent.len() {
            directories.insert(parent[..depth].to_vec());
        }
    }

    let mut guidance = String::new();
    let mut found_guidance = false;
    let mut directory_snapshots = Vec::new();
    for path in directories {
        let Some(directory) = open_guidance_directory(root, &path)? else {
            continue;
        };
        let directory_snapshot =
            fstat_directory(&directory).map_err(|_| RootGuidanceReadError::ReadFailed)?;
        let agents_snapshot = guidance_file_snapshot(&directory)?;
        if let Some(contents) = read_agents_md_at(&directory, &mut read)? {
            found_guidance = true;
            if !path.is_empty() {
                let directory_path = path.iter().fold(PathBuf::new(), |mut path, component| {
                    path.push(component);
                    path
                });
                guidance.push_str(NESTED_GUIDANCE_MARKER);
                guidance.push_str(&directory_path.join("AGENTS.md").display().to_string());
                guidance.push_str(" ---\n");
            }
            guidance.push_str(&contents);
            if guidance.len() > MAX_ROOT_AGENTS_BYTES {
                return Err(RootGuidanceReadError::TooLarge);
            }
        }
        after_directory_read(&path).map_err(|_| RootGuidanceReadError::ReadFailed)?;
        verify_guidance_directory(root, &path, &directory_snapshot, agents_snapshot.as_ref())?;
        directory_snapshots.push((path, directory_snapshot, agents_snapshot));
    }

    // Recheck every relevant directory after the full guidance walk. This catches a guidance
    // entry added to an earlier directory while later target ancestors were being inspected.
    for (path, directory_snapshot, agents_snapshot) in directory_snapshots {
        verify_guidance_directory(root, &path, &directory_snapshot, agents_snapshot.as_ref())?;
    }

    if found_guidance {
        Ok(Some(guidance))
    } else {
        Ok(None)
    }
}

fn normalize_guidance_target(
    target: &Path,
) -> Result<(Vec<OsString>, OsString), RootGuidanceReadError> {
    if target.is_absolute() {
        return Err(RootGuidanceReadError::InvalidTargetPath);
    }
    let mut components = Vec::new();
    for component in target.components() {
        match component {
            Component::Normal(name) if name.to_str().is_some() => {
                components.push(name.to_os_string());
            }
            Component::CurDir => {}
            _ => return Err(RootGuidanceReadError::InvalidTargetPath),
        }
    }
    let name = components
        .pop()
        .ok_or(RootGuidanceReadError::InvalidTargetPath)?;
    Ok((components, name))
}

fn open_guidance_directory(
    root: &WorkspaceRoot,
    components: &[OsString],
) -> Result<Option<DirHandle>, RootGuidanceReadError> {
    let mut directory =
        duplicate_directory(&root.descriptor).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    for component in components {
        let status = match stat_at(&directory, component) {
            Ok(status) => status,
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(_) => return Err(RootGuidanceReadError::ReadFailed),
        };
        if is_symlink(&status) {
            return Err(RootGuidanceReadError::Symlink);
        }
        if !is_directory(&status) {
            return Err(RootGuidanceReadError::InvalidTargetPath);
        }
        let opened =
            open_at(&directory, component, OpenRequest::ReadDirectory).map_err(|error| {
                if is_symlink_loop(&error) {
                    RootGuidanceReadError::ChangedDuringRead
                } else {
                    RootGuidanceReadError::ReadFailed
                }
            })?;
        let opened = DirHandle::from_file(opened);
        let opened_status =
            fstat_directory(&opened).map_err(|_| RootGuidanceReadError::ReadFailed)?;
        if !same_file_snapshot(&status, &opened_status) {
            return Err(RootGuidanceReadError::ChangedDuringRead);
        }
        directory = opened;
    }
    Ok(Some(directory))
}

fn verify_guidance_directory(
    root: &WorkspaceRoot,
    components: &[OsString],
    original_directory_status: &FileStatus,
    original_agents_status: Option<&FileStatus>,
) -> Result<(), RootGuidanceReadError> {
    let Some(current) = open_guidance_directory(root, components)? else {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    };
    let current_status =
        fstat_directory(&current).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let current_agents_status = guidance_file_snapshot(&current)?;
    let agents_match = match (original_agents_status, current_agents_status.as_ref()) {
        (None, None) => true,
        (Some(original), Some(current)) => same_file_snapshot(original, current),
        _ => false,
    };
    if !same_file_snapshot(original_directory_status, &current_status) || !agents_match {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    }
    Ok(())
}

fn guidance_file_snapshot(parent: &DirHandle) -> Result<Option<FileStatus>, RootGuidanceReadError> {
    match stat_at(parent, OsStr::new("AGENTS.md")) {
        Ok(status) => Ok(Some(status)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(_) => Err(RootGuidanceReadError::ReadFailed),
    }
}

fn read_agents_md_at(
    parent: &DirHandle,
    mut read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    let name = OsStr::new("AGENTS.md");
    let original_status = match stat_at(parent, name) {
        Ok(status) => status,
        Err(error) if is_not_found(&error) => {
            return match stat_at(parent, name) {
                Err(error) if is_not_found(&error) => Ok(None),
                Ok(_) => Err(RootGuidanceReadError::ChangedDuringRead),
                Err(_) => Err(RootGuidanceReadError::ReadFailed),
            }
        }
        Err(_) => return Err(RootGuidanceReadError::ReadFailed),
    };
    if is_symlink(&original_status) {
        return Err(RootGuidanceReadError::Symlink);
    }
    if !is_regular_file(&original_status) {
        return Err(RootGuidanceReadError::NotRegularFile);
    }
    if original_status.size > MAX_ROOT_AGENTS_BYTES as u64 {
        return Err(RootGuidanceReadError::TooLarge);
    }

    let mut file = open_at(parent, name, OpenRequest::ReadFile).map_err(|error| {
        if is_symlink_loop(&error) {
            RootGuidanceReadError::ChangedDuringRead
        } else {
            RootGuidanceReadError::ReadFailed
        }
    })?;
    let opened_status = fstat_file(&file).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    if !same_file_snapshot(&original_status, &opened_status) {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    }

    let contents = read(&mut file).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let final_descriptor_status =
        fstat_file(&file).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let final_path_status = stat_at(parent, name).map_err(|error| {
        if is_not_found(&error) {
            RootGuidanceReadError::ChangedDuringRead
        } else {
            RootGuidanceReadError::ReadFailed
        }
    })?;
    if !same_file_snapshot(&original_status, &final_descriptor_status)
        || !same_file_snapshot(&original_status, &final_path_status)
    {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    }
    if contents.len() > MAX_ROOT_AGENTS_BYTES {
        return Err(RootGuidanceReadError::TooLarge);
    }

    String::from_utf8(contents)
        .map(Some)
        .map_err(|_| RootGuidanceReadError::InvalidUtf8)
}

pub(super) fn read_open_text_file(mut file: File) -> Result<String, CliError> {
    let metadata = file
        .metadata()
        .map_err(|_| CliError::new("unable to read file"))?;
    if !metadata.is_file() {
        return Err(CliError::new("not a regular file"));
    }
    if metadata.len() > MAX_FILE_SIZE as u64 {
        return Err(CliError::new("file exceeds 1 MiB"));
    }

    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_FILE_SIZE as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CliError::new("unable to read file"))?;
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("file exceeds 1 MiB"));
    }
    String::from_utf8(contents).map_err(|_| CliError::new("file is not valid UTF-8"))
}
