//! Compile-only Windows backend until native containment is implemented.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

use super::super::syscalls::{OpenRequest, RenameFileError};
use super::super::types::{CreateMode, DirHandle, FileIdentity, FileStatus};

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "workspace operations are unavailable on this platform",
    )
}

pub(crate) fn encode_workspace_path(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide().flat_map(u16::to_le_bytes).collect()
}

pub(crate) fn open_current_directory() -> io::Result<DirHandle> {
    Err(unavailable())
}

pub(crate) fn open_directory_path(_path: &Path) -> io::Result<DirHandle> {
    Err(unavailable())
}

pub(crate) fn duplicate_directory(_directory: &DirHandle) -> io::Result<DirHandle> {
    Err(unavailable())
}

pub(crate) fn open_at(
    _parent: &DirHandle,
    _name: &OsStr,
    _request: OpenRequest,
) -> io::Result<File> {
    Err(unavailable())
}

pub(crate) fn stat_at(_parent: &DirHandle, _name: &OsStr) -> io::Result<FileStatus> {
    Err(unavailable())
}

pub(crate) fn fstat_file(_file: &File) -> io::Result<FileStatus> {
    Err(unavailable())
}

pub(crate) fn try_lock_file_exclusive(_file: &File) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn read_link_at(_parent: &DirHandle, _name: &OsStr) -> io::Result<std::path::PathBuf> {
    Err(unavailable())
}

pub(crate) fn rename_file_at(
    _parent: &DirHandle,
    _temporary_name: &OsStr,
    _name: &OsStr,
) -> Result<(), RenameFileError> {
    Err(RenameFileError::NotCommitted)
}

pub(crate) fn unlink_created_file(
    _parent: &DirHandle,
    _name: &OsStr,
    _identity: Option<&FileIdentity>,
) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn unlink_at(_parent: &DirHandle, _name: &OsStr) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn mkdir_at(_parent: &DirHandle, _name: &OsStr, _mode: CreateMode) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn set_mode(_file: &File, _mode: CreateMode) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn sync_file(_file: &File) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn sync_directory(_directory: &DirHandle) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) fn close_file(_file: File) -> io::Result<()> {
    Err(unavailable())
}

pub(crate) struct DirectoryStream;

impl DirectoryStream {
    pub(crate) fn open(_directory: File) -> io::Result<Self> {
        Err(unavailable())
    }

    pub(crate) fn next_entry(&mut self) -> io::Result<Option<(std::ffi::OsString, Vec<u32>)>> {
        Err(unavailable())
    }
}

pub(crate) fn is_symlink_loop(_error: &io::Error) -> bool {
    false
}

pub(crate) fn contains_nul(path: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    path.encode_wide().any(|code_unit| code_unit == 0)
}

pub(crate) fn path_has_trailing_separator(path: &OsStr) -> bool {
    path.to_string_lossy()
        .chars()
        .last()
        .is_some_and(|character| matches!(character, '/' | '\\'))
}

#[cfg(test)]
mod tests {
    use super::open_current_directory;

    #[test]
    fn workspace_backend_reports_unavailable_without_opening_a_root() {
        let error = open_current_directory().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    use std::io;
}
