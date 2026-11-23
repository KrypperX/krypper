//! Atomic replacement of existing files, including unified-diff patches.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use super::patch::{self, MAX_PATCH_BYTES};
use super::paths::open_workspace_parent;
use super::read::read_open_text_file;
use super::syscalls::{
    close_fd, fstat, is_regular_file, is_symlink, open_at, rename_file_at, same_inode, set_mode,
    stat_at, sync_fd, unlink_created_file, write_all_to_fd, write_to_fd,
};
use super::types::{CliError, OriginalFileState, PatchEvidence, WorkspaceRoot, MAX_FILE_SIZE};

pub(crate) fn replace_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
) -> Result<usize, CliError> {
    replace_text_file_at_with_writer(root, path, match_text, replacement_text, write_to_fd)
}

pub(super) fn replace_text_file_at_with_writer<F>(
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

pub(super) fn replace_file_contents_atomically<W, C, R>(
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
    if !failed && set_mode(raw_fd, original.status.st_mode & 0o777).is_err() {
        failed = true;
    }
    if !failed && sync_fd(raw_fd).is_err() {
        failed = true;
    }
    if close_fd(raw_fd).is_err() {
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

pub(super) fn read_file_contents_at_for_verification(
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

pub(super) fn unique_match_range(
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

pub(super) fn apply_patch_file_at_with_ops<W, C, R>(
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
