//! Optional filesystem write confinement for the fixed offline Cargo runner.
//!
//! This is applied inside Bubblewrap, before Cargo is executed. The Bwrap mounts remain the
//! primary boundary; Landlock limits writes to prepared Cargo and temporary work directories.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

const LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const LANDLOCK_ADD_RULE: libc::c_long = 445;
const LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const CREATE_RULESET_VERSION: libc::c_uint = 1;
const RULE_PATH_BENEATH: libc::c_uint = 1;
const MAX_SUPPORTED_ABI: i32 = 12;
const MIN_REQUIRED_ABI: i32 = 3;

const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

const WRITE_ACCESS_ABI_1: u64 = ACCESS_FS_WRITE_FILE
    | ACCESS_FS_REMOVE_DIR
    | ACCESS_FS_REMOVE_FILE
    | ACCESS_FS_MAKE_CHAR
    | ACCESS_FS_MAKE_DIR
    | ACCESS_FS_MAKE_REG
    | ACCESS_FS_MAKE_SOCK
    | ACCESS_FS_MAKE_FIFO
    | ACCESS_FS_MAKE_BLOCK
    | ACCESS_FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LandlockStatus {
    Applied { abi: i32 },
    Unavailable { abi: Option<i32> },
}

/// Query and apply the fixed write allowlist. Only a genuinely unavailable or too-old/unknown
/// ABI returns `Unavailable`; any setup error after a compatible ABI was found is fatal.
pub(crate) fn apply_fixed_write_policy() -> io::Result<LandlockStatus> {
    let abi = match query_abi() {
        Ok(abi) => abi,
        Err(error) if abi_unavailable(&error) => {
            return Ok(LandlockStatus::Unavailable { abi: None });
        }
        Err(error) => return Err(error),
    };

    if !(MIN_REQUIRED_ABI..=MAX_SUPPORTED_ABI).contains(&abi) {
        return Ok(LandlockStatus::Unavailable { abi: Some(abi) });
    }

    // ABI 5 adds device ioctls, which can mutate state outside ordinary file writes. Keep that
    // permission restricted to the same scratch roots. ABI 9+ rights for pathname socket
    // resolution and network/capability/namespace controls are outside this filesystem-write
    // policy; Bubblewrap's fixed network and namespace controls remain authoritative there.
    let mut handled_access_fs = WRITE_ACCESS_ABI_1 | ACCESS_FS_REFER;
    if abi >= 3 {
        handled_access_fs |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        handled_access_fs |= ACCESS_FS_IOCTL_DEV;
    }
    let ruleset_attr = RulesetAttr { handled_access_fs };
    let ruleset_fd = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            std::ptr::addr_of!(ruleset_attr).cast::<libc::c_void>(),
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if ruleset_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let ruleset = unsafe { std::fs::File::from_raw_fd(ruleset_fd as RawFd) };

    for scratch in ["/tmp/home", "/tmp/target", "/tmp/tmp", "/cargo"] {
        add_write_rule(ruleset.as_raw_fd(), scratch, handled_access_fs)?;
    }

    let result = unsafe { libc::syscall(LANDLOCK_RESTRICT_SELF, ruleset.as_raw_fd(), 0u32) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(LandlockStatus::Applied { abi })
}

fn query_abi() -> io::Result<i32> {
    let result = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        i32::try_from(result).map_err(|_| io::Error::other("Landlock ABI value overflow"))
    }
}

fn add_write_rule(ruleset_fd: RawFd, path: &str, access: u64) -> io::Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let rule = PathBeneathAttr {
        allowed_access: access,
        parent_fd: directory.as_raw_fd(),
    };
    let result = unsafe {
        libc::syscall(
            LANDLOCK_ADD_RULE,
            ruleset_fd,
            RULE_PATH_BENEATH,
            std::ptr::addr_of!(rule).cast::<libc::c_void>(),
            0u32,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn abi_unavailable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL)
    )
}

#[cfg(test)]
mod tests {
    use super::{abi_unavailable, PathBeneathAttr, WRITE_ACCESS_ABI_1};
    use std::io;

    #[test]
    fn write_policy_covers_file_creation_removal_reparenting_and_truncation() {
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
        assert_eq!(std::mem::align_of::<PathBeneathAttr>(), 1);
        assert_ne!(WRITE_ACCESS_ABI_1 & (1 << 1), 0);
        assert_ne!(WRITE_ACCESS_ABI_1 & (1 << 4), 0);
        assert_ne!(WRITE_ACCESS_ABI_1 & (1 << 5), 0);
        assert_ne!(WRITE_ACCESS_ABI_1 & (1 << 12), 0);
        assert_ne!(super::ACCESS_FS_REFER, 0);
        assert_ne!(super::ACCESS_FS_TRUNCATE, 0);
        assert_ne!(super::ACCESS_FS_IOCTL_DEV, 0);
    }

    #[test]
    fn only_kernel_unavailable_probe_errors_are_optional() {
        assert!(abi_unavailable(&io::Error::from_raw_os_error(libc::ENOSYS)));
        assert!(abi_unavailable(&io::Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));
        assert!(abi_unavailable(&io::Error::from_raw_os_error(libc::EINVAL)));
        assert!(!abi_unavailable(&io::Error::from_raw_os_error(libc::EPERM)));
    }
}
