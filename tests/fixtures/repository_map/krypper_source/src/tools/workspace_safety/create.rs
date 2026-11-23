//! Creation of files and directories inside a verified workspace parent.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::paths::open_workspace_parent;
use super::syscalls::{
    close_fd, duplicate_fd, fstat, is_directory, is_symlink, mkdir_at, open_at,
    open_current_directory, same_inode, split_components, stat_at, unlink_at, write_to_fd,
};
use super::types::{CliError, WorkspaceRoot, MAX_FILE_SIZE};

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

    mkdir_at(parent.as_raw_fd(), &name, 0o777).map_err(|error| {
        if error.raw_os_error() == Some(libc::EEXIST) {
            CliError::new("path already exists")
        } else {
            CliError::new("unable to create directory")
        }
    })
}

pub(super) fn create_text_file_at_with_writer<F>(
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

pub(super) fn finish_new_file<F>(
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

    if close_fd(raw_fd).is_err() {
        failure = true;
    }
    if !failure {
        return Ok(());
    }

    if let Some(created) = identity {
        if let Ok(current) = stat_at(parent_fd, name) {
            if same_inode(&created, &current) && unlink_at(parent_fd, name).is_err() {
                return Err(CliError::new("unable to write file"));
            }
        }
    }
    Err(CliError::new("unable to write file"))
}

pub(super) fn validate_create_text_file<'a>(
    path: &OsStr,
    text: &'a str,
) -> Result<&'a [u8], CliError> {
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
