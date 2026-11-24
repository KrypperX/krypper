//! Resolution of workspace-relative paths and the containment walk itself.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use super::syscalls::{
    duplicate_directory, duplicate_directory_file, fstat_directory, fstat_file, is_directory,
    is_not_found, is_regular_file, is_relative_workspace_path, is_symlink, open_at,
    open_current_directory, open_directory_path, prepend_components, read_link_at, same_inode,
    split_components, stat_at, OpenRequest, PathComponent,
};
use super::types::{CliError, DirHandle, WorkspaceRoot, MAX_SYMLINKS};

pub(super) fn open_cwd_relative(path: &OsStr, directory: bool) -> Result<File, CliError> {
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }

    let root = open_current_directory()
        .map_err(|error| CliError::from_io(&error, "unable to read file"))?;
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

pub(super) fn open_workspace_directory(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<DirHandle, CliError> {
    open_workspace_relative(root, path, true).map(DirHandle::from_file)
}

pub(crate) fn open_workspace_relative_with_resolved_path(
    root: &WorkspaceRoot,
    path: &OsStr,
    directory: bool,
) -> Result<(File, PathBuf), CliError> {
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    let descriptor = duplicate_directory(&root.descriptor)
        .map_err(|error| CliError::from_io(&error, "unable to read file"))?;
    open_root_relative_with_resolved_path(descriptor, &root.path, path, directory)
}

pub(super) fn open_root_relative(
    root: DirHandle,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    open_root_relative_with_resolved_path(root, root_path, path, directory).map(|(file, _)| file)
}

pub(super) fn open_root_relative_with_resolved_path(
    root: DirHandle,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<(File, PathBuf), CliError> {
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }

    let mut directories = vec![root];
    let mut resolved_components = Vec::<OsString>::new();
    let mut pending = split_components(path);
    let mut symlinks = 0;

    while let Some(component) = pending.pop_front() {
        let name = match component {
            PathComponent::Current => continue,
            PathComponent::Parent => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                resolved_components.pop();
                continue;
            }
            PathComponent::Normal(name) => name,
            PathComponent::Prefix(_) | PathComponent::Root => {
                return Err(CliError::new("absolute paths are not allowed"));
            }
        };

        let parent = directories
            .last()
            .expect("invariant: the root handle remains on the stack");
        let status = stat_at(parent, &name).map_err(|error| {
            if is_not_found(&error) {
                CliError::new("file not found")
            } else {
                CliError::from_io(&error, "invalid path")
            }
        })?;

        if is_symlink(&status) {
            symlinks += 1;
            if symlinks > MAX_SYMLINKS {
                return Err(CliError::new("invalid path"));
            }
            let target = read_link_at(parent, &name).map_err(|_| CliError::new("invalid path"))?;
            if target.is_absolute() {
                let mut target_path = normalize_absolute(&target);
                if !target_path.starts_with(root_path) {
                    target_path = fs::canonicalize(&target_path).map_err(|_| {
                        CliError::new("path must stay inside the current directory")
                    })?;
                    if !target_path.starts_with(root_path) {
                        return Err(CliError::new("path must stay inside the current directory"));
                    }
                }
                verify_root_descriptor(&directories[0], root_path)?;
                let relative = target_path
                    .strip_prefix(root_path)
                    .map_err(|_| CliError::new("invalid path"))?;
                directories.truncate(1);
                resolved_components.clear();
                prepend_components(&mut pending, relative);
            } else {
                prepend_components(&mut pending, &target);
            }
            continue;
        }

        let final_component = !pending
            .iter()
            .any(|part| !matches!(part, PathComponent::Current));
        if final_component {
            if directory && !is_directory(&status) {
                return Err(CliError::new("not a directory"));
            }
            if !directory && !is_regular_file(&status) {
                return Err(CliError::new("not a regular file"));
            }
        }

        let request = if !final_component || directory {
            OpenRequest::ReadDirectory
        } else {
            OpenRequest::ReadFile
        };
        let opened = open_at(parent, &name, request)
            .map_err(|error| CliError::from_io(&error, "unable to read file"))?;
        let opened_status = fstat_file(&opened)
            .map_err(|error| CliError::from_io(&error, "unable to read file"))?;
        if !same_inode(&status, &opened_status)
            || (final_component && directory && !is_directory(&opened_status))
            || (final_component && !directory && !is_regular_file(&opened_status))
        {
            return Err(CliError::new("unable to read file"));
        }
        resolved_components.push(name);
        if final_component {
            return Ok((opened, resolved_components.into_iter().collect()));
        }
        directories.push(DirHandle::from_file(opened));
    }

    let current_directory = directories
        .last()
        .expect("invariant: the root handle remains on the stack");
    let file = duplicate_directory_file(current_directory)
        .map_err(|error| CliError::from_io(&error, "unable to read file"))?;
    Ok((file, resolved_components.into_iter().collect()))
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
) -> Result<(DirHandle, OsString), CliError> {
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    let mut components = split_components(path);
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    let name = match name {
        PathComponent::Normal(name) => name,
        _ => return Err(CliError::new("invalid file path")),
    };

    let mut directories = vec![duplicate_directory(&root.descriptor)
        .map_err(|error| CliError::from_io(&error, "unable to open current directory"))?];
    for component in components {
        let name = match component {
            PathComponent::Current => continue,
            PathComponent::Parent => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            PathComponent::Normal(name) => name,
            PathComponent::Prefix(_) | PathComponent::Root => {
                return Err(CliError::new("absolute paths are not allowed"));
            }
        };

        let parent = directories
            .last()
            .expect("invariant: the root handle remains on the stack");
        let status = stat_at(parent, &name).map_err(|error| {
            if is_not_found(&error) {
                CliError::new("parent directory not found")
            } else {
                CliError::from_io(&error, "invalid parent directory")
            }
        })?;
        if is_symlink(&status) {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        if !is_directory(&status) {
            return Err(CliError::new("parent is not a directory"));
        }

        let opened = open_at(parent, &name, OpenRequest::ReadDirectory)
            .map_err(|error| CliError::from_io(&error, "invalid parent directory"))?;
        let opened_directory = DirHandle::from_file(opened);
        let opened_status = fstat_directory(&opened_directory)
            .map_err(|error| CliError::from_io(&error, "invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while replacing file"));
        }
        directories.push(opened_directory);
    }

    let parent = directories
        .pop()
        .expect("invariant: the root handle remains on the stack");
    Ok((parent, name))
}

pub(super) fn verify_root_descriptor(
    descriptor: &DirHandle,
    root_path: &Path,
) -> Result<(), CliError> {
    let opened =
        fstat_directory(descriptor).map_err(|error| CliError::from_io(&error, "invalid path"))?;
    let named = open_directory_path(root_path)
        .map_err(|error| CliError::from_io(&error, "invalid path"))?;
    let named =
        fstat_directory(&named).map_err(|error| CliError::from_io(&error, "invalid path"))?;
    if !same_inode(&opened, &named) {
        return Err(CliError::new("invalid path"));
    }
    Ok(())
}
