//! POSIX workspace backend for Linux and macOS.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::super::syscalls::OpenRequest;
use super::super::types::{CreateMode, DirHandle, FileIdentity, FileKind, FileStatus};

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub(crate) fn encode_workspace_path(path: &OsStr) -> Vec<u8> {
    path.as_bytes().to_vec()
}

fn owned_file_or_error(descriptor: RawFd) -> io::Result<File> {
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative descriptor returned by open/openat is newly owned here.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

pub(crate) fn open_current_directory() -> io::Result<DirHandle> {
    let path = CString::new(".").expect("invariant: a dot path contains no NUL byte");
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_file_or_error(descriptor).map(DirHandle::from_file)
}

pub(crate) fn open_directory_path(path: &Path) -> io::Result<DirHandle> {
    let path = c_string(path.as_os_str())?;
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_file_or_error(descriptor).map(DirHandle::from_file)
}

pub(crate) fn duplicate_directory(directory: &DirHandle) -> io::Result<DirHandle> {
    let name = CString::new(".").expect("invariant: a dot path contains no NUL byte");
    // SAFETY: `directory` is an open directory and `name` is a live NUL-terminated dot path.
    let descriptor = unsafe {
        libc::openat(
            directory.file().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    owned_file_or_error(descriptor).map(DirHandle::from_file)
}

pub(crate) fn open_at(parent: &DirHandle, name: &OsStr, request: OpenRequest) -> io::Result<File> {
    let name = c_string(name)?;
    let (flags, mode) = match request {
        OpenRequest::ReadFile => (
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0,
        ),
        OpenRequest::ReadDirectory => (
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ),
        OpenRequest::CreateFile(mode) => (
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode.bits()
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file mode"))?,
        ),
    };
    // SAFETY: `parent` is an open directory and `name` is a NUL-terminated component.
    let descriptor = unsafe { libc::openat(parent.file().as_raw_fd(), name.as_ptr(), flags, mode) };
    owned_file_or_error(descriptor)
}

#[allow(clippy::unnecessary_cast)] // Native stat field widths differ on macOS.
fn file_status(status: &libc::stat) -> FileStatus {
    let file_type = status.st_mode & libc::S_IFMT;
    let kind = if file_type == libc::S_IFDIR {
        FileKind::Directory
    } else if file_type == libc::S_IFREG {
        FileKind::Regular
    } else if file_type == libc::S_IFLNK {
        FileKind::Symlink
    } else {
        FileKind::Other
    };

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let (modified_seconds, modified_nanos, changed_seconds, changed_nanos) = (
        status.st_mtime,
        status.st_mtime_nsec,
        status.st_ctime,
        status.st_ctime_nsec,
    );

    FileStatus {
        identity: FileIdentity {
            volume: status.st_dev as u64,
            file: status.st_ino as u64,
        },
        kind,
        mode: CreateMode::from_bits((status.st_mode & 0o777) as u32),
        size: status.st_size as u64,
        modified_seconds: modified_seconds as i64,
        modified_nanos: modified_nanos as i64,
        changed_seconds: changed_seconds as i64,
        changed_nanos: changed_nanos as i64,
    }
}

fn stat_descriptor(descriptor: RawFd) -> io::Result<FileStatus> {
    // SAFETY: the output points to initialized writable storage and `descriptor` is live.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `descriptor` is open and `status` is writable for the call.
    if unsafe { libc::fstat(descriptor, &mut status) } == 0 {
        Ok(file_status(&status))
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn stat_at(parent: &DirHandle, name: &OsStr) -> io::Result<FileStatus> {
    let name = c_string(name)?;
    // SAFETY: the output is writable, the directory remains open, and `name` is NUL-terminated.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `parent` is open, `name` is valid, and `AT_SYMLINK_NOFOLLOW` classifies the entry itself.
    let result = unsafe {
        libc::fstatat(
            parent.file().as_raw_fd(),
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(file_status(&status))
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn fstat_file(file: &File) -> io::Result<FileStatus> {
    stat_descriptor(file.as_raw_fd())
}

pub(crate) fn read_link_at(parent: &DirHandle, name: &OsStr) -> io::Result<PathBuf> {
    let name = c_string(name)?;
    let mut capacity = 256;
    loop {
        let mut target = vec![0_u8; capacity];
        // SAFETY: `target` is writable for `capacity` bytes and both path arguments stay live.
        let length = unsafe {
            libc::readlinkat(
                parent.file().as_raw_fd(),
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
            return Ok(PathBuf::from(OsString::from_vec(target)));
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|length| *length <= 1024 * 1024)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "symlink target is too long")
            })?;
    }
}

pub(crate) fn rename_file_at(
    parent: &DirHandle,
    temporary_name: &OsStr,
    name: &OsStr,
) -> io::Result<()> {
    let temporary_name = c_string(temporary_name)?;
    let name = c_string(name)?;
    // SAFETY: both names are NUL-terminated components and `parent` is an open directory.
    if unsafe {
        libc::renameat(
            parent.file().as_raw_fd(),
            temporary_name.as_ptr(),
            parent.file().as_raw_fd(),
            name.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    sync_directory(parent)
}

pub(crate) fn unlink_created_file(
    parent: &DirHandle,
    name: &OsStr,
    identity: Option<&FileIdentity>,
) {
    let Some(identity) = identity else {
        return;
    };
    let Ok(current) = stat_at(parent, name) else {
        return;
    };
    if &current.identity != identity {
        return;
    }
    let Ok(name) = c_string(name) else {
        return;
    };
    // SAFETY: `parent` is open, `name` is a single component, and the current file identity matches.
    unsafe { libc::unlinkat(parent.file().as_raw_fd(), name.as_ptr(), 0) };
}

pub(crate) fn unlink_at(parent: &DirHandle, name: &OsStr) -> io::Result<()> {
    let name = c_string(name)?;
    // SAFETY: `parent` is an open directory and `name` is a NUL-terminated component.
    if unsafe { libc::unlinkat(parent.file().as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[allow(clippy::useless_conversion)] // Validate the mode_t width on platforms where it is narrower.
pub(crate) fn mkdir_at(parent: &DirHandle, name: &OsStr, mode: CreateMode) -> io::Result<()> {
    let name = c_string(name)?;
    let mode = mode
        .bits()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory mode"))?;
    // SAFETY: `parent` is an open directory and `name` is a NUL-terminated component.
    if unsafe { libc::mkdirat(parent.file().as_raw_fd(), name.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[allow(clippy::useless_conversion)] // Validate the mode_t width on platforms where it is narrower.
pub(crate) fn set_mode(file: &File, mode: CreateMode) -> io::Result<()> {
    let mode = mode
        .bits()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file mode"))?;
    // SAFETY: `file` is live and the checked mode value fits the platform mode type.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: F_FULLFSYNC takes the open file descriptor and no additional argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `file` is live and fsync takes one descriptor.
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn sync_directory(directory: &DirHandle) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: F_FULLFSYNC takes the open directory descriptor and no additional argument.
        if unsafe { libc::fcntl(directory.file().as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `directory` is an open directory descriptor and fsync takes one descriptor.
        if unsafe { libc::fsync(directory.file().as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn close_file(file: File) -> io::Result<()> {
    let descriptor = file.into_raw_fd();
    // SAFETY: ownership was transferred from `file` immediately above and this is its only close.
    if unsafe { libc::close(descriptor) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    pub(crate) fn open(directory: File) -> io::Result<Self> {
        let raw = directory.into_raw_fd();
        // SAFETY: ownership of this live descriptor was transferred from `directory`; fdopendir
        // takes ownership on success, and the error branch below closes it otherwise.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            // SAFETY: fdopendir did not take ownership when it returned null.
            unsafe { libc::close(raw) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self(stream))
    }

    pub(crate) fn next_entry(&mut self) -> io::Result<Option<(OsString, Vec<u32>)>> {
        // SAFETY: this selects the calling thread's errno slot for readdir's end/error distinction.
        unsafe { *errno_slot() = 0 };
        // SAFETY: `self.0` owns a live DIR until Drop.
        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            // SAFETY: errno is read immediately after readdir returns null.
            if unsafe { *errno_slot() } != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(None);
        }
        // SAFETY: readdir returns a NUL-terminated name valid until the next call; copy it now.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes()
            .to_vec();
        let name = OsString::from_vec(bytes.clone());
        Ok(Some((name, python_codepoints(&bytes))))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the live DIR pointer returned by fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_slot() -> *mut libc::c_int {
    // SAFETY: libc returns this thread's errno slot on Linux.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_slot() -> *mut libc::c_int {
    // SAFETY: libc returns this thread's errno slot on macOS.
    unsafe { libc::__error() }
}

pub(crate) fn is_symlink_loop(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

pub(crate) fn contains_nul(path: &OsStr) -> bool {
    path.as_bytes().contains(&0)
}

pub(crate) fn path_has_trailing_separator(path: &OsStr) -> bool {
    path.as_bytes().last() == Some(&b'/')
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
                        .expect("invariant: valid_up_to identifies valid UTF-8");
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
