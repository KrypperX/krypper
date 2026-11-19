//! Reading text files and the root AGENTS.md snapshot through the containment walk.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;

use super::paths::{open_cwd_relative, open_workspace_relative, verify_root_descriptor};
use super::syscalls::{
    fstat, is_regular_file, is_symlink, open_at, open_current_directory, open_directory_path,
    same_file_snapshot, stat_at,
};
use super::types::{
    CliError, RootGuidanceReadError, WorkspaceRoot, MAX_FILE_SIZE, MAX_ROOT_AGENTS_BYTES,
};

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
    read_root_agents_md_at_with_reader(root, |file| {
        let mut contents = Vec::with_capacity(MAX_ROOT_AGENTS_BYTES + 1);
        file.take((MAX_ROOT_AGENTS_BYTES + 1) as u64)
            .read_to_end(&mut contents)?;
        Ok(contents)
    })
}

pub(super) fn read_root_agents_md_at_with_reader(
    root: &WorkspaceRoot,
    read: impl FnOnce(&mut File) -> io::Result<Vec<u8>>,
) -> Result<Option<String>, RootGuidanceReadError> {
    let parent_fd = root.descriptor.as_raw_fd();
    let name = OsStr::new("AGENTS.md");
    let original_status = match stat_at(parent_fd, name) {
        Ok(status) => status,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
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
