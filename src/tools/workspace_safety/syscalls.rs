//! Thin wrappers over the POSIX calls used by workspace safety.
//!
//! Every `unsafe` block in workspace safety lives here. Each function performs
//! one syscall and converts its result; none of them makes a containment or
//! authorisation decision. Which flags and paths are acceptable is decided by
//! the caller in `workspace_safety.rs`.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

pub(super) fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub(super) fn same_inode(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

pub(super) fn file_type(status: &libc::stat) -> libc::mode_t {
    status.st_mode & libc::S_IFMT
}

pub(super) fn is_symlink(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFLNK
}

pub(super) fn is_directory(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFDIR
}

pub(super) fn is_regular_file(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFREG
}

pub(super) fn same_file_snapshot(left: &libc::stat, right: &libc::stat) -> bool {
    same_inode(left, right)
        && left.st_mode == right.st_mode
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

pub(super) fn split_components(path: &[u8]) -> VecDeque<OsString> {
    path.split(|byte| *byte == b'/')
        .map(|part| OsString::from_vec(part.to_vec()))
        .collect()
}

pub(super) fn prepend_components(pending: &mut VecDeque<OsString>, path: &[u8]) {
    for component in split_components(path).into_iter().rev() {
        pending.push_front(component);
    }
}

pub(super) fn owned_fd_or_error(descriptor: RawFd) -> io::Result<OwnedFd> {
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative descriptor returned by `open`/`openat` is newly owned by this function.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

pub(super) fn open_at(
    parent_fd: RawFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    // SAFETY: `parent_fd` is an open directory descriptor and `name` is a NUL-terminated component.
    let descriptor = unsafe { libc::openat(parent_fd, name.as_ptr(), flags, mode) };
    owned_fd_or_error(descriptor)
}

pub(super) fn open_current_directory() -> io::Result<OwnedFd> {
    let path = CString::new(".").expect("invariant: a dot path contains no NUL byte");
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd_or_error(descriptor)
}

pub(super) fn open_directory_path(path: &Path) -> io::Result<OwnedFd> {
    let path = c_string(path.as_os_str())?;
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd_or_error(descriptor)
}

pub(super) fn duplicate_fd(descriptor: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `descriptor` is live for the duration of this call.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
    owned_fd_or_error(duplicate)
}

pub(super) fn stat_at(parent_fd: RawFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_string(name)?;
    // SAFETY: the output points to initialized writable storage, and the parent/name arguments remain valid.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `parent_fd` is open, `name` is NUL-terminated, and `status` is writable.
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn fstat(descriptor: RawFd) -> io::Result<libc::stat> {
    // SAFETY: the output points to initialized writable storage and `descriptor` is a live file descriptor.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `descriptor` is live and `status` is writable for the syscall.
    if unsafe { libc::fstat(descriptor, &mut status) } == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn read_link_at(parent_fd: RawFd, name: &OsStr) -> io::Result<Vec<u8>> {
    let name = c_string(name)?;
    let mut capacity = 256;
    loop {
        let mut target = vec![0_u8; capacity];
        // SAFETY: `target` is writable for `capacity` bytes, and the path arguments are valid during the call.
        let length = unsafe {
            libc::readlinkat(
                parent_fd,
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
            return Ok(target);
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|length| *length <= 1024 * 1024)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "symlink target is too long")
            })?;
    }
}

pub(super) fn write_to_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` points to a readable buffer for `bytes.len()` bytes and `fd` is open for writing.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

pub(super) fn write_all_to_fd<F>(fd: RawFd, contents: &[u8], writer: &mut F) -> io::Result<()>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let mut offset = 0;
    while offset < contents.len() {
        match writer(fd, &contents[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "zero-byte write")),
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => return Err(io::Error::other("writer reported an invalid byte count")),
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn rename_file_at(
    parent_fd: RawFd,
    temporary_name: &OsStr,
    name: &OsStr,
) -> io::Result<()> {
    let temporary_name = c_string(temporary_name)?;
    let name = c_string(name)?;
    // SAFETY: both names are NUL-terminated single path components and `parent_fd` is an open directory.
    if unsafe { libc::renameat(parent_fd, temporary_name.as_ptr(), parent_fd, name.as_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `parent_fd` is an open directory containing the committed rename.
    if unsafe { libc::fsync(parent_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn unlink_created_file(parent_fd: RawFd, name: &OsStr, identity: Option<&libc::stat>) {
    let Some(identity) = identity else {
        return;
    };
    let Ok(current) = stat_at(parent_fd, name) else {
        return;
    };
    let Ok(name) = c_string(name) else {
        return;
    };
    if same_inode(identity, &current) {
        // SAFETY: `parent_fd` is open, `name` is a single component, and the inode still matches the temporary file created above.
        unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
    }
}

pub(super) fn unlink_at(parent_fd: RawFd, name: &OsStr) -> io::Result<()> {
    let name = c_string(name)?;
    // SAFETY: `parent_fd` is an open directory and `name` is a NUL-terminated single component.
    if unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn mkdir_at(parent_fd: RawFd, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    let name = c_string(name)?;
    // SAFETY: `parent_fd` is an open directory and `name` is a NUL-terminated single component.
    if unsafe { libc::mkdirat(parent_fd, name.as_ptr(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn set_mode(descriptor: RawFd, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `descriptor` is a live file descriptor opened for this operation.
    if unsafe { libc::fchmod(descriptor, mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn sync_fd(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: `descriptor` is a live file descriptor.
    if unsafe { libc::fsync(descriptor) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn close_fd(descriptor: RawFd) -> io::Result<()> {
    // SAFETY: `descriptor` is open and this call is its only close.
    if unsafe { libc::close(descriptor) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    pub(super) fn open(directory: File) -> io::Result<Self> {
        let raw = duplicate_fd(directory.as_raw_fd())?.into_raw_fd();
        // SAFETY: `raw` is a valid, independently owned directory descriptor; `fdopendir` takes ownership on success.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            // SAFETY: `fdopendir` did not take ownership when it returned null.
            unsafe { libc::close(raw) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self(stream))
    }

    pub(super) fn next_entry(&mut self) -> io::Result<Option<Vec<u8>>> {
        // SAFETY: `__errno_location` returns this thread's errno slot on Linux.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `self.0` owns a live DIR pointer until its Drop implementation closes it.
        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            // SAFETY: Reading the thread-local errno slot is valid after a failed `readdir` call.
            if unsafe { *libc::__errno_location() } != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(None);
        }
        // SAFETY: `d_name` is NUL-terminated by `readdir`, and is copied before the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        Ok(Some(name.to_vec()))
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the live DIR pointer returned by `fdopendir`.
        unsafe { libc::closedir(self.0) };
    }
}
