//! Creation of files and directories inside a verified workspace parent.

use super::paths::{open_workspace_parent, validate_workspace_mutation_path};
use super::syscalls::{
    close_file, contains_nul, duplicate_directory, fstat_file, is_already_exists, is_directory,
    is_interrupted, is_not_found, is_relative_workspace_path, is_symlink, mkdir_at, open_at,
    open_current_directory, same_inode, split_components, stat_at, unlink_at, write_to_file,
    OpenRequest, PathComponent,
};
use super::types::{CliError, CreateMode, DirHandle, WorkspaceRoot, MAX_FILE_SIZE};
use std::ffi::OsStr;
use std::fs::File;
use std::io;

pub(crate) fn create_text_file(path: &OsStr, text: &str) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    let root = open_current_directory()
        .map_err(|error| CliError::from_io(&error, "unable to open current directory"))?;
    create_text_file_at_with_writer(&root, path, contents, write_to_file)
}

pub(crate) fn create_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    text: &str,
) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    create_text_file_at_with_writer(&root.descriptor, path, contents, write_to_file)
}

pub(crate) fn create_directory_at(root: &WorkspaceRoot, path: &OsStr) -> Result<(), CliError> {
    validate_output_path(path, "invalid directory path")?;
    let (parent, name) = open_workspace_parent(root, path)?;
    match stat_at(&parent, &name) {
        Ok(status) if is_symlink(&status) => {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        Ok(_) => return Err(CliError::new("path already exists")),
        Err(error) if is_not_found(&error) => {}
        Err(error) => return Err(CliError::from_io(&error, "invalid directory path")),
    }

    mkdir_at(&parent, &name, CreateMode::from_bits(0o777)).map_err(|error| {
        if is_already_exists(&error) {
            CliError::new("path already exists")
        } else {
            CliError::from_io(&error, "unable to create directory")
        }
    })
}

pub(super) fn create_text_file_at_with_writer<F>(
    root: &DirHandle,
    path: &OsStr,
    contents: &[u8],
    mut writer: F,
) -> Result<(), CliError>
where
    F: FnMut(&mut File, &[u8]) -> io::Result<usize>,
{
    validate_output_path(path, "invalid file path")?;
    validate_workspace_mutation_path(path)?;
    let mut components = split_components(path);
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    let name = match name {
        PathComponent::Normal(name) => name,
        _ => return Err(CliError::new("invalid file path")),
    };

    let mut directories = vec![duplicate_directory(root)
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
        let opened = DirHandle::from_file(opened);
        let opened_status = fstat_file(opened.file())
            .map_err(|error| CliError::from_io(&error, "invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while creating file"));
        }
        directories.push(opened);
    }

    let parent = directories
        .last()
        .expect("invariant: the root handle remains on the stack");
    let file = open_at(
        parent,
        &name,
        OpenRequest::CreateFile(CreateMode::from_bits(0o666)),
    )
    .map_err(|error| {
        if is_already_exists(&error) {
            CliError::new("file already exists")
        } else if is_not_found(&error) {
            CliError::new("parent directory not found")
        } else {
            CliError::from_io(&error, "unable to create file")
        }
    })?;
    finish_new_file(file, parent, &name, contents, &mut writer)
}

pub(super) fn finish_new_file<F>(
    mut file: File,
    parent: &DirHandle,
    name: &OsStr,
    contents: &[u8],
    writer: &mut F,
) -> Result<(), CliError>
where
    F: FnMut(&mut File, &[u8]) -> io::Result<usize>,
{
    let (identity, mut failure) = match fstat_file(&file) {
        Ok(status) => (Some(status.identity), false),
        Err(_) => (None, true),
    };
    let mut offset = 0;

    while !failure && offset < contents.len() {
        match writer(&mut file, &contents[offset..]) {
            Ok(0) => failure = true,
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => failure = true,
            Err(error) if is_interrupted(&error) => continue,
            Err(_) => failure = true,
        }
    }

    if close_file(file).is_err() {
        failure = true;
    }
    if !failure {
        return Ok(());
    }

    if let Some(created) = identity {
        if let Ok(current) = stat_at(parent, name) {
            if current.identity == created && unlink_at(parent, name).is_err() {
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
    validate_output_path(path, "invalid file path")?;
    let contents = text.as_bytes();
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("text exceeds 1 MiB"));
    }
    Ok(contents)
}

fn validate_output_path(path: &OsStr, error: &'static str) -> Result<(), CliError> {
    if contains_nul(path) {
        return Err(CliError::new(error));
    }
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path.is_empty() || super::syscalls::path_has_trailing_separator(path) {
        return Err(CliError::new(error));
    }
    Ok(())
}
