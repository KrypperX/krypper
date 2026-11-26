//! The Linux sandbox command: host executable lookup and the contained child command.

use crate::runner_handshake::RunnerHandshakePipes;
use crate::runner_protocol::{ACK_FD, STATUS_FD};
use crate::{
    DirectoryDescriptors, PinnedResourceAccess, SandboxControl, SandboxIsolationEvidence,
    SandboxRunnerPin, ValidatedExecutionRequirements,
};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

const SECCOMP_FD: RawFd = 203;
const RUNNER_FD: RawFd = 206;
const FIRST_PINNED_FD: RawFd = 208;
const MAX_BPF_INSTRUCTIONS: usize = 4096;
const BPF_INSTRUCTION_BYTES: usize = 8;
const MFD_CLOEXEC: libc::c_uint = 0x0001;
const MFD_ALLOW_SEALING: libc::c_uint = 0x0002;

// Reap on every error path too. A stuck kernel wait is never allowed to block the caller.

pub(super) fn sandbox(
    directories: &DirectoryDescriptors,
    requirements: &ValidatedExecutionRequirements,
    group: &Path,
    filter: &[u8],
    runner: Option<&SandboxRunnerPin>,
    handshake: Option<&RunnerHandshakePipes>,
    evidence: &mut SandboxIsolationEvidence,
) -> io::Result<Command> {
    if handshake.is_some() && runner.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runner handshake requires the fixed sibling executable",
        ));
    }
    let filter_file = sealed_filter_fd(filter)?;
    let pinned_filter = duplicate_fd_at_or_above(filter_file.as_raw_fd(), FIRST_PINNED_FD)?;
    drop(filter_file);
    let roots = requirements
        .resources()
        .iter()
        .map(|resource| {
            if resource.access != PinnedResourceAccess::ReadOnly {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sandbox resource access is unsupported",
                ));
            }
            directories
                .for_resource(resource.id)
                .try_clone()
                .map(Into::into)
        })
        .collect::<io::Result<Vec<OwnedFd>>>()?;
    if roots.len() != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox resource roles do not match the fixed profile",
        ));
    }
    // Keep sources above destinations so dup2 cannot overwrite a later source descriptor.
    let mut pinned = Vec::new();
    for fd in roots {
        pinned.push(duplicate_fd_at_or_above(fd.as_raw_fd(), FIRST_PINNED_FD)?);
    }
    let roots = pinned;
    let pinned_runner = runner
        .map(|runner| duplicate_fd_at_or_above(runner.executable().as_raw_fd(), FIRST_PINNED_FD))
        .transpose()?;
    let pinned_status_writer = handshake
        .map(|pipes| duplicate_fd_at_or_above(pipes.status_writer()?.as_raw_fd(), FIRST_PINNED_FD))
        .transpose()?;
    let pinned_ack_reader = handshake
        .map(|pipes| {
            duplicate_fd_at_or_above(pipes.acknowledgement_reader()?.as_raw_fd(), FIRST_PINNED_FD)
        })
        .transpose()?;
    let attach = OpenOptions::new()
        .write(true)
        .open(group.join("cgroup.procs"))?;
    let mut command = crate::host::host_command("/usr/bin/bwrap");
    evidence.request(SandboxControl::SandboxRuntime);
    command.stderr(Stdio::piped());
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::UserNamespace,
        &["--unshare-user"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::PidNamespace,
        &["--unshare-pid"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::NetworkNamespace,
        &["--unshare-net"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::IpcNamespace,
        &["--unshare-ipc"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::UtsNamespace,
        &["--unshare-uts"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::CgroupNamespace,
        &["--unshare-cgroup"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::UserNamespace,
        &["--disable-userns", "--assert-userns-disabled"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ProcessLifecycle,
        &["--die-with-parent", "--new-session"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::SeccompFilter,
        &["--seccomp", "203"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::CapabilityDrop,
        &["--cap-drop", "ALL"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ReadOnlySystemMounts,
        &[
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
        ],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ReadOnlyWorkspace,
        &["--ro-bind-fd", "200", "/workspace"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ReadOnlyToolchain,
        &["--ro-bind-fd", "201", "/toolchain"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::TemporaryFilesystems,
        &["--size", "16777216", "--tmpfs", "/cargo"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ReadOnlyRegistry,
        &["--ro-bind-fd", "202", "/cargo/registry"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ProcMount,
        &["--proc", "/proc", "--remount-ro", "/proc"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::DeviceMount,
        &["--dev", "/dev"],
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::TemporaryFilesystems,
        &["--size", "1073741824", "--tmpfs", "/tmp"],
    );
    command.args([
        "--dir",
        "/tmp/home",
        "--dir",
        "/tmp/target",
        "--dir",
        "/tmp/tmp",
    ]);
    command.args([
        "--symlink",
        "/cargo/registry",
        "/tmp/home/registry",
        "--chdir",
        "/workspace",
    ]);
    if pinned_runner.is_some() {
        command.args(["--ro-bind-fd", "206", "/run/krypper-sandbox-runner"]);
        evidence.request(SandboxControl::LandlockFilesystemWrites);
    }
    let mut environment_args = vec!["--clearenv"];
    for (name, value) in crate::OFFLINE_CARGO_TEST_ENVIRONMENT {
        environment_args.extend(["--setenv", name, value]);
    }
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ClearedEnvironment,
        &environment_args,
    );
    push_control_args(
        &mut command,
        evidence,
        SandboxControl::ReadOnlyRoot,
        &["--remount-ro", "/"],
    );
    // Pinned directory descriptors remove validation-to-mount pathname races; cgroup
    // attachment precedes sandbox exec. Sources stay at or above 208; fixed child descriptors
    // 200-206 are populated only after close_range marks ambient descriptors close-on-exec.
    //
    // SAFETY: the closure runs between fork and exec, where only async-signal-safe calls are
    // allowed. It calls only `getpid`, `write`, `syscall(SYS_close_range)`, `dup2`,
    // `setrlimit` and `prctl`, all of which are async-signal-safe. It formats the pid into a
    // stack buffer, performs no heap allocation, takes no locks, and builds errors with
    // `io::Error::last_os_error`, which only reads `errno`. The descriptors it uses
    // (`attach`, `roots`, the compiled filter, and optional runner/handshake descriptors) are
    // moved into the closure and stay open until exec.
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
            if libc::dup2(pinned_filter.as_raw_fd(), SECCOMP_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(runner) = &pinned_runner {
                if libc::dup2(runner.as_raw_fd(), RUNNER_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(status_writer) = &pinned_status_writer {
                if libc::dup2(status_writer.as_raw_fd(), STATUS_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(ack_reader) = &pinned_ack_reader {
                if libc::dup2(ack_reader.as_raw_fd(), ACK_FD) < 0 {
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

fn push_control_args(
    command: &mut Command,
    evidence: &mut SandboxIsolationEvidence,
    control: SandboxControl,
    arguments: &[&str],
) {
    command.args(arguments);
    evidence.request(control);
}

fn sealed_filter_fd(filter: &[u8]) -> io::Result<OwnedFd> {
    if filter.is_empty()
        || !filter.len().is_multiple_of(BPF_INSTRUCTION_BYTES)
        || filter.len() / BPF_INSTRUCTION_BYTES > MAX_BPF_INSTRUCTIONS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compiled syscall filter has an invalid byte length",
        ));
    }

    let name = b"krypper-offline-cargo-seccomp\0";
    // SAFETY: `name` is NUL-terminated and lives through the syscall; the flags are fixed Linux
    // memfd flags and no pointer arguments are retained by the kernel.
    let raw_fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr().cast::<libc::c_char>(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a nonnegative memfd descriptor was returned above and is not owned elsewhere.
    let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd as RawFd) };
    file.write_all(filter)?;
    if unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file.into())
}

fn duplicate_fd_at_or_above(fd: RawFd, minimum: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `fd` remains owned by its caller for this operation; `F_DUPFD_CLOEXEC` only reads
    // the descriptor and returns a fresh descriptor at or above the fixed minimum.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duplicate` was checked to be nonnegative and is a fresh descriptor that nothing
    // else owns; the returned `OwnedFd` takes sole ownership.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

#[cfg(test)]
mod tests {
    use super::{
        duplicate_fd_at_or_above, sandbox, sealed_filter_fd, FIRST_PINNED_FD, RUNNER_FD, SECCOMP_FD,
    };
    use crate::{
        DirectoryDescriptors, SandboxBackend, SandboxControl, SandboxEvidenceStatus,
        SandboxIsolationEvidence, ValidatedExecutionRequirements,
    };
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use uuid::Uuid;

    #[test]
    fn filter_descriptor_is_sealed_pinned_and_ready_to_read() {
        let filter = sealed_filter_fd(&[0x06, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let current = unsafe { libc::lseek(filter.as_raw_fd(), 0, libc::SEEK_CUR) };
        assert_eq!(
            current, 0,
            "Bubblewrap must read the filter from its first byte"
        );
        let seals = unsafe { libc::fcntl(filter.as_raw_fd(), libc::F_GET_SEALS) };
        let expected =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & expected, expected);
        let pinned = duplicate_fd_at_or_above(filter.as_raw_fd(), FIRST_PINNED_FD).unwrap();
        assert!(pinned.as_raw_fd() >= FIRST_PINNED_FD);
    }

    #[test]
    fn malformed_filter_bytes_fail_without_building_a_sandbox_command() {
        assert!(sealed_filter_fd(&[]).is_err());
        assert!(sealed_filter_fd(&[0; 7]).is_err());
    }

    #[test]
    fn invalid_filter_source_descriptor_returns_an_error() {
        let error = duplicate_fd_at_or_above(-1, FIRST_PINNED_FD).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn bubblewrap_receives_the_filter_on_its_fixed_descriptor() {
        let root = std::env::temp_dir().join(format!("krypper-sandbox-command-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("toolchain")).unwrap();
        fs::create_dir_all(root.join("registry")).unwrap();
        let group = root.join("cgroup");
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cgroup.procs"), b"").unwrap();
        let directories = DirectoryDescriptors::new(
            File::open(root.join("workspace")).unwrap(),
            File::open(root.join("toolchain")).unwrap(),
            File::open(root.join("registry")).unwrap(),
        );
        let filter = [0x06, 0, 0, 0, 0, 0, 0, 0];
        let mut evidence = SandboxIsolationEvidence::new(Some(SandboxBackend::Bubblewrap));
        let command = sandbox(
            &directories,
            &ValidatedExecutionRequirements::offline_cargo_test(),
            &group,
            &filter,
            None,
            None,
            &mut evidence,
        )
        .unwrap();
        assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/bwrap");
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let seccomp = args
            .iter()
            .position(|argument| argument == "--seccomp")
            .unwrap();
        assert_eq!(args.get(seccomp + 1).map(String::as_str), Some("203"));
        assert_eq!(SECCOMP_FD, 203);
        assert_eq!(RUNNER_FD, 206);
        assert_eq!(FIRST_PINNED_FD, 208);
        assert!(evidence.controls().iter().any(|record| {
            record.control == SandboxControl::SandboxRuntime
                && record.status == SandboxEvidenceStatus::Requested
        }));
        for (control, argument) in [
            (SandboxControl::UserNamespace, "--unshare-user"),
            (SandboxControl::PidNamespace, "--unshare-pid"),
            (SandboxControl::NetworkNamespace, "--unshare-net"),
            (SandboxControl::IpcNamespace, "--unshare-ipc"),
            (SandboxControl::UtsNamespace, "--unshare-uts"),
            (SandboxControl::CgroupNamespace, "--unshare-cgroup"),
            (SandboxControl::SeccompFilter, "--seccomp"),
            (SandboxControl::CapabilityDrop, "--cap-drop"),
            (SandboxControl::ReadOnlySystemMounts, "/usr"),
            (SandboxControl::ReadOnlyWorkspace, "/workspace"),
            (SandboxControl::ReadOnlyToolchain, "/toolchain"),
            (SandboxControl::ReadOnlyRegistry, "/cargo/registry"),
            (SandboxControl::TemporaryFilesystems, "--tmpfs"),
            (SandboxControl::DeviceMount, "--dev"),
            (SandboxControl::ProcMount, "--proc"),
            (SandboxControl::ReadOnlyRoot, "/"),
            (SandboxControl::ClearedEnvironment, "--clearenv"),
            (SandboxControl::ProcessLifecycle, "--die-with-parent"),
        ] {
            assert!(args.iter().any(|value| value == argument));
            assert!(evidence.controls().iter().any(|record| {
                record.control == control && record.status == SandboxEvidenceStatus::Requested
            }));
        }
        assert!(!evidence
            .controls()
            .iter()
            .any(|record| record.status == SandboxEvidenceStatus::Observed));
        assert_eq!(evidence.controls().len(), 19);
        drop(command);
        fs::remove_dir_all(root).unwrap();
    }
}
