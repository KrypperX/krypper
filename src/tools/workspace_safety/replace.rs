//! Atomic replacement of existing files, including unified-diff patches.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path};
use std::sync::{Mutex, OnceLock};

use super::patch::{self, MAX_PATCH_BYTES};
use super::paths::open_workspace_parent;
use super::read::read_open_text_file;
use super::syscalls::{
    close_file, contains_nul, fstat_file, is_already_exists, is_not_found, is_regular_file,
    is_relative_workspace_path, is_symlink, open_at, path_has_trailing_separator, rename_file_at,
    same_inode, set_mode, stat_at, sync_file, try_lock_file_exclusive, unlink_created_file,
    write_all_to_file, write_to_file, OpenRequest, RenameFileError,
};
use super::types::{
    CliError, CreateMode, DirHandle, FileIdentity, OriginalFileState, PatchEvidence, WorkspaceRoot,
    MAX_FILE_SIZE,
};

pub(crate) fn replace_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
) -> Result<usize, CliError> {
    replace_text_file_at_with_writer(root, path, match_text, replacement_text, write_to_file)
}

pub(super) fn replace_text_file_at_with_writer<F>(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
    writer: F,
) -> Result<usize, CliError>
where
    F: FnMut(&mut File, &[u8]) -> io::Result<usize>,
{
    if contains_nul(path) {
        return Err(CliError::new("invalid file path"));
    }
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path.is_empty() || path_has_trailing_separator(path) {
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
    let original_status = stat_at(&parent, &name).map_err(|error| {
        if is_not_found(&error) {
            CliError::new("file not found")
        } else {
            CliError::from_io(&error, "invalid path")
        }
    })?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }

    let original_file = open_at(&parent, &name, OpenRequest::ReadFile)
        .map_err(|_| CliError::new("unable to open file"))?;
    let opened_status =
        fstat_file(&original_file).map_err(|_| CliError::new("unable to open file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while replacing it"));
    }

    let contents = read_open_text_file(original_file)?;
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

    let mut operations = ReplacementOperations {
        writer,
        committer: rename_file_at,
        readback: read_file_contents_at_for_verification,
        cleanup: unlink_created_file,
        mode_setter: set_mode,
    };
    replace_file_contents_atomically(
        &parent,
        &name,
        OriginalFileState {
            status: original_status,
            contents: contents.as_bytes(),
        },
        replacement.as_bytes(),
        &mut operations,
    )?;
    Ok(output_length)
}

pub(super) struct ReplacementOperations<W, C, R, U, M> {
    pub(super) writer: W,
    pub(super) committer: C,
    pub(super) readback: R,
    pub(super) cleanup: U,
    pub(super) mode_setter: M,
}

static IN_PROCESS_REPLACEMENTS: OnceLock<Mutex<HashSet<FileIdentity>>> = OnceLock::new();

struct InProcessReplacementLock(FileIdentity);

impl InProcessReplacementLock {
    fn try_acquire(identity: FileIdentity) -> Result<Self, CliError> {
        let registry = IN_PROCESS_REPLACEMENTS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut identities = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if identities.insert(identity) {
            Ok(Self(identity))
        } else {
            Err(CliError::new("file is locked by another replacement"))
        }
    }
}

impl Drop for InProcessReplacementLock {
    fn drop(&mut self) {
        let registry = IN_PROCESS_REPLACEMENTS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut identities = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identities.remove(&self.0);
    }
}

pub(super) fn replace_file_contents_atomically<W, C, R, U, M>(
    parent: &DirHandle,
    name: &OsStr,
    original: OriginalFileState<'_>,
    contents: &[u8],
    operations: &mut ReplacementOperations<W, C, R, U, M>,
) -> Result<(), CliError>
where
    W: FnMut(&mut File, &[u8]) -> io::Result<usize>,
    C: FnMut(&DirHandle, &OsStr, &OsStr) -> Result<(), RenameFileError>,
    R: FnMut(&DirHandle, &OsStr) -> Result<Vec<u8>, CliError>,
    U: FnMut(&DirHandle, &OsStr, Option<&FileIdentity>) -> io::Result<()>,
    M: FnMut(&File, CreateMode) -> io::Result<()>,
{
    let _process_lock = InProcessReplacementLock::try_acquire(original.status.identity)?;
    let lock_file = open_at(parent, name, OpenRequest::ReadFile)
        .map_err(|_| CliError::new("unable to lock file for replacement"))?;
    let lock_status =
        fstat_file(&lock_file).map_err(|_| CliError::new("unable to lock file for replacement"))?;
    if !same_inode(&original.status, &lock_status) || !is_regular_file(&lock_status) {
        return Err(CliError::new("file changed while replacing it"));
    }
    try_lock_file_exclusive(&lock_file).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            CliError::new("file is locked by another replacement")
        } else {
            CliError::new("unable to lock file for replacement")
        }
    })?;
    let locked_path_status =
        stat_at(parent, name).map_err(|_| CliError::new("file changed while replacing it"))?;
    if !same_inode(&original.status, &locked_path_status) || !is_regular_file(&locked_path_status) {
        return Err(CliError::new("file changed while replacing it"));
    }

    let (temporary_name, temporary_file) = (0..3)
        .find_map(|_| {
            let name = OsString::from(format!(".krypper-replace-{}", uuid::Uuid::new_v4()));
            match open_at(
                parent,
                &name,
                OpenRequest::CreateFile(CreateMode::from_bits(0o600)),
            ) {
                Ok(file) => Some(Ok((name, file))),
                Err(error) if is_already_exists(&error) => None,
                Err(_) => Some(Err(CliError::new("unable to stage file replacement"))),
            }
        })
        .transpose()?
        .ok_or_else(|| CliError::new("unable to stage file replacement"))?;

    let identity = fstat_file(&temporary_file)
        .ok()
        .or_else(|| stat_at(parent, &temporary_name).ok())
        .map(|status| status.identity);
    let mut temporary_file = temporary_file;
    let mut failed = identity.is_none();
    if !failed && write_all_to_file(&mut temporary_file, contents, &mut operations.writer).is_err()
    {
        failed = true;
    }
    if !failed && sync_file(&temporary_file).is_err() {
        failed = true;
    }
    if failed {
        let error = cleanup_failure(
            &mut operations.cleanup,
            parent,
            &temporary_name,
            identity.as_ref(),
            "unable to stage file replacement",
        );
        let _ = close_file(temporary_file);
        return Err(error);
    }

    let current_status = stat_at(parent, name).map_err(|_| {
        cleanup_failure(
            &mut operations.cleanup,
            parent,
            &temporary_name,
            identity.as_ref(),
            "file changed while replacing it",
        )
    })?;
    if !same_inode(&original.status, &current_status) || !is_regular_file(&current_status) {
        return Err(cleanup_failure(
            &mut operations.cleanup,
            parent,
            &temporary_name,
            identity.as_ref(),
            "file changed while replacing it",
        ));
    }

    let current_contents = match read_file_contents_at_for_verification(parent, name) {
        Ok(contents) => contents,
        Err(_) => {
            return Err(cleanup_failure(
                &mut operations.cleanup,
                parent,
                &temporary_name,
                identity.as_ref(),
                "unable to verify original file",
            ));
        }
    };
    if current_contents != original.contents {
        return Err(cleanup_failure(
            &mut operations.cleanup,
            parent,
            &temporary_name,
            identity.as_ref(),
            "file changed while replacing it",
        ));
    }

    match (operations.committer)(parent, &temporary_name, name) {
        Ok(()) => {}
        Err(RenameFileError::NotCommitted) => {
            return Err(cleanup_failure(
                &mut operations.cleanup,
                parent,
                &temporary_name,
                identity.as_ref(),
                "unable to replace file",
            ));
        }
        Err(RenameFileError::CommittedButUnverified) => {
            return Err(CliError::WorkspaceMutationCommittedButUnverified);
        }
    }
    if (operations.mode_setter)(&temporary_file, original.status.mode).is_err()
        || sync_file(&temporary_file).is_err()
    {
        return Err(CliError::WorkspaceMutationCommittedButUnverified);
    }
    if close_file(temporary_file).is_err() {
        return Err(CliError::WorkspaceMutationCommittedButUnverified);
    }
    let stored_contents = (operations.readback)(parent, name)
        .map_err(|_| CliError::WorkspaceMutationCommittedButUnverified)?;
    if stored_contents != contents {
        return Err(CliError::WorkspaceMutationCommittedButUnverified);
    }
    Ok(())
}

fn cleanup_failure<U>(
    cleanup: &mut U,
    parent: &DirHandle,
    temporary_name: &OsStr,
    identity: Option<&FileIdentity>,
    failure_message: &str,
) -> CliError
where
    U: FnMut(&DirHandle, &OsStr, Option<&FileIdentity>) -> io::Result<()>,
{
    if cleanup(parent, temporary_name, identity).is_ok() {
        CliError::new(failure_message)
    } else {
        CliError::new("replacement failed and staged file cleanup also failed")
    }
}

pub(super) fn read_file_contents_at_for_verification(
    parent: &DirHandle,
    name: &OsStr,
) -> Result<Vec<u8>, CliError> {
    let original_status =
        stat_at(parent, name).map_err(|_| CliError::new("unable to verify stored file"))?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }
    if original_status.size > MAX_FILE_SIZE as u64 {
        return Err(CliError::new("file exceeds 1 MiB"));
    }

    let file = open_at(parent, name, OpenRequest::ReadFile)
        .map_err(|_| CliError::new("unable to verify stored file"))?;
    let opened_status =
        fstat_file(&file).map_err(|_| CliError::new("unable to verify stored file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while verifying it"));
    }

    let mut contents = Vec::with_capacity(original_status.size as usize);
    file.take(MAX_FILE_SIZE as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CliError::new("unable to verify stored file"))?;
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("file exceeds 1 MiB"));
    }
    let current_status =
        stat_at(parent, name).map_err(|_| CliError::new("file changed while verifying it"))?;
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
        write_to_file,
        rename_file_at,
        read_file_contents_at_for_verification,
    )
}

pub(super) fn apply_patch_file_at_with_ops<W, C, R>(
    root: &WorkspaceRoot,
    path: &OsStr,
    patch_text: &str,
    writer: W,
    committer: C,
    readback: R,
) -> Result<PatchEvidence, CliError>
where
    W: FnMut(&mut File, &[u8]) -> io::Result<usize>,
    C: FnMut(&DirHandle, &OsStr, &OsStr) -> Result<(), RenameFileError>,
    R: FnMut(&DirHandle, &OsStr) -> Result<Vec<u8>, CliError>,
{
    if contains_nul(path) {
        return Err(CliError::new("invalid file path"));
    }
    if !is_relative_workspace_path(path) {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if Path::new(path)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CliError::new("traversal paths are not allowed"));
    }
    if path.is_empty() || path_has_trailing_separator(path) {
        return Err(CliError::new("invalid file path"));
    }
    if patch_text.len() > MAX_PATCH_BYTES {
        return Err(CliError::new("patch exceeds 32 KiB"));
    }

    let path_text = path
        .to_str()
        .ok_or_else(|| CliError::new("file path is not valid UTF-8"))?;
    let (parent, name) = open_workspace_parent(root, path)?;
    let original_status = stat_at(&parent, &name).map_err(|error| {
        if is_not_found(&error) {
            CliError::new("file not found")
        } else {
            CliError::from_io(&error, "invalid path")
        }
    })?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }

    let original_file = open_at(&parent, &name, OpenRequest::ReadFile)
        .map_err(|_| CliError::new("unable to open file"))?;
    let opened_status =
        fstat_file(&original_file).map_err(|_| CliError::new("unable to open file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while patching it"));
    }
    let original = read_open_text_file(original_file)?;
    let result =
        patch::apply_unified_diff(path_text, &original, patch_text).map_err(CliError::new)?;
    if result.contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("patched file exceeds 1 MiB"));
    }

    let mut operations = ReplacementOperations {
        writer,
        committer,
        readback,
        cleanup: unlink_created_file,
        mode_setter: set_mode,
    };
    replace_file_contents_atomically(
        &parent,
        &name,
        OriginalFileState {
            status: original_status,
            contents: original.as_bytes(),
        },
        result.contents.as_bytes(),
        &mut operations,
    )?;

    Ok(PatchEvidence {
        bytes_written: result.contents.len(),
        before_diff: result.before_diff,
        after_diff: result.after_diff,
    })
}
