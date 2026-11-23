//! Resolution of workspace-relative paths and the containment walk itself.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::types::{CliError, WorkspaceRoot, MAX_SYMLINKS};

use super::syscalls::{
    duplicate_fd, fstat, is_directory, is_regular_file, is_symlink, open_at,
    open_current_directory, prepend_components, read_link_at, same_inode, split_components,
    stat_at,
};

pub(super) fn open_cwd_relative(path: &OsStr, directory: bool) -> Result<File, CliError> {
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }

    let root = open_current_directory().map_err(|_| CliError::new("unable to read file"))?;
    let root_path = fs::canonicalize(".").map_err(|_| CliError::new("invalid path"))?;
    open_root_relative(root, &root_path, path, directory)
}

pub(super) fn open_workspace_relative(
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

pub(super) fn open_root_relative(
    root: OwnedFd,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    open_root_relative_with_resolved_path(root, root_path, path, directory).map(|(file, _)| file)
}

pub(super) fn open_root_relative_with_resolved_path(
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

pub(super) fn normalize_absolute(path: &Path) -> PathBuf {
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

pub(super) fn open_workspace_parent(
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

pub(super) fn verify_root_descriptor(descriptor: RawFd, root_path: &Path) -> Result<(), CliError> {
    let opened = fstat(descriptor).map_err(|_| CliError::new("invalid path"))?;
    let named = fs::metadata(root_path).map_err(|_| CliError::new("invalid path"))?;
    if opened.st_dev != named.dev() || opened.st_ino != named.ino() {
        return Err(CliError::new("invalid path"));
    }
    Ok(())
}
