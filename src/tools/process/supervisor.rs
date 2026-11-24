//! Process supervision: leases, bounded pipes, reaping and the two run entry points.

use super::outcome::{record_event, resolve_outcome, ProcessOutcome, ProcessResource, StopReason};
use super::policy::{cargo_command_args, validate_test_filter, OfflineCargoPolicy};
use super::result::ProcessResult;
use super::sandbox::{host_command, sandbox};
use crate::tools::process_profile::ProfileLimits;
use crate::tools::WorkspaceRoot;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

static ACTIVE: Mutex<()> = Mutex::new(());
static UNCERTAIN_CLEANUP: AtomicBool = AtomicBool::new(false);
const POST_EXIT_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

// All system-manager commands have fixed executables, bounded output and a hard wait.
// No caller-provided command, bus address, environment, or shell is accepted.

struct ManagedChild(Option<std::process::Child>);
impl std::ops::Deref for ManagedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
fn reap(child: &mut std::process::Child) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            _ => {}
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = child.kill();
            if !reap(&mut child) {
                // Keep the wait handle until the kernel permits reaping; never claim cleanup.
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

fn bounded_host(command: &mut Command) -> io::Result<String> {
    let mut child = ManagedChild(Some(command.spawn()?));
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing pipe"))?;
    nonblocking(&stdout)?;
    let mut stderr = child.stderr.take();
    if let Some(pipe) = &stderr {
        nonblocking(pipe)?;
    }
    let mut errors = Vec::new();
    let start = Instant::now();
    let mut output = Vec::new();
    loop {
        let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
        drain(&mut stdout, &mut output, remaining)?;
        if let Some(pipe) = &mut stderr {
            let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
            drain(pipe, &mut errors, remaining)?;
        }
        if output.len() + errors.len() >= 16 * 1024 || start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = reap(&mut child);
            return Err(io::Error::other("host command exceeded bound"));
        }
        if let Some(status) = child.try_wait()? {
            let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
            drain(&mut stdout, &mut output, remaining)?;
            if !status.success() {
                if let Some(pipe) = &mut stderr {
                    let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
                    drain(pipe, &mut errors, remaining)?;
                }
                return Err(io::Error::other(format!(
                    "host command failed: {}",
                    String::from_utf8_lossy(&errors[..errors.len().min(512)])
                )));
            }
            return String::from_utf8(output).map_err(io::Error::other);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: `pipe` is borrowed for the whole function, so its descriptor stays open;
    // `F_GETFL`/`F_SETFL` take and return plain integers and touch no memory.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        // SAFETY: same descriptor and same reasoning as the `F_GETFL` call above.
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Return true only at EOF. Read at most one bounded chunk per stream per poll.
fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>, remaining: usize) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    let count = remaining.min(buffer.len());
    if count == 0 {
        return Ok(false);
    }
    match pipe.read(&mut buffer[..count]) {
        Ok(0) => Ok(true),
        Ok(n) => {
            bytes.extend_from_slice(&buffer[..n]);
            Ok(false)
        }
        Err(e)
            if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostExitDrain {
    Complete,
    OutputLimit,
    Cancelled,
    DeadlineExpired,
}

fn drain_after_child_exit(
    stdout_pipe: &mut impl Read,
    stderr_pipe: &mut impl Read,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    output_limit: usize,
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
) -> io::Result<PostExitDrain> {
    let started = Instant::now();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        stdout_eof |= drain(stdout_pipe, stdout, remaining)?;
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        stderr_eof |= drain(stderr_pipe, stderr, remaining)?;
        if stdout.len() + stderr.len() >= output_limit {
            return Ok(PostExitDrain::OutputLimit);
        }
        if cancelled() {
            return Ok(PostExitDrain::Cancelled);
        }
        if stdout_eof && stderr_eof {
            return Ok(PostExitDrain::Complete);
        }
        if started.elapsed() >= timeout {
            return Ok(PostExitDrain::DeadlineExpired);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn record_cgroup_limit_events(group: &Path, events: &mut Vec<ProcessResource>) {
    for (name, field, resource) in [
        ("memory.events", "max", ProcessResource::Memory),
        ("pids.events", "max", ProcessResource::Processes),
    ] {
        if fs::read_to_string(group.join(name)).is_ok_and(|contents| {
            contents.lines().any(|line| {
                line.split_once(' ').is_some_and(|(key, value)| {
                    key == field && value.parse::<u64>().is_ok_and(|count| count > 0)
                })
            })
        }) {
            record_event(events, resource);
        }
    }
}

fn cpu_usage_usec(group: &Path) -> io::Result<u64> {
    fs::read_to_string(group.join("cpu.stat"))?
        .lines()
        .find_map(|line| {
            line.strip_prefix("usage_usec ")
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| io::Error::other("CPU accounting unavailable"))
}

// One host-wide lease survives caller crashes. A later process must establish that the
// previous unit and its cgroup are empty before it may run another profile.
struct HostLease {
    _lock: File,
    receipt: PathBuf,
}

#[derive(Debug)]
enum HostLeaseError {
    Unavailable(io::Error),
    CleanupUnconfirmed(io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviousUnitState {
    NotLoaded,
    Stopped,
}

impl HostLease {
    fn acquire() -> Result<Self, HostLeaseError> {
        let uid = super::effective_uid();
        let directory = PathBuf::from(format!("/run/user/{uid}"));
        let metadata = fs::symlink_metadata(&directory).map_err(HostLeaseError::Unavailable)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "untrusted runtime directory",
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("krypper-offline-cargo.lock"))
            .map_err(HostLeaseError::Unavailable)?;
        let metadata = file.metadata().map_err(HostLeaseError::Unavailable)?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.len() > 4096
        {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "untrusted execution lease",
            )));
        }
        // SAFETY: `file` owns a live descriptor for the duration of the call, and `flock`
        // takes only that descriptor and an integer operation.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "another offline profile is active",
            )));
        }
        let mut lease = Self {
            _lock: file,
            receipt: directory.join("krypper-offline-cargo.receipt"),
        };
        lease.recover_previous()?;
        Ok(lease)
    }

    fn recover_previous(&mut self) -> Result<(), HostLeaseError> {
        let uid = super::effective_uid();
        let mut previous = String::new();
        let receipt = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.receipt)
        {
            Ok(mut receipt) => {
                let metadata = receipt
                    .metadata()
                    .map_err(HostLeaseError::CleanupUnconfirmed)?;
                if !metadata.is_file()
                    || metadata.uid() != uid
                    || metadata.nlink() != 1
                    || metadata.len() > 4096
                {
                    return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                        "untrusted cleanup receipt",
                    )));
                }
                (&mut receipt)
                    .take(4097)
                    .read_to_string(&mut previous)
                    .map_err(HostLeaseError::CleanupUnconfirmed)?;
                receipt
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(HostLeaseError::CleanupUnconfirmed(error)),
        };
        if previous.is_empty() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "incomplete cleanup receipt",
            )));
        }

        let mut lines = previous.lines();
        let unit = lines.next().unwrap_or("");
        let id = unit
            .strip_prefix("krypper-test-")
            .and_then(|s| s.strip_suffix(".service"));
        if id.and_then(|s| Uuid::parse_str(s).ok()).is_none() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "unconfirmed execution lease",
            )));
        }
        let group = lines.next();
        if lines.next().is_some() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "invalid cleanup receipt",
            )));
        }

        let state = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            unit,
            "--property=LoadState,ActiveState",
        ]))
        .map_err(HostLeaseError::CleanupUnconfirmed)?;
        parse_previous_unit_state(&state).map_err(HostLeaseError::CleanupUnconfirmed)?;
        if let Some(group) = group {
            let group = Path::new(group);
            if !group.starts_with("/sys/fs/cgroup")
                || group.file_name().and_then(|s| s.to_str()) != Some(unit)
                || group
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                    "untrusted cleanup identity",
                )));
            }
            if group.exists()
                && !fs::read_to_string(group.join("cgroup.events"))
                    .map_err(HostLeaseError::CleanupUnconfirmed)?
                    .lines()
                    .any(|l| l == "populated 0")
            {
                return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                    "previous cgroup is not empty",
                )));
            }
        }
        drop(receipt);
        self.record("").map_err(HostLeaseError::CleanupUnconfirmed)
    }
    fn record(&mut self, text: &str) -> io::Result<()> {
        let directory = self.receipt.parent().unwrap();
        if text.is_empty() {
            match fs::remove_file(&self.receipt) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        } else {
            let temporary = directory.join(format!(".krypper-cargo-{}", Uuid::new_v4()));
            let update = (|| -> io::Result<()> {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&temporary)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                fs::rename(&temporary, &self.receipt)
            })();
            if update.is_err() {
                let _ = fs::remove_file(temporary);
            }
            update?;
        }
        File::open(directory)?.sync_all()
    }
}

fn parse_previous_unit_state(output: &str) -> io::Result<PreviousUnitState> {
    let mut load_state = None;
    let mut active_state = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(io::Error::other("invalid previous systemd unit state"));
        };
        match key {
            "LoadState" if load_state.replace(value).is_none() => {}
            "ActiveState" if active_state.replace(value).is_none() => {}
            _ => return Err(io::Error::other("invalid previous systemd unit state")),
        }
    }

    match (load_state, active_state) {
        (Some("not-found"), Some("inactive")) => Ok(PreviousUnitState::NotLoaded),
        (Some("loaded"), Some("inactive" | "failed")) => Ok(PreviousUnitState::Stopped),
        _ => Err(io::Error::other("previous process cleanup is unconfirmed")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveLockError {
    Busy,
    Poisoned,
}

fn try_active_lock(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, ActiveLockError> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(ActiveLockError::Busy),
        Err(TryLockError::Poisoned(_)) => Err(ActiveLockError::Poisoned),
    }
}

fn hold_for_recovery(result: &mut ProcessResult, diagnostic: impl Into<String>) {
    result.outcome = ProcessOutcome::OutcomeUnknown;
    result.cleanup_confirmed = false;
    result.recovery_hold = true;
    result.diagnostic = Some(diagnostic.into());
}

pub(super) struct Supervisor {
    pub(super) unit: String,
    pub(super) group: PathBuf,
    pub(super) cleaned: bool,
    pub(super) limits: ProfileLimits,
}
impl Supervisor {
    fn create(id: Uuid, limits: ProfileLimits) -> io::Result<Self> {
        let unit = format!("krypper-test-{}.service", id.simple());
        let memory = format!("MemoryMax={}", limits.memory_bytes);
        let tasks = format!("TasksMax={}", limits.processes);
        let runtime_secs = limits.wall_secs.saturating_add(15);
        let runtime = format!("RuntimeMaxSec={runtime_secs}");
        let keepalive = runtime_secs.to_string();
        // Only a trusted inert keeper starts here. No workspace code runs until verification.
        let mut command = host_command("/usr/bin/systemd-run");
        command.args([
            "--user",
            "--quiet",
            "--collect",
            "--unit",
            &unit,
            "-p",
            "Delegate=yes",
        ]);
        command.args(["-p", memory.as_str(), "-p", "MemorySwapMax=0"]);
        command.args(["-p", tasks.as_str()]);
        let creation = bounded_host(command.args([
            "-p",
            "CPUQuota=100%",
            "-p",
            runtime.as_str(),
            "-p",
            "KillMode=control-group",
            "-p",
            "TimeoutStopSec=1",
            "-p",
            "SendSIGKILL=yes",
            "--",
            "/usr/bin/sleep",
            keepalive.as_str(),
        ]));
        let mut supervisor = Self {
            unit,
            group: PathBuf::new(),
            cleaned: false,
            limits,
        };
        creation?;
        let group = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            &supervisor.unit,
            "--property=ControlGroup",
            "--value",
        ]))?;
        let path = Path::new(group.trim());
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(io::Error::other("invalid cgroup identity"));
        }
        supervisor.group = Path::new("/sys/fs/cgroup").join(path.strip_prefix("/").unwrap());
        supervisor.verify()?;
        let membership = fs::read_to_string("/proc/thread-self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| io::Error::other("cgroup v2 membership unavailable"))?;
        let current = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        let common = supervisor
            .group
            .ancestors()
            .find(|ancestor| current.starts_with(ancestor))
            .ok_or_else(|| io::Error::other("cgroup attachment ancestry unavailable"))?;
        OpenOptions::new()
            .write(true)
            .open(common.join("cgroup.procs"))
            .map_err(|_| {
                io::Error::other(
                    "caller must run inside the writable delegated user-manager cgroup subtree",
                )
            })?;
        Ok(supervisor)
    }
    pub(super) fn verify(&self) -> io::Result<()> {
        let read = |name| fs::read_to_string(self.group.join(name));
        let memory = read("memory.max")?.trim().parse::<u64>().ok();
        let tasks = read("pids.max")?.trim().parse::<u64>().ok();
        let cpu = read("cpu.max")?;
        let fields = cpu
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>();
        if memory != Some(self.limits.memory_bytes)
            || tasks != Some(self.limits.processes)
            || read("memory.swap.max")?.trim() != "0"
            || !matches!(fields, Ok(ref v) if v.len() == 2 && v[0] > 0 && v[1] > 0 && v[0] <= v[1])
        {
            return Err(io::Error::other(
                "required hard resource controls unavailable",
            ));
        }
        // Both attach and whole-tree kill must be delegated before launch.
        OpenOptions::new()
            .write(true)
            .open(self.group.join("cgroup.procs"))?;
        OpenOptions::new()
            .write(true)
            .open(self.group.join("cgroup.kill"))?;
        Ok(())
    }
    fn clean(&mut self) -> bool {
        if self.cleaned {
            return true;
        }
        if !self.group.as_os_str().is_empty() && self.group.exists() {
            // The manager can still terminate the unit when the caller's cgroup mount is
            // read-only. Success still requires independent empty-tree readback below.
            let _ = fs::write(self.group.join("cgroup.kill"), b"1");
        }
        let _ =
            bounded_host(host_command("/usr/bin/systemctl").args(["--user", "stop", &self.unit]));
        let inactive = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            &self.unit,
            "--property=ActiveState",
            "--value",
        ]))
        .is_ok_and(|s| matches!(s.trim(), "inactive" | "failed"));
        let start = Instant::now();
        while self.group.exists() && start.elapsed() < Duration::from_secs(3) {
            if fs::read_to_string(self.group.join("cgroup.events"))
                .is_ok_and(|s| s.lines().any(|l| l == "populated 0"))
            {
                self.cleaned = inactive;
                return self.cleaned;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // A removed cgroup cannot contain live processes. Also require manager acknowledgement.
        self.cleaned = inactive && !self.group.exists();
        self.cleaned
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if !self.clean() {
            UNCERTAIN_CLEANUP.store(true, Ordering::SeqCst);
        }
    }
}

fn executed_test_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            line.strip_prefix("test ")
                .is_some_and(|test| test.ends_with(" ... ok") || test.ends_with(" ... FAILED"))
        })
        .count()
}

pub(super) fn record_test_evidence(result: &mut ProcessResult) {
    result.executed_tests = Some(
        executed_test_count(&result.stdout).saturating_add(executed_test_count(&result.stderr)),
    );
    result.tests_ran = result.executed_tests.is_some_and(|count| count > 0);
    if result.test_filter.is_some()
        && result.outcome == ProcessOutcome::Exited(0)
        && !result.tests_ran
    {
        result.outcome = ProcessOutcome::NoTestsMatched;
        result.diagnostic = Some("test filter executed no tests".into());
    }
}

pub(super) fn record_raw_output(result: &mut ProcessResult, stdout: Vec<u8>, stderr: Vec<u8>) {
    result.output_bytes = stdout.len().saturating_add(stderr.len());
    result.stdout = String::from_utf8_lossy(&stdout).into_owned();
    result.stderr = String::from_utf8_lossy(&stderr).into_owned();
}

pub(super) fn settle_before_launch(
    result: &mut ProcessResult,
    supervisor: &mut Supervisor,
    stop: StopReason,
) {
    result.cleanup_confirmed = supervisor.clean();
    result.outcome = resolve_outcome(stop, &[], result.cleanup_confirmed);
}

pub(crate) fn run(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    let limits = policy.profile().limits();
    run_with_bounds(
        root,
        policy,
        test_filter,
        Duration::from_secs(limits.wall_secs),
        limits.output_bytes,
        limits.cpu_ms.saturating_mul(1_000),
    )
}

// Private lower-only bounds let host fixtures exercise failures without minute-long waits.
pub(super) fn run_with_bounds(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget: u64,
) -> ProcessResult {
    run_with_containment(
        root,
        policy,
        test_filter,
        wall,
        output,
        cpu_budget,
        cfg!(target_os = "linux") && super::effective_uid() != 0,
    )
}

#[cfg(test)]
pub(super) fn run_without_containment(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget: u64,
) -> ProcessResult {
    run_with_containment(root, policy, test_filter, wall, output, cpu_budget, false)
}

fn run_with_containment(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget: u64,
    contained: bool,
) -> ProcessResult {
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let limits = policy.profile().limits();
    let wall = wall.min(Duration::from_secs(limits.wall_secs));
    let output = output.min(limits.output_bytes);
    let cpu_budget = cpu_budget.min(limits.cpu_ms.saturating_mul(1_000));
    let mut result =
        ProcessResult::empty_with_filter(ProcessOutcome::SandboxUnavailable, test_filter);
    if !contained {
        return result;
    }
    if UNCERTAIN_CLEANUP.load(Ordering::SeqCst) {
        hold_for_recovery(&mut result, "previous process cleanup is unconfirmed");
        return result;
    }
    let _guard = match try_active_lock(&ACTIVE) {
        Ok(guard) => guard,
        Err(ActiveLockError::Busy) => {
            return ProcessResult::empty_with_filter(ProcessOutcome::Denied, test_filter);
        }
        Err(ActiveLockError::Poisoned) => {
            hold_for_recovery(
                &mut result,
                "process supervisor lock is poisoned; restart to recheck the persisted cleanup receipt",
            );
            return result;
        }
    };
    if policy.cancelled.load(Ordering::SeqCst) {
        result.outcome = ProcessOutcome::Cancelled;
        return result;
    }
    // A task must never be able to edit its own trusted runtime or registry mounts.
    for runtime in [&policy.toolchain, &policy.registry] {
        if runtime.path().starts_with(root.path()) || root.path().starts_with(runtime.path()) {
            return ProcessResult::denied(test_filter);
        }
    }
    let mut lease = match HostLease::acquire() {
        Ok(lease) => lease,
        Err(HostLeaseError::Unavailable(error)) => {
            result.diagnostic = Some(error.to_string());
            return result;
        }
        Err(HostLeaseError::CleanupUnconfirmed(error)) => {
            hold_for_recovery(&mut result, error.to_string());
            return result;
        }
    };
    let unit = format!("krypper-test-{}.service", result.run_id.simple());
    if lease.record(&format!("{unit}\n")).is_err() {
        return result;
    }
    let mut supervisor = match Supervisor::create(result.run_id, policy.profile().limits()) {
        Ok(s) => s,
        Err(e) => {
            result.diagnostic = Some(e.to_string());
            result.cleanup_confirmed = !UNCERTAIN_CLEANUP.load(Ordering::SeqCst);
            if result.cleanup_confirmed {
                let _ = lease.record("");
            } else {
                result.outcome = ProcessOutcome::OutcomeUnknown;
                result.recovery_hold = true;
            }
            return result;
        }
    };
    let mut launched = false;
    let mut stop = StopReason::Unknown;
    let started = Instant::now();
    let execution = (|| -> io::Result<()> {
        lease.record(&format!(
            "{}\n{}\n",
            supervisor.unit,
            supervisor.group.display()
        ))?;
        let mut probe = sandbox(root, policy, &supervisor.group)?;
        probe.args(["--", "/usr/bin/true"]);
        if !bounded_host(&mut probe)?.is_empty() {
            return Err(io::Error::other("unexpected sandbox output"));
        }
        supervisor.verify()?;
        if policy.cancelled.load(Ordering::SeqCst) {
            settle_before_launch(&mut result, &mut supervisor, StopReason::Cancelled);
            return Ok(());
        }
        let mut command = sandbox(root, policy, &supervisor.group)?;
        command.args(cargo_command_args(policy.profile(), test_filter));
        result.outcome = ProcessOutcome::SpawnFailed;
        let mut child = ManagedChild(Some(command.spawn()?));
        launched = true;
        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut err = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        nonblocking(&out)?;
        nonblocking(&err)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out_eof = false;
        let mut err_eof = false;
        let mut events = Vec::new();
        let mut cgroup_events_recorded = false;
        let mut output_drain_confirmed = true;
        loop {
            let remaining = output.saturating_sub(stdout.len() + stderr.len());
            out_eof |= drain(&mut out, &mut stdout, remaining)?;
            let remaining = output.saturating_sub(stdout.len() + stderr.len());
            err_eof |= drain(&mut err, &mut stderr, remaining)?;
            if stdout.len() + stderr.len() >= output {
                result.truncated = true;
                stop = StopReason::OutputLimit;
                break;
            }
            if policy.cancelled.load(Ordering::SeqCst) {
                stop = StopReason::Cancelled;
                break;
            }
            if let Some(exit) = child.try_wait()? {
                let cpu_budget_exceeded = cpu_usage_usec(&supervisor.group)? >= cpu_budget;
                // Read kernel counters before cleanup removes the cgroup files.
                record_cgroup_limit_events(&supervisor.group, &mut events);
                cgroup_events_recorded = true;
                // A descendant can keep the inherited pipe handles open after the leader exits.
                // Kill the whole cgroup first, then wait only for a bounded pipe drain.
                let _ = supervisor.clean();
                let post_exit_drain = if out_eof && err_eof {
                    PostExitDrain::Complete
                } else {
                    drain_after_child_exit(
                        &mut out,
                        &mut err,
                        &mut stdout,
                        &mut stderr,
                        output,
                        POST_EXIT_PIPE_DRAIN_TIMEOUT,
                        || policy.cancelled.load(Ordering::SeqCst),
                    )?
                };
                stop = match post_exit_drain {
                    PostExitDrain::Complete => {
                        if cpu_budget_exceeded {
                            StopReason::CpuBudget
                        } else if let Some(code) = exit.code() {
                            StopReason::Exited(code)
                        } else {
                            StopReason::Signaled(exit.signal().unwrap_or(0))
                        }
                    }
                    PostExitDrain::OutputLimit => {
                        result.truncated = true;
                        StopReason::OutputLimit
                    }
                    PostExitDrain::Cancelled => StopReason::Cancelled,
                    PostExitDrain::DeadlineExpired => {
                        result.truncated = true;
                        output_drain_confirmed = false;
                        result.diagnostic =
                            Some("output pipes remained open after bounded cgroup cleanup".into());
                        StopReason::Unknown
                    }
                };
                break;
            }
            if started.elapsed() >= wall {
                stop = StopReason::WallTimeout;
                break;
            }
            if cpu_usage_usec(&supervisor.group)? >= cpu_budget {
                stop = StopReason::CpuBudget;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        // Check kernel counters before removing the group. A killed process with a quota event
        // must not be confused with an ordinary exit or an invented successful result.
        if !cgroup_events_recorded {
            record_cgroup_limit_events(&supervisor.group, &mut events);
        }
        if matches!(stop, StopReason::Signaled(libc::SIGXCPU)) {
            record_event(&mut events, ProcessResource::Cpu);
        }
        if matches!(stop, StopReason::Signaled(libc::SIGXFSZ)) {
            record_event(&mut events, ProcessResource::FileSize);
        }
        result.cleanup_confirmed = supervisor.clean() && output_drain_confirmed;
        // cgroup.kill terminates bwrap as well as descendants; reap our direct child.
        if !reap(&mut child) {
            result.cleanup_confirmed = false;
        }
        result.outcome = resolve_outcome(stop, &events, result.cleanup_confirmed);
        record_raw_output(&mut result, stdout, stderr);
        result.resource_events = events;
        record_test_evidence(&mut result);
        Ok(())
    })();
    if let Err(e) = execution {
        result.diagnostic = Some(e.to_string());
        if launched {
            result.outcome = ProcessOutcome::OutcomeUnknown;
        } else if result.outcome == ProcessOutcome::SandboxUnavailable {
            result.outcome = ProcessOutcome::SandboxSetupFailed;
        }
    }
    result.cleanup_confirmed &= supervisor.clean();
    result.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    result.output_limit = output;
    if !result.cleanup_confirmed {
        UNCERTAIN_CLEANUP.store(true, Ordering::SeqCst);
        result.outcome = ProcessOutcome::OutcomeUnknown;
        result.recovery_hold = true;
    }
    if result.cleanup_confirmed {
        let _ = lease.record("");
    }
    result
}

#[cfg(test)]
mod recovery_state_tests {
    use super::{parse_previous_unit_state, try_active_lock, ActiveLockError, PreviousUnitState};
    use std::sync::Mutex;

    #[test]
    fn previous_unit_state_distinguishes_not_loaded_from_unknown() {
        assert_eq!(
            parse_previous_unit_state("LoadState=not-found\nActiveState=inactive\n").unwrap(),
            PreviousUnitState::NotLoaded
        );
        assert_eq!(
            parse_previous_unit_state("LoadState=loaded\nActiveState=inactive\n").unwrap(),
            PreviousUnitState::Stopped
        );
        assert_eq!(
            parse_previous_unit_state("LoadState=loaded\nActiveState=failed\n").unwrap(),
            PreviousUnitState::Stopped
        );

        for unknown in [
            "LoadState=error\nActiveState=inactive\n",
            "LoadState=not-found\nActiveState=active\n",
            "LoadState=loaded\nActiveState=active\n",
            "LoadState=loaded\nActiveState=activating\n",
            "ActiveState=inactive\n",
            "LoadState=loaded\nActiveState=inactive\nExtra=value\n",
        ] {
            assert!(
                parse_previous_unit_state(unknown).is_err(),
                "unrecognized state must retain the recovery hold: {unknown:?}"
            );
        }
    }

    #[test]
    fn active_lock_reports_busy_and_poisoned_as_different_states() {
        let busy = Mutex::new(());
        let _guard = busy.lock().unwrap();
        assert!(matches!(try_active_lock(&busy), Err(ActiveLockError::Busy)));

        let poisoned = Mutex::new(());
        std::thread::scope(|scope| {
            let join = scope.spawn(|| {
                let _guard = poisoned.lock().unwrap();
                panic!("simulate a supervisor panic while holding the lock");
            });
            assert!(join.join().is_err());
        });
        assert!(matches!(
            try_active_lock(&poisoned),
            Err(ActiveLockError::Poisoned)
        ));
    }
}

#[cfg(test)]
mod post_exit_drain_tests {
    use super::{drain_after_child_exit, nonblocking, PostExitDrain};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct ProcessGroup(libc::pid_t);

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            // SAFETY: this test created the process group with the child PID as its ID.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    #[test]
    fn inherited_pipe_writer_does_not_extend_the_post_exit_deadline() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "(sleep 30) & printf 'leader output\\n'; exit 0"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let _process_group = ProcessGroup(child.id() as libc::pid_t);
        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        nonblocking(&stdout_pipe).unwrap();
        nonblocking(&stderr_pipe).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(0));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let started = Instant::now();
        let drain = drain_after_child_exit(
            &mut stdout_pipe,
            &mut stderr_pipe,
            &mut stdout,
            &mut stderr,
            1024,
            Duration::from_millis(100),
            || false,
        )
        .unwrap();

        assert_eq!(drain, PostExitDrain::DeadlineExpired);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(stdout, b"leader output\n");

        // SAFETY: this test created the isolated group and its sleeping descendant.
        let killed = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        assert_eq!(killed, 0);
        assert_eq!(
            drain_after_child_exit(
                &mut stdout_pipe,
                &mut stderr_pipe,
                &mut stdout,
                &mut stderr,
                1024,
                Duration::from_secs(1),
                || false,
            )
            .unwrap(),
            PostExitDrain::Complete
        );
    }
}
