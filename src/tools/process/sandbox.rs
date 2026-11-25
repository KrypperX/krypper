//! The Linux sandbox command: host executable lookup and the contained child command.

use super::policy::OfflineCargoPolicy;
use crate::tools::WorkspaceRoot;
use std::fs::OpenOptions;
use std::io::{self};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn host_command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    let uid = super::effective_uid();
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

// Reap on every error path too. A stuck kernel wait is never allowed to block the caller.

pub(super) fn sandbox(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    group: &Path,
) -> io::Result<Command> {
    let roots: [OwnedFd; 3] = [
        root.process_descriptor()?.into(),
        policy.toolchain.process_descriptor()?.into(),
        policy.registry.process_descriptor()?.into(),
    ];
    // Keep sources above destinations so dup2 cannot overwrite a later source descriptor.
    let mut pinned = Vec::new();
    for fd in roots {
        // SAFETY: `fd` owns a live descriptor for the whole call, and `F_DUPFD_CLOEXEC`
        // with a plain integer minimum has no other memory or ownership requirements.
        let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 203) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `duplicate` was checked to be nonnegative just above, so it is a fresh
        // descriptor that nothing else owns; the `OwnedFd` takes sole ownership.
        pinned.push(unsafe { OwnedFd::from_raw_fd(duplicate) });
    }
    let roots = pinned;
    let attach = OpenOptions::new()
        .write(true)
        .open(group.join("cgroup.procs"))?;
    let mut command = host_command("/usr/bin/bwrap");
    command.stderr(Stdio::piped()).args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--ro-bind",
        "/lib",
        "/lib",
        "--ro-bind",
        "/lib64",
        "/lib64",
        "--ro-bind-fd",
        "200",
        "/workspace",
        "--ro-bind-fd",
        "201",
        "/toolchain",
        "--size",
        "16777216",
        "--tmpfs",
        "/cargo",
        "--ro-bind-fd",
        "202",
        "/cargo/registry",
        "--proc",
        "/proc",
        "--remount-ro",
        "/proc",
        "--dev",
        "/dev",
        "--size",
        "1073741824",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/tmp/home",
        "--chdir",
        "/workspace",
        "--clearenv",
        "--setenv",
        "PATH",
        "/toolchain/bin:/usr/bin:/bin",
        "--setenv",
        "HOME",
        "/tmp/home",
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "CARGO_HOME",
        "/cargo",
        "--setenv",
        "CARGO_BUILD_JOBS",
        "1",
        "--setenv",
        "CARGO_PROFILE_DEV_DEBUG",
        "0",
        "--setenv",
        "CARGO_PROFILE_TEST_DEBUG",
        "0",
        "--setenv",
        "CARGO_INCREMENTAL",
        "0",
        "--setenv",
        "CARGO_TARGET_DIR",
        "/tmp/target",
        "--setenv",
        "RUSTC",
        "/toolchain/bin/rustc",
        "--setenv",
        "LANG",
        "C",
        "--remount-ro",
        "/",
    ]);
    // Pinned directory descriptors remove validation-to-mount pathname races; cgroup
    // attachment precedes sandbox exec.
    //
    // SAFETY: the closure runs between fork and exec, where only async-signal-safe calls are
    // allowed. It calls only `getpid`, `write`, `syscall(SYS_close_range)`, `dup2`,
    // `setrlimit` and `prctl`, all of which are async-signal-safe. It formats the pid into a
    // stack buffer, performs no heap allocation, takes no locks, and builds errors with
    // `io::Error::last_os_error`, which only reads `errno`. The descriptors it uses
    // (`attach`, `roots`) are moved into the closure and stay open until exec.
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(move || {
            let pid = libc::getpid();
            let mut digits = [0u8; 20];
            let mut n = pid as u32;
            let mut i = digits.len();
            loop {
                i -= 1;
                digits[i] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            if libc::write(
                attach.as_raw_fd(),
                digits[i..].as_ptr().cast(),
                digits.len() - i,
            ) != (digits.len() - i) as isize
            {
                return Err(io::Error::last_os_error());
            }
            // Do not leak any ambient non-CLOEXEC descriptor into the sandbox helper.
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            for (i, fd) in roots.iter().enumerate() {
                if libc::dup2(fd.as_raw_fd(), 200 + i as i32) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            for (resource, value) in [
                (libc::RLIMIT_NOFILE, 256),
                (libc::RLIMIT_FSIZE, 64 * 1024 * 1024),
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_CPU, 30),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}
