//! Platform-neutral syscall facade for workspace safety.
//!
//! File descriptors, native metadata structures, errno values, and OS path
//! encodings are confined to `platform/{unix,windows}.rs`. Callers work with
//! owned directory handles, portable file identity and file-kind values, and
//! standard library files.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

use super::types::{CreateMode, DirHandle, FileKind, FileStatus};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OpenRequest {
    ReadFile,
    ReadDirectory,
    CreateFile(CreateMode),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PathComponent {
    Prefix(OsString),
    Root,
    Current,
    Parent,
    Normal(OsString),
}

#[cfg(unix)]
#[path = "platform/unix.rs"]
mod backend;
#[cfg(windows)]
#[path = "platform/windows.rs"]
mod backend;

pub(super) use backend::{
    close_file, duplicate_directory, fstat_file, is_symlink_loop, mkdir_at, open_at,
    open_current_directory, open_directory_path, read_link_at, rename_file_at, set_mode, stat_at,
    sync_file, unlink_at, unlink_created_file, DirectoryStream,
};

pub(super) fn duplicate_directory_file(directory: &DirHandle) -> io::Result<File> {
    duplicate_directory(directory).map(DirHandle::into_file)
}

pub(super) fn fstat_directory(directory: &DirHandle) -> io::Result<FileStatus> {
    fstat_file(directory.file())
}

pub(super) fn same_inode(left: &FileStatus, right: &FileStatus) -> bool {
    left.identity == right.identity
}

pub(super) fn same_file_snapshot(left: &FileStatus, right: &FileStatus) -> bool {
    left.identity == right.identity
        && left.kind == right.kind
        && left.mode == right.mode
        && left.size == right.size
        && left.modified_seconds == right.modified_seconds
        && left.modified_nanos == right.modified_nanos
        && left.changed_seconds == right.changed_seconds
        && left.changed_nanos == right.changed_nanos
}

pub(super) fn is_symlink(status: &FileStatus) -> bool {
    status.kind == FileKind::Symlink
}

pub(super) fn is_directory(status: &FileStatus) -> bool {
    status.kind == FileKind::Directory
}

pub(super) fn is_regular_file(status: &FileStatus) -> bool {
    status.kind == FileKind::Regular
}

pub(super) fn is_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

pub(super) fn is_already_exists(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AlreadyExists
}

pub(super) fn is_interrupted(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted
}

pub(super) fn write_to_file(file: &mut File, bytes: &[u8]) -> io::Result<usize> {
    std::io::Write::write(file, bytes)
}

pub(super) fn write_all_to_file<F>(
    file: &mut File,
    contents: &[u8],
    writer: &mut F,
) -> io::Result<()>
where
    F: FnMut(&mut File, &[u8]) -> io::Result<usize>,
{
    let mut offset = 0;
    while offset < contents.len() {
        match writer(file, &contents[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "zero-byte write")),
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => return Err(io::Error::other("writer reported an invalid byte count")),
            Err(error) if is_interrupted(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn split_components(path: &OsStr) -> VecDeque<PathComponent> {
    Path::new(path)
        .components()
        .map(|component| match component {
            Component::Prefix(prefix) => PathComponent::Prefix(prefix.as_os_str().to_os_string()),
            Component::RootDir => PathComponent::Root,
            Component::CurDir => PathComponent::Current,
            Component::ParentDir => PathComponent::Parent,
            Component::Normal(name) => PathComponent::Normal(name.to_os_string()),
        })
        .collect()
}

pub(super) fn prepend_components(pending: &mut VecDeque<PathComponent>, path: &Path) {
    for component in split_components(path.as_os_str()).into_iter().rev() {
        pending.push_front(component);
    }
}

pub(super) fn contains_nul(path: &OsStr) -> bool {
    backend::contains_nul(path)
}

pub(super) fn is_relative_workspace_path(path: &OsStr) -> bool {
    !Path::new(path).is_absolute()
        && !split_components(path)
            .iter()
            .any(|component| matches!(component, PathComponent::Prefix(_) | PathComponent::Root))
}

pub(super) fn path_has_trailing_separator(path: &OsStr) -> bool {
    backend::path_has_trailing_separator(path)
}
