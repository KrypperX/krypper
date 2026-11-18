use std::collections::{BTreeSet, VecDeque};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

mod patch;
mod search;
pub(crate) use patch::MAX_PATCH_BYTES;
pub(crate) use search::{search_text_files_at, search_tree_text_files_at};

const MAX_FILE_SIZE: usize = 1024 * 1024;
const MAX_ROOT_AGENTS_BYTES: usize = 16 * 1024;
const MAX_SYMLINKS: usize = 40;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 1024;
const NESTED_GUIDANCE_MARKER: &str = "\n\n--- AGENTS.md: ";

#[derive(Debug)]
pub(crate) struct CliError(String);

impl CliError {
    fn new(message: &'static str) -> Self {
        Self(message.to_owned())
    }

    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct WorkspaceRoot {
    descriptor: OwnedFd,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootGuidanceReadError {
    InvalidTargetPath,
    Symlink,
    NotRegularFile,
    TooLarge,
    InvalidUtf8,
    ChangedDuringRead,
    ReadFailed,
}

impl fmt::Display for RootGuidanceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTargetPath => {
                formatter.write_str("edit target path is invalid or outside the task workspace")
            }
            Self::Symlink => formatter.write_str("AGENTS.md guidance or edit target is a symlink"),
            Self::NotRegularFile => formatter.write_str("AGENTS.md guidance is not a regular file"),
            Self::TooLarge => formatter.write_str("AGENTS.md guidance exceeds 16 KiB"),
            Self::InvalidUtf8 => formatter.write_str("AGENTS.md guidance is not valid UTF-8"),
            Self::ChangedDuringRead => {
                formatter.write_str("AGENTS.md guidance changed during the safe read")
            }
            Self::ReadFailed => formatter.write_str("AGENTS.md guidance could not be read"),
        }
    }
}

impl std::error::Error for RootGuidanceReadError {}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PatchEvidence {
    pub(crate) bytes_written: usize,
    pub(crate) before_diff: String,
    pub(crate) after_diff: String,
}

struct OriginalFileState<'a> {
    status: &'a libc::stat,
    contents: &'a [u8],
}

impl WorkspaceRoot {
    pub(crate) fn process_descriptor(&self) -> io::Result<OwnedFd> {
        self.descriptor.try_clone()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let metadata = fstat(self.descriptor.as_raw_fd())?;
        Ok((metadata.st_dev as u64, metadata.st_ino as u64))
    }

    pub(crate) fn duplicate(&self) -> io::Result<Self> {
        Ok(Self {
            descriptor: self.descriptor.try_clone()?,
            path: self.path.clone(),
        })
    }
}

pub(crate) fn capture_workspace_root() -> Result<WorkspaceRoot, String> {
    let descriptor = open_current_directory()
        .map_err(|error| format!("unable to open current directory: {error}"))?;
    let path = fs::canonicalize(".")
        .map_err(|error| format!("unable to resolve current directory: {error}"))?;
    verify_root_descriptor(descriptor.as_raw_fd(), &path)
        .map_err(|error| format!("unable to verify current directory: {}", error.message()))?;
    Ok(WorkspaceRoot { descriptor, path })
}

pub(crate) fn capture_workspace_root_at(path: &Path) -> Result<WorkspaceRoot, String> {
    let path = fs::canonicalize(path)
        .map_err(|error| format!("unable to resolve workspace path: {error}"))?;
    let descriptor = open_directory_path(&path)
        .map_err(|error| format!("unable to open workspace path: {error}"))?;
    verify_root_descriptor(descriptor.as_raw_fd(), &path)
        .map_err(|error| format!("unable to verify workspace path: {}", error.message()))?;
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
    read_agents_md_at(root.descriptor.as_raw_fd(), |file| {
        let mut contents = Vec::with_capacity(MAX_ROOT_AGENTS_BYTES + 1);
        file.take((MAX_ROOT_AGENTS_BYTES + 1) as u64)
            .read_to_end(&mut contents)?;
        Ok(contents)
    })
}

#[cfg(test)]
fn read_root_agents_md_at_with_reader(
    root: &WorkspaceRoot,
    read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    read_agents_md_at(root.descriptor.as_raw_fd(), read)
}

pub(crate) fn read_project_guidance_at(
    root: &WorkspaceRoot,
    targets: &[PathBuf],
) -> Result<Option<String>, RootGuidanceReadError> {
    read_project_guidance_at_with_reader(root, targets, |file| {
        let mut contents = Vec::with_capacity(MAX_ROOT_AGENTS_BYTES + 1);
        file.take((MAX_ROOT_AGENTS_BYTES + 1) as u64)
            .read_to_end(&mut contents)?;
        Ok(contents)
    })
}

fn read_project_guidance_at_with_reader(
    root: &WorkspaceRoot,
    targets: &[PathBuf],
    read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    read_project_guidance_at_with_reader_and_hook(root, targets, read, |_| Ok(()))
}

fn read_project_guidance_at_with_reader_and_hook(
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
        match stat_at(parent_fd.as_raw_fd(), &name) {
            Ok(status) if is_symlink(&status) => {
                return Err(RootGuidanceReadError::Symlink);
            }
            Ok(status) if is_regular_file(&status) || is_directory(&status) => {
                safe_targets.push((parent, name));
            }
            Ok(_) => return Err(RootGuidanceReadError::InvalidTargetPath),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
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
            fstat(directory.as_raw_fd()).map_err(|_| RootGuidanceReadError::ReadFailed)?;
        let agents_snapshot = guidance_file_snapshot(directory.as_raw_fd())?;
        if let Some(contents) = read_agents_md_at(directory.as_raw_fd(), &mut read)? {
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
) -> Result<Option<OwnedFd>, RootGuidanceReadError> {
    let mut directory = root
        .descriptor
        .try_clone()
        .map_err(|_| RootGuidanceReadError::ReadFailed)?;
    for component in components {
        let status = match stat_at(directory.as_raw_fd(), component) {
            Ok(status) => status,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(_) => return Err(RootGuidanceReadError::ReadFailed),
        };
        if is_symlink(&status) {
            return Err(RootGuidanceReadError::Symlink);
        }
        if !is_directory(&status) {
            return Err(RootGuidanceReadError::InvalidTargetPath);
        }
        let opened = open_at(
            directory.as_raw_fd(),
            component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                RootGuidanceReadError::ChangedDuringRead
            } else {
                RootGuidanceReadError::ReadFailed
            }
        })?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| RootGuidanceReadError::ReadFailed)?;
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
    original_directory_status: &libc::stat,
    original_agents_status: Option<&libc::stat>,
) -> Result<(), RootGuidanceReadError> {
    let Some(current) = open_guidance_directory(root, components)? else {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    };
    let current_status =
        fstat(current.as_raw_fd()).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let current_agents_status = guidance_file_snapshot(current.as_raw_fd())?;
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

fn guidance_file_snapshot(parent_fd: RawFd) -> Result<Option<libc::stat>, RootGuidanceReadError> {
    match stat_at(parent_fd, OsStr::new("AGENTS.md")) {
        Ok(status) => Ok(Some(status)),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(_) => Err(RootGuidanceReadError::ReadFailed),
    }
}

fn read_agents_md_at(
    parent_fd: RawFd,
    mut read: impl FnMut(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    let name = OsStr::new("AGENTS.md");
    let original_status = match stat_at(parent_fd, name) {
        Ok(status) => status,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            return match stat_at(parent_fd, name) {
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
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
    if original_status.st_size < 0 || original_status.st_size > MAX_ROOT_AGENTS_BYTES as i64 {
        return Err(RootGuidanceReadError::TooLarge);
    }

    let descriptor = open_at(
        parent_fd,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            RootGuidanceReadError::ChangedDuringRead
        } else {
            RootGuidanceReadError::ReadFailed
        }
    })?;
    let mut file = File::from(descriptor);
    let opened_status = fstat(file.as_raw_fd()).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    if !same_file_snapshot(&original_status, &opened_status) {
        return Err(RootGuidanceReadError::ChangedDuringRead);
    }

    let contents = read(&mut file).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let final_descriptor_status =
        fstat(file.as_raw_fd()).map_err(|_| RootGuidanceReadError::ReadFailed)?;
    let final_path_status = stat_at(parent_fd, name).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOENT) {
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

fn same_file_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    same_inode(left, right)
        && left.st_mode == right.st_mode
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn read_open_text_file(mut file: File) -> Result<String, CliError> {
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

pub(crate) fn list_directory(path: &OsStr) -> Result<Vec<String>, CliError> {
    list_directory_entries(path).map(|names| {
        names
            .into_iter()
            .map(|name| quote_json(&name.sort_key))
            .collect()
    })
}

pub(crate) fn list_task_directory_names_at(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<Vec<OsString>, CliError> {
    list_open_directory_with_limit(
        open_workspace_relative(root, path, true)?,
        Some(MAX_TASK_DIRECTORY_ENTRIES),
    )
    .map(|names| names.into_iter().map(|name| name.name).collect())
}

fn list_directory_entries(path: &OsStr) -> Result<Vec<ListedName>, CliError> {
    list_open_directory(open_cwd_relative(path, true)?)
}

fn list_open_directory(directory: File) -> Result<Vec<ListedName>, CliError> {
    list_open_directory_with_limit(directory, None)
}

fn list_open_directory_with_limit(
    directory: File,
    maximum_entries: Option<usize>,
) -> Result<Vec<ListedName>, CliError> {
    list_open_directory_with_limit_and_error(
        directory,
        maximum_entries,
        "directory exceeds 1,024 entries",
    )
}

fn list_open_directory_with_limit_and_error(
    directory: File,
    maximum_entries: Option<usize>,
    limit_error: &'static str,
) -> Result<Vec<ListedName>, CliError> {
    let duplicate = duplicate_fd(directory.as_raw_fd())
        .map_err(|_| CliError::new("unable to list directory"))?;
    let raw = duplicate.into_raw_fd();
    // SAFETY: `raw` is a valid, independently owned directory descriptor; `fdopendir` takes ownership on success.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        // SAFETY: `fdopendir` did not take ownership when it returned null.
        unsafe { libc::close(raw) };
        return Err(CliError::new("unable to list directory"));
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();

    loop {
        // SAFETY: `__errno_location` returns this thread's errno slot on Linux.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `stream` owns a live DIR pointer until its Drop implementation closes it.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: Reading the thread-local errno slot is valid after a failed `readdir` call.
            if unsafe { *libc::__errno_location() } != 0 {
                return Err(CliError::new("unable to list directory"));
            }
            break;
        }
        // SAFETY: `d_name` is NUL-terminated by `readdir`, and is copied before the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            if maximum_entries.is_some_and(|maximum| names.len() == maximum) {
                return Err(CliError::new(limit_error));
            }
            names.push(ListedName::new(name.to_vec()));
        }
    }

    names.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(names)
}

pub(crate) fn create_text_file(path: &OsStr, text: &str) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    let root =
        open_current_directory().map_err(|_| CliError::new("unable to open current directory"))?;
    create_text_file_at_with_writer(root.as_raw_fd(), path, contents, write_to_fd)
}

pub(crate) fn create_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    text: &str,
) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    create_text_file_at_with_writer(root.descriptor.as_raw_fd(), path, contents, write_to_fd)
}

pub(crate) fn create_directory_at(root: &WorkspaceRoot, path: &OsStr) -> Result<(), CliError> {
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid directory path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid directory path"));
    }

    let (parent, name) = open_workspace_parent(root, path)?;
    match stat_at(parent.as_raw_fd(), &name) {
        Ok(status) if is_symlink(&status) => {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        Ok(_) => return Err(CliError::new("path already exists")),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(_) => return Err(CliError::new("invalid directory path")),
    }

    let name = c_string(&name).map_err(|_| CliError::new("invalid directory path"))?;
    // SAFETY: `parent` is an open directory descriptor and `name` is a NUL-terminated single component.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } == 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) {
        Err(CliError::new("path already exists"))
    } else {
        Err(CliError::new("unable to create directory"))
    }
}

pub(crate) fn replace_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
) -> Result<usize, CliError> {
    replace_text_file_at_with_writer(root, path, match_text, replacement_text, write_to_fd)
}

fn replace_text_file_at_with_writer<F>(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
    mut writer: F,
) -> Result<usize, CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid file path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid file path"));
    }
    if match_text.is_empty() {
        return Err(CliError::new("match text must not be empty"));
    }
    if match_text.len() > MAX_FILE_SIZE {
        return Err(CliError::new("match text exceeds 1 MiB"));
    }
    if replacement_text.len() > MAX_FILE_SIZE {
        return Err(CliError::new("replacement text exceeds 1 MiB"));
    }

    let (parent, name) = open_workspace_parent(root, path)?;
    let parent_fd = parent.as_raw_fd();
    let original_status = stat_at(parent_fd, &name).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOENT) {
            CliError::new("file not found")
        } else {
            CliError::new("invalid path")
        }
    })?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }

    let original_file = open_at(
        parent_fd,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to open file"))?;
    let opened_status =
        fstat(original_file.as_raw_fd()).map_err(|_| CliError::new("unable to open file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while replacing it"));
    }

    let contents = read_open_text_file(File::from(original_file))?;
    let range = unique_match_range(&contents, match_text)?;
    let output_length = contents
        .len()
        .checked_sub(match_text.len())
        .and_then(|length| length.checked_add(replacement_text.len()))
        .filter(|length| *length <= MAX_FILE_SIZE)
        .ok_or_else(|| CliError::new("replacement would exceed 1 MiB"))?;
    let mut replacement = String::with_capacity(output_length);
    replacement.push_str(&contents[..range.start]);
    replacement.push_str(replacement_text);
    replacement.push_str(&contents[range.end..]);

    replace_file_contents_atomically(
        parent_fd,
        &name,
        OriginalFileState {
            status: &original_status,
            contents: contents.as_bytes(),
        },
        replacement.as_bytes(),
        &mut writer,
        &mut rename_file_at,
        &mut read_file_contents_at_for_verification,
    )?;
    Ok(output_length)
}

pub(crate) fn apply_patch_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    patch_text: &str,
) -> Result<PatchEvidence, CliError> {
    apply_patch_file_at_with_ops(
        root,
        path,
        patch_text,
        write_to_fd,
        rename_file_at,
        read_file_contents_at_for_verification,
    )
}

fn apply_patch_file_at_with_ops<W, C, R>(
    root: &WorkspaceRoot,
    path: &OsStr,
    patch_text: &str,
    mut writer: W,
    mut committer: C,
    mut readback: R,
) -> Result<PatchEvidence, CliError>
where
    W: FnMut(RawFd, &[u8]) -> io::Result<usize>,
    C: FnMut(RawFd, &OsStr, &OsStr) -> io::Result<()>,
    R: FnMut(RawFd, &OsStr) -> Result<Vec<u8>, CliError>,
{
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid file path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if Path::new(path)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CliError::new("traversal paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid file path"));
    }
    if patch_text.len() > MAX_PATCH_BYTES {
        return Err(CliError::new("patch exceeds 32 KiB"));
    }

    let path_text = path
        .to_str()
        .ok_or_else(|| CliError::new("file path is not valid UTF-8"))?;
    let (parent, name) = open_workspace_parent(root, path)?;
    let parent_fd = parent.as_raw_fd();
    let original_status = stat_at(parent_fd, &name).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOENT) {
            CliError::new("file not found")
        } else {
            CliError::new("invalid path")
        }
    })?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }

    let original_file = open_at(
        parent_fd,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to open file"))?;
    let opened_status =
        fstat(original_file.as_raw_fd()).map_err(|_| CliError::new("unable to open file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while patching it"));
    }
    let original = read_open_text_file(File::from(original_file))?;
    let result =
        patch::apply_unified_diff(path_text, &original, patch_text).map_err(CliError::new)?;
    if result.contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("patched file exceeds 1 MiB"));
    }

    replace_file_contents_atomically(
        parent_fd,
        &name,
        OriginalFileState {
            status: &original_status,
            contents: original.as_bytes(),
        },
        result.contents.as_bytes(),
        &mut writer,
        &mut committer,
        &mut readback,
    )?;

    Ok(PatchEvidence {
        bytes_written: result.contents.len(),
        before_diff: result.before_diff,
        after_diff: result.after_diff,
    })
}

fn open_workspace_parent(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<(OwnedFd, OsString), CliError> {
    let mut components = split_components(path.as_bytes());
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    if matches!(name.as_bytes(), b"" | b"." | b"..") {
        return Err(CliError::new("invalid file path"));
    }

    let mut directories = vec![duplicate_fd(root.descriptor.as_raw_fd())
        .map_err(|_| CliError::new("unable to open current directory"))?];
    for component in components {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("parent directory not found")
            } else {
                CliError::new("invalid parent directory")
            }
        })?;
        if is_symlink(&status) {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        if !is_directory(&status) {
            return Err(CliError::new("parent is not a directory"));
        }

        let opened = open_at(
            parent_fd,
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("invalid parent directory"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while replacing file"));
        }
        directories.push(opened);
    }

    let parent = directories
        .pop()
        .expect("invariant: the root descriptor remains on the stack");
    Ok((parent, name))
}

fn unique_match_range(
    contents: &str,
    match_text: &str,
) -> Result<std::ops::Range<usize>, CliError> {
    let start = contents
        .find(match_text)
        .ok_or_else(|| CliError::new("match text was not found"))?;
    let next_character = contents[start..].chars().next().map_or(0, char::len_utf8);
    if contents[start + next_character..].contains(match_text) {
        return Err(CliError::new("match text is ambiguous"));
    }
    Ok(start..start + match_text.len())
}

fn replace_file_contents_atomically<W, C, R>(
    parent_fd: RawFd,
    name: &OsStr,
    original: OriginalFileState<'_>,
    contents: &[u8],
    writer: &mut W,
    committer: &mut C,
    readback: &mut R,
) -> Result<(), CliError>
where
    W: FnMut(RawFd, &[u8]) -> io::Result<usize>,
    C: FnMut(RawFd, &OsStr, &OsStr) -> io::Result<()>,
    R: FnMut(RawFd, &OsStr) -> Result<Vec<u8>, CliError>,
{
    let (temporary_name, temporary_file) = (0..3)
        .find_map(|_| {
            let name = OsString::from(format!(".krypper-replace-{}", uuid::Uuid::new_v4()));
            match open_at(
                parent_fd,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            ) {
                Ok(file) => Some(Ok((name, file))),
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => None,
                Err(_) => Some(Err(CliError::new("unable to stage file replacement"))),
            }
        })
        .transpose()?
        .ok_or_else(|| CliError::new("unable to stage file replacement"))?;

    let identity = fstat(temporary_file.as_raw_fd())
        .ok()
        .or_else(|| stat_at(parent_fd, &temporary_name).ok());
    let raw_fd = temporary_file.into_raw_fd();
    let mut failed = identity.is_none();
    if !failed && write_all_to_fd(raw_fd, contents, writer).is_err() {
        failed = true;
    }
    if !failed
        // SAFETY: `raw_fd` is the open temporary file descriptor transferred from `OwnedFd`.
        && unsafe { libc::fchmod(raw_fd, original.status.st_mode & 0o777) } != 0
    {
        failed = true;
    }
    if !failed
        // SAFETY: `raw_fd` remains open and refers to the complete staged file.
        && unsafe { libc::fsync(raw_fd) } != 0
    {
        failed = true;
    }
    // SAFETY: `raw_fd` was transferred from `OwnedFd` above and is closed exactly once here.
    if unsafe { libc::close(raw_fd) } != 0 {
        failed = true;
    }
    if failed {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("unable to stage file replacement"));
    }

    let current_status = stat_at(parent_fd, name).map_err(|_| {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        CliError::new("file changed while replacing it")
    })?;
    if !same_inode(original.status, &current_status) || !is_regular_file(&current_status) {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("file changed while replacing it"));
    }

    let current_contents = match read_file_contents_at_for_verification(parent_fd, name) {
        Ok(contents) => contents,
        Err(_) => {
            unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
            return Err(CliError::new("unable to verify original file"));
        }
    };
    if current_contents != original.contents {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("file changed while replacing it"));
    }

    if committer(parent_fd, &temporary_name, name).is_err() {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("unable to replace file"));
    }
    let stored_contents =
        readback(parent_fd, name).map_err(|_| CliError::new("unable to verify stored file"))?;
    if stored_contents != contents {
        return Err(CliError::new(
            "stored file did not match the expected contents",
        ));
    }
    Ok(())
}

fn rename_file_at(parent_fd: RawFd, temporary_name: &OsStr, name: &OsStr) -> io::Result<()> {
    let temporary_name = c_string(temporary_name)?;
    let name = c_string(name)?;
    // SAFETY: both names are NUL-terminated single path components and `parent_fd` is an open directory.
    if unsafe { libc::renameat(parent_fd, temporary_name.as_ptr(), parent_fd, name.as_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `parent_fd` is an open directory containing the committed rename.
    if unsafe { libc::fsync(parent_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_file_contents_at_for_verification(
    parent_fd: RawFd,
    name: &OsStr,
) -> Result<Vec<u8>, CliError> {
    let original_status =
        stat_at(parent_fd, name).map_err(|_| CliError::new("unable to verify stored file"))?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }
    if original_status.st_size < 0 || original_status.st_size as usize > MAX_FILE_SIZE {
        return Err(CliError::new("file exceeds 1 MiB"));
    }

    let file = open_at(
        parent_fd,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to verify stored file"))?;
    let opened_status =
        fstat(file.as_raw_fd()).map_err(|_| CliError::new("unable to verify stored file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while verifying it"));
    }

    let mut contents = Vec::with_capacity(original_status.st_size as usize);
    File::from(file)
        .take(MAX_FILE_SIZE as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CliError::new("unable to verify stored file"))?;
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("file exceeds 1 MiB"));
    }
    let current_status =
        stat_at(parent_fd, name).map_err(|_| CliError::new("file changed while verifying it"))?;
    if !same_inode(&opened_status, &current_status) || !is_regular_file(&current_status) {
        return Err(CliError::new("file changed while verifying it"));
    }
    Ok(contents)
}

fn unlink_created_file(parent_fd: RawFd, name: &OsStr, identity: Option<&libc::stat>) {
    let Some(identity) = identity else {
        return;
    };
    let Ok(current) = stat_at(parent_fd, name) else {
        return;
    };
    let Ok(name) = c_string(name) else {
        return;
    };
    if same_inode(identity, &current) {
        // SAFETY: `parent_fd` is open, `name` is a single component, and the inode still matches the temporary file created above.
        unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
    }
}

fn write_all_to_fd<F>(fd: RawFd, contents: &[u8], writer: &mut F) -> io::Result<()>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let mut offset = 0;
    while offset < contents.len() {
        match writer(fd, &contents[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "zero-byte write")),
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => return Err(io::Error::other("writer reported an invalid byte count")),
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_create_text_file<'a>(path: &OsStr, text: &'a str) -> Result<&'a [u8], CliError> {
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid file path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid file path"));
    }

    let contents = text.as_bytes();
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("text exceeds 1 MiB"));
    }
    Ok(contents)
}

fn open_cwd_relative(path: &OsStr, directory: bool) -> Result<File, CliError> {
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }

    let root = open_current_directory().map_err(|_| CliError::new("unable to read file"))?;
    let root_path = fs::canonicalize(".").map_err(|_| CliError::new("invalid path"))?;
    open_root_relative(root, &root_path, path, directory)
}

fn open_workspace_relative(
    root: &WorkspaceRoot,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    open_workspace_relative_with_resolved_path(root, path, directory).map(|(file, _)| file)
}

pub(crate) fn open_workspace_relative_with_resolved_path(
    root: &WorkspaceRoot,
    path: &OsStr,
    directory: bool,
) -> Result<(File, PathBuf), CliError> {
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    let descriptor = duplicate_fd(root.descriptor.as_raw_fd())
        .map_err(|_| CliError::new("unable to read file"))?;
    open_root_relative_with_resolved_path(descriptor, &root.path, path, directory)
}

fn open_root_relative(
    root: OwnedFd,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    open_root_relative_with_resolved_path(root, root_path, path, directory).map(|(file, _)| file)
}

fn open_root_relative_with_resolved_path(
    root: OwnedFd,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<(File, PathBuf), CliError> {
    let mut directories = vec![root];
    let mut resolved_components = Vec::<OsString>::new();
    let mut pending = split_components(path.as_bytes());
    let mut symlinks = 0;

    while let Some(component) = pending.pop_front() {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                resolved_components.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("file not found")
            } else {
                CliError::new("invalid path")
            }
        })?;

        if is_symlink(&status) {
            symlinks += 1;
            if symlinks > MAX_SYMLINKS {
                return Err(CliError::new("invalid path"));
            }
            let target =
                read_link_at(parent_fd, &component).map_err(|_| CliError::new("invalid path"))?;
            if target.first() == Some(&b'/') {
                let target_path = PathBuf::from(OsString::from_vec(target));
                let target_path = normalize_absolute(&target_path);
                if !target_path.starts_with(root_path) {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                verify_root_descriptor(directories[0].as_raw_fd(), root_path)?;
                let relative = target_path
                    .strip_prefix(root_path)
                    .map_err(|_| CliError::new("invalid path"))?;
                directories.truncate(1);
                resolved_components.clear();
                prepend_components(&mut pending, relative.as_os_str().as_bytes());
            } else {
                prepend_components(&mut pending, &target);
            }
            continue;
        }

        let final_component = !pending
            .iter()
            .any(|part| !matches!(part.as_bytes(), b"" | b"."));
        if final_component {
            if directory && !is_directory(&status) {
                return Err(CliError::new("not a directory"));
            }
            if !directory && !is_regular_file(&status) {
                return Err(CliError::new("not a regular file"));
            }
        }

        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        if !final_component || directory {
            flags |= libc::O_DIRECTORY;
        }
        let opened = open_at(parent_fd, &component, flags, 0)
            .map_err(|_| CliError::new("unable to read file"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("unable to read file"))?;
        if !same_inode(&status, &opened_status)
            || (final_component && directory && !is_directory(&opened_status))
            || (final_component && !directory && !is_regular_file(&opened_status))
        {
            return Err(CliError::new("unable to read file"));
        }
        resolved_components.push(component.clone());
        if final_component {
            return Ok((
                File::from(opened),
                resolved_components.into_iter().collect(),
            ));
        }
        directories.push(opened);
    }

    let descriptor = duplicate_fd(directories[0].as_raw_fd())
        .map_err(|_| CliError::new("unable to read file"))?;
    Ok((
        File::from(descriptor),
        resolved_components.into_iter().collect(),
    ))
}

fn create_text_file_at_with_writer<F>(
    root_fd: RawFd,
    path: &OsStr,
    contents: &[u8],
    mut writer: F,
) -> Result<(), CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let mut components = split_components(path.as_bytes());
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    if matches!(name.as_bytes(), b"" | b"." | b"..") {
        return Err(CliError::new("invalid file path"));
    }

    let mut directories =
        vec![duplicate_fd(root_fd).map_err(|_| CliError::new("unable to open current directory"))?];
    for component in components {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("parent directory not found")
            } else {
                CliError::new("invalid parent directory")
            }
        })?;
        if is_symlink(&status) {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        if !is_directory(&status) {
            return Err(CliError::new("parent is not a directory"));
        }

        let opened = open_at(
            parent_fd,
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("invalid parent directory"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while creating file"));
        }
        directories.push(opened);
    }

    let parent_fd = directories
        .last()
        .expect("invariant: the root descriptor remains on the stack")
        .as_raw_fd();
    let file = open_at(
        parent_fd,
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o666,
    )
    .map_err(|error| match error.raw_os_error() {
        Some(libc::EEXIST) => CliError::new("file already exists"),
        Some(libc::ENOENT) => CliError::new("parent directory not found"),
        _ => CliError::new("unable to create file"),
    })?;
    finish_new_file(file, parent_fd, &name, contents, &mut writer)
}

fn finish_new_file<F>(
    file: OwnedFd,
    parent_fd: RawFd,
    name: &OsStr,
    contents: &[u8],
    writer: &mut F,
) -> Result<(), CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let raw_fd = file.into_raw_fd();
    let (identity, mut failure) = match fstat(raw_fd) {
        Ok(identity) => (Some(identity), false),
        Err(_) => (None, true),
    };
    let mut offset = 0;

    while !failure && offset < contents.len() {
        match writer(raw_fd, &contents[offset..]) {
            Ok(0) => failure = true,
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => failure = true,
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => failure = true,
        }
    }

    // SAFETY: `raw_fd` is the descriptor transferred from `OwnedFd` above and is closed exactly once here.
    if unsafe { libc::close(raw_fd) } != 0 {
        failure = true;
    }
    if !failure {
        return Ok(());
    }

    if let (Some(created), Ok(cleanup_name)) = (identity, c_string(name)) {
        if let Ok(current) = stat_at(parent_fd, name) {
            if same_inode(&created, &current) {
                // SAFETY: `parent_fd` is open and `cleanup_name` names one component; this removes only the inode created above.
                if unsafe { libc::unlinkat(parent_fd, cleanup_name.as_ptr(), 0) } != 0 {
                    return Err(CliError::new("unable to write file"));
                }
            }
        }
    }
    Err(CliError::new("unable to write file"))
}

fn write_to_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` points to a readable buffer for `bytes.len()` bytes and `fd` is open for writing.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

fn open_current_directory() -> io::Result<OwnedFd> {
    let path = CString::new(".").expect("invariant: a dot path contains no NUL byte");
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd_or_error(descriptor)
}

fn open_directory_path(path: &Path) -> io::Result<OwnedFd> {
    let path = c_string(path.as_os_str())?;
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd_or_error(descriptor)
}

fn open_at(
    parent_fd: RawFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    // SAFETY: `parent_fd` is an open directory descriptor and `name` is a NUL-terminated component.
    let descriptor = unsafe { libc::openat(parent_fd, name.as_ptr(), flags, mode) };
    owned_fd_or_error(descriptor)
}

fn owned_fd_or_error(descriptor: RawFd) -> io::Result<OwnedFd> {
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative descriptor returned by `open`/`openat` is newly owned by this function.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn duplicate_fd(descriptor: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `descriptor` is live for the duration of this call.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
    owned_fd_or_error(duplicate)
}

fn stat_at(parent_fd: RawFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_string(name)?;
    // SAFETY: the output points to initialized writable storage, and the parent/name arguments remain valid.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `parent_fd` is open, `name` is NUL-terminated, and `status` is writable.
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn fstat(descriptor: RawFd) -> io::Result<libc::stat> {
    // SAFETY: the output points to initialized writable storage and `descriptor` is a live file descriptor.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `descriptor` is live and `status` is writable for the syscall.
    if unsafe { libc::fstat(descriptor, &mut status) } == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_link_at(parent_fd: RawFd, name: &OsStr) -> io::Result<Vec<u8>> {
    let name = c_string(name)?;
    let mut capacity = 256;
    loop {
        let mut target = vec![0_u8; capacity];
        // SAFETY: `target` is writable for `capacity` bytes, and the path arguments are valid during the call.
        let length = unsafe {
            libc::readlinkat(
                parent_fd,
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < target.len() {
            target.truncate(length);
            return Ok(target);
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|length| *length <= 1024 * 1024)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "symlink target is too long")
            })?;
    }
}

fn split_components(path: &[u8]) -> VecDeque<OsString> {
    path.split(|byte| *byte == b'/')
        .map(|part| OsString::from_vec(part.to_vec()))
        .collect()
}

fn prepend_components(pending: &mut VecDeque<OsString>, path: &[u8]) {
    for component in split_components(path).into_iter().rev() {
        pending.push_front(component);
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn verify_root_descriptor(descriptor: RawFd, root_path: &Path) -> Result<(), CliError> {
    let opened = fstat(descriptor).map_err(|_| CliError::new("invalid path"))?;
    let named = fs::metadata(root_path).map_err(|_| CliError::new("invalid path"))?;
    if opened.st_dev != named.dev() || opened.st_ino != named.ino() {
        return Err(CliError::new("invalid path"));
    }
    Ok(())
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn same_inode(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

fn file_type(status: &libc::stat) -> libc::mode_t {
    status.st_mode & libc::S_IFMT
}

fn is_symlink(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFLNK
}

fn is_directory(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFDIR
}

fn is_regular_file(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFREG
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the live DIR pointer returned by `fdopendir`.
        unsafe { libc::closedir(self.0) };
    }
}

struct ListedName {
    sort_key: Vec<u32>,
    name: OsString,
}

impl ListedName {
    fn new(bytes: Vec<u8>) -> Self {
        let sort_key = python_codepoints(&bytes);
        let name = OsString::from_vec(bytes);
        Self { sort_key, name }
    }
}

fn python_codepoints(bytes: &[u8]) -> Vec<u32> {
    let mut codepoints = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(valid) => {
                codepoints.extend(valid.chars().map(u32::from));
                break;
            }
            Err(error) => {
                let valid_length = error.valid_up_to();
                if valid_length > 0 {
                    let valid = std::str::from_utf8(&bytes[offset..offset + valid_length])
                        .expect("invariant: `valid_up_to` identifies valid UTF-8");
                    codepoints.extend(valid.chars().map(u32::from));
                    offset += valid_length;
                }
                let invalid_length = error.error_len().unwrap_or(bytes.len() - offset);
                codepoints.extend(
                    bytes[offset..offset + invalid_length]
                        .iter()
                        .map(|byte| 0xdc00 + u32::from(*byte)),
                );
                offset += invalid_length;
            }
        }
    }
    codepoints
}

fn quote_json(codepoints: &[u32]) -> String {
    let mut quoted = String::from("\"");
    for codepoint in codepoints {
        match *codepoint {
            0x08 => quoted.push_str("\\b"),
            0x09 => quoted.push_str("\\t"),
            0x0a => quoted.push_str("\\n"),
            0x0c => quoted.push_str("\\f"),
            0x0d => quoted.push_str("\\r"),
            0x00..=0x1f => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x20..=0x7e if *codepoint != u32::from(b'"') && *codepoint != u32::from(b'\\') => {
                quoted.push(
                    char::from_u32(*codepoint).expect("invariant: ASCII is a Unicode scalar"),
                );
            }
            0x22 => quoted.push_str("\\\""),
            0x5c => quoted.push_str("\\\\"),
            0x7f..=0xffff => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x10000..=0x10ffff => {
                let scalar = codepoint - 0x10000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                quoted.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
            _ => quoted.push('\u{fffd}'),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::{
        capture_workspace_root_at, create_text_file, create_text_file_at_with_writer,
        replace_text_file_at, replace_text_file_at_with_writer, WorkspaceRoot, MAX_FILE_SIZE,
    };
    use std::fs;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn failed_write_removes_the_partial_file_it_created() {
        let path = std::env::temp_dir().join(format!(
            "krypper-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let root = fs::File::open(&path).unwrap();
        let mut writes = 0;
        let result = create_text_file_at_with_writer(
            root.as_raw_fd(),
            std::ffi::OsStr::new("partial.txt"),
            b"complete text",
            |fd, bytes| {
                writes += 1;
                if writes == 1 {
                    // SAFETY: `fd` is the open file created by the function under test; `bytes` has at least three bytes.
                    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), 3) };
                    if count < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(count as usize)
                } else {
                    Err(io::Error::other("simulated disk failure"))
                }
            },
        );
        assert_eq!(result.unwrap_err().message(), "unable to write file");
        assert!(!path.join("partial.txt").exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_changes_only_one_exact_match_in_an_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "before exact match after").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        let result = replace_text_file_at(
            &root,
            std::ffi::OsStr::new("note.txt"),
            "exact match",
            "new text",
        );

        assert_eq!(result.unwrap(), "before new text after".len());
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            "before new text after"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_accepts_a_result_at_the_one_mib_limit() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-limit-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "old").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };
        let replacement = "x".repeat(MAX_FILE_SIZE);

        let result =
            replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "old", &replacement);

        assert_eq!(result.unwrap(), MAX_FILE_SIZE);
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            replacement
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_rejects_empty_missing_and_ambiguous_matches_without_changes() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-matches-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "aaaa").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        for (match_text, expected_error) in [
            ("", "match text must not be empty"),
            ("missing", "match text was not found"),
            ("aa", "match text is ambiguous"),
        ] {
            let error = replace_text_file_at(
                &root,
                std::ffi::OsStr::new("note.txt"),
                match_text,
                "replacement",
            )
            .unwrap_err();
            assert_eq!(error.message(), expected_error);
            assert_eq!(fs::read_to_string(path.join("note.txt")).unwrap(), "aaaa");
        }

        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn failed_replacement_keeps_the_original_and_removes_its_partial_stage() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "before exact match after").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };
        let mut writes = 0;

        let result = replace_text_file_at_with_writer(
            &root,
            std::ffi::OsStr::new("note.txt"),
            "exact match",
            "a much longer replacement",
            |fd, bytes| {
                writes += 1;
                if writes == 1 {
                    // SAFETY: `fd` is the staged file descriptor and `bytes` contains at least three bytes.
                    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), 3) };
                    if count < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(count as usize)
                } else {
                    Err(io::Error::other("simulated disk failure"))
                }
            },
        );

        assert_eq!(
            result.unwrap_err().message(),
            "unable to stage file replacement"
        );
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            "before exact match after"
        );
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_rejects_unsafe_paths_non_regular_files_and_oversize_content() {
        use std::os::unix::fs::symlink;

        let path = std::env::temp_dir().join(format!(
            "krypper-replace-safety-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let outside = path.join("outside");
        let workspace = path.join("workspace");
        fs::create_dir(&path).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(&outside, "outside text").unwrap();
        fs::write(workspace.join("note.txt"), "regular text").unwrap();
        fs::write(workspace.join("large.txt"), vec![b'x'; MAX_FILE_SIZE + 1]).unwrap();
        fs::write(workspace.join("not-a-directory"), "file").unwrap();
        fs::create_dir(workspace.join("directory")).unwrap();
        symlink("note.txt", workspace.join("linked.txt")).unwrap();
        symlink(".", workspace.join("linked-parent")).unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&workspace).unwrap().into(),
            path: fs::canonicalize(&workspace).unwrap(),
        };
        let too_long = "x".repeat(MAX_FILE_SIZE + 1);
        let fits_but_overflows = "x".repeat(MAX_FILE_SIZE);
        let cases = [
            (
                std::ffi::OsStr::new("../outside"),
                "text",
                "replacement",
                "path must stay inside the current directory",
            ),
            (
                std::ffi::OsStr::new("linked.txt"),
                "text",
                "replacement",
                "symlink paths are not allowed",
            ),
            (
                std::ffi::OsStr::new("linked-parent/note.txt"),
                "text",
                "replacement",
                "symlink paths are not allowed",
            ),
            (
                std::ffi::OsStr::new("missing-parent/new.txt"),
                "text",
                "replacement",
                "parent directory not found",
            ),
            (
                std::ffi::OsStr::new("not-a-directory/new.txt"),
                "text",
                "replacement",
                "parent is not a directory",
            ),
            (
                std::ffi::OsStr::new("directory"),
                "text",
                "replacement",
                "not a regular file",
            ),
            (
                std::ffi::OsStr::new("large.txt"),
                "x",
                "replacement",
                "file exceeds 1 MiB",
            ),
            (
                std::ffi::OsStr::new("note.txt"),
                "text",
                fits_but_overflows.as_str(),
                "replacement would exceed 1 MiB",
            ),
            (
                std::ffi::OsStr::new("note.txt"),
                "text",
                too_long.as_str(),
                "replacement text exceeds 1 MiB",
            ),
        ];

        for (file, match_text, replacement_text, expected_error) in cases {
            let error =
                replace_text_file_at(&root, file, match_text, replacement_text).unwrap_err();
            assert_eq!(error.message(), expected_error);
        }
        assert_eq!(
            fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "regular text"
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside text");
        assert_eq!(fs::read_dir(&workspace).unwrap().count(), 6);
        assert!(!workspace.join("missing-parent").exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn write_enforces_the_encoded_one_mib_limit_before_creation() {
        let _lock = crate::CURRENT_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_directory = std::env::current_dir().unwrap();
        let path = std::env::temp_dir().join(format!(
            "krypper-write-limit-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        std::env::set_current_dir(&path).unwrap();
        let exact = "é".repeat(MAX_FILE_SIZE / 2);
        let result = create_text_file(std::ffi::OsStr::new("exact.txt"), &exact);
        let too_large = "é".repeat(MAX_FILE_SIZE / 2 + 1);
        let rejected = create_text_file(std::ffi::OsStr::new("large.txt"), &too_large);
        std::env::set_current_dir(previous_directory).unwrap();
        assert!(result.is_ok());
        assert_eq!(rejected.unwrap_err().message(), "text exceeds 1 MiB");
        assert_eq!(
            fs::metadata(path.join("exact.txt")).unwrap().len(),
            MAX_FILE_SIZE as u64
        );
        assert!(!path.join("large.txt").exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn directory_names_match_python_json_escaping_and_sort_order() {
        let name = super::ListedName::new(vec![b'\xff']);
        assert_eq!(super::quote_json(&name.sort_key), "\"\\udcff\"");
        assert_eq!(name.sort_key, vec![0xdcff]);
    }

    fn guidance_workspace() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "krypper-guidance-read-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (base, workspace)
    }

    #[test]
    fn root_agents_read_is_bounded_and_does_not_load_nested_files() {
        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("nested")).unwrap();
        fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
        fs::write(workspace.join("nested/AGENTS.md"), "nested guidance").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        assert_eq!(
            super::read_root_agents_md_at(&root).unwrap().as_deref(),
            Some("root guidance")
        );
        fs::remove_file(workspace.join("AGENTS.md")).unwrap();
        assert_eq!(super::read_root_agents_md_at(&root).unwrap(), None);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_guidance_is_root_to_target_ordered_and_excludes_siblings() {
        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("src/deep")).unwrap();
        fs::create_dir_all(workspace.join("tests")).unwrap();
        fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
        fs::write(workspace.join("src/AGENTS.md"), "src guidance").unwrap();
        fs::write(workspace.join("src/deep/AGENTS.md"), "deep guidance").unwrap();
        fs::write(workspace.join("tests/AGENTS.md"), "sibling guidance").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        let nested =
            super::read_project_guidance_at(&root, &[PathBuf::from("src/deep/generated.rs")])
                .unwrap();
        assert_eq!(
            nested.as_deref(),
            Some(
                "root guidance\n\n--- AGENTS.md: src/AGENTS.md ---\nsrc guidance\n\n--- AGENTS.md: src/deep/AGENTS.md ---\ndeep guidance"
            )
        );
        assert_eq!(
            super::read_project_guidance_at(&root, &[PathBuf::from("note.txt")]).unwrap(),
            Some("root guidance".to_owned())
        );
        fs::remove_file(workspace.join("AGENTS.md")).unwrap();
        assert_eq!(
            super::read_project_guidance_at(&root, &[PathBuf::from("note.txt")]).unwrap(),
            None
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_guidance_rejects_out_of_workspace_targets_in_a_mixed_plan() {
        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
        let outside = base.join("outside.txt");
        fs::write(&outside, "outside canary").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        let result = super::read_project_guidance_at(
            &root,
            &[
                PathBuf::from("src/generated.rs"),
                PathBuf::from("../outside.txt"),
            ],
        );

        assert_eq!(result, Err(super::RootGuidanceReadError::InvalidTargetPath));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_guidance_rejects_symlinked_edit_targets_without_reading_canaries() {
        use std::os::unix::fs::symlink;

        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
        let outside_file = base.join("outside.txt");
        fs::write(&outside_file, "outside file canary").unwrap();
        symlink(&outside_file, workspace.join("src/linked.rs")).unwrap();
        let outside_directory = base.join("outside-directory");
        fs::create_dir(&outside_directory).unwrap();
        let outside_canary = outside_directory.join("canary.txt");
        fs::write(&outside_canary, "outside directory canary").unwrap();
        symlink(&outside_directory, workspace.join("linked-directory")).unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        let file_target = super::read_project_guidance_at(
            &root,
            &[
                PathBuf::from("src/generated.rs"),
                PathBuf::from("src/linked.rs"),
            ],
        );
        let parent_target = super::read_project_guidance_at(
            &root,
            &[PathBuf::from("linked-directory/generated.rs")],
        );

        assert_eq!(file_target, Err(super::RootGuidanceReadError::Symlink));
        assert_eq!(parent_target, Err(super::RootGuidanceReadError::Symlink));
        assert_eq!(
            fs::read_to_string(outside_file).unwrap(),
            "outside file canary"
        );
        assert_eq!(
            fs::read_to_string(outside_canary).unwrap(),
            "outside directory canary"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_guidance_rejects_non_directory_edit_target_parents() {
        let (base, workspace) = guidance_workspace();
        fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
        fs::write(workspace.join("blocked"), "not a directory").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        let result =
            super::read_project_guidance_at(&root, &[PathBuf::from("blocked/generated.rs")]);

        assert_eq!(result, Err(super::RootGuidanceReadError::InvalidTargetPath));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn nested_guidance_changed_during_a_safe_read_is_rejected() {
        use std::io::Read;

        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        let agents = workspace.join("src/AGENTS.md");
        fs::write(&agents, "stable guidance").unwrap();
        let outside = base.join("outside.txt");
        fs::write(&outside, "outside canary").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        let result = super::read_project_guidance_at_with_reader(
            &root,
            &[PathBuf::from("src/generated.rs")],
            |file| {
                fs::write(&agents, "changed guidance")?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            },
        );

        assert_eq!(result, Err(super::RootGuidanceReadError::ChangedDuringRead));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn nested_guidance_created_after_absence_checks_is_rejected() {
        let (base, workspace) = guidance_workspace();
        fs::create_dir_all(workspace.join("src")).unwrap();
        let outside = base.join("outside.txt");
        fs::write(&outside, "outside canary").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();
        let mut injected = false;

        let result = super::read_project_guidance_at_with_reader_and_hook(
            &root,
            &[PathBuf::from("src/generated.rs")],
            |_| Ok(Vec::new()),
            |directory| {
                if directory == [std::ffi::OsString::from("src")] {
                    fs::write(workspace.join("src/AGENTS.md"), "late nested guidance")?;
                    injected = true;
                }
                Ok(())
            },
        );

        assert!(
            injected,
            "the guidance file must be created after the absent read"
        );
        assert_eq!(result, Err(super::RootGuidanceReadError::ChangedDuringRead));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_agents_read_rejects_invalid_utf8_and_oversized_files() {
        let (base, workspace) = guidance_workspace();
        let root = capture_workspace_root_at(&workspace).unwrap();

        fs::write(workspace.join("AGENTS.md"), [0xff]).unwrap();
        assert_eq!(
            super::read_root_agents_md_at(&root),
            Err(super::RootGuidanceReadError::InvalidUtf8)
        );

        fs::write(workspace.join("AGENTS.md"), vec![b'x'; 16 * 1024 + 1]).unwrap();
        assert_eq!(
            super::read_root_agents_md_at(&root),
            Err(super::RootGuidanceReadError::TooLarge)
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_agents_read_rejects_symlinks_without_reading_outside_canary() {
        use std::os::unix::fs::symlink;

        let (base, workspace) = guidance_workspace();
        let outside = base.join("outside.txt");
        fs::write(&outside, "outside canary").unwrap();
        symlink(&outside, workspace.join("AGENTS.md")).unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        assert_eq!(
            super::read_root_agents_md_at(&root),
            Err(super::RootGuidanceReadError::Symlink)
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_agents_read_reports_read_failures_and_changes_during_read() {
        use std::io::{self, Read};

        let (base, workspace) = guidance_workspace();
        let agents = workspace.join("AGENTS.md");
        fs::write(&agents, "stable guidance").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        assert_eq!(
            super::read_root_agents_md_at_with_reader(&root, |_| {
                Err(io::Error::other("injected read failure"))
            }),
            Err(super::RootGuidanceReadError::ReadFailed)
        );
        assert_eq!(
            super::read_root_agents_md_at_with_reader(&root, |file| {
                fs::write(&agents, "changed guidance")?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            }),
            Err(super::RootGuidanceReadError::ChangedDuringRead)
        );
        fs::remove_dir_all(base).unwrap();
    }
}
