//! Process supervision: leases, bounded pipes, reaping and the two run entry points.

use super::policy::{
    cargo_command_args, validate_test_filter, OfflineCargoPolicy, CPU_BUDGET, MEMORY, OUTPUT,
    TASKS, WALL,
};
use super::sandbox::{host_command, sandbox};
use crate::tools::WorkspaceRoot;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

static ACTIVE: Mutex<()> = Mutex::new(());
static UNCERTAIN_CLEANUP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    Denied,
    InvalidTestFilter,
    NoTestsMatched,
    SandboxUnavailable,
    SandboxSetupFailed,
    SpawnFailed,
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Cancelled,
    ResourceLimitExceeded { resource: ProcessResource },
    OutputLimitExceeded,
    OutcomeUnknown,
}

/// The kernel-enforced resource whose limit prevented normal completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessResource {
    Cpu,
    Memory,
    Processes,
    FileSize,
}

/// Bounded bytes are decoded lossily. Renderers must escape terminal controls and treat
/// the text as untrusted evidence, never as instructions. No output is checkpointed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessResult {
    #[serde(serialize_with = "serialize_run_id")]
    pub run_id: Uuid,
    pub outcome: ProcessOutcome,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub diagnostic: Option<String>,
    pub cleanup_confirmed: bool,
    pub test_filter: Option<String>,
    pub tests_ran: bool,
    pub executed_tests: Option<usize>,
}

fn serialize_run_id<S: serde::Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

impl ProcessResult {
    pub(crate) fn denied(test_filter: Option<&str>) -> Self {
        Self::empty_with_filter(ProcessOutcome::Denied, test_filter)
    }
    pub(crate) fn invalid_test_filter() -> Self {
        let mut result = Self::empty(ProcessOutcome::InvalidTestFilter);
        result.diagnostic = Some("test_filter is invalid".into());
        result
    }
    pub(super) fn empty(outcome: ProcessOutcome) -> Self {
        Self::empty_with_filter(outcome, None)
    }
    pub(super) fn empty_with_filter(outcome: ProcessOutcome, test_filter: Option<&str>) -> Self {
        Self {
            run_id: Uuid::new_v4(),
            outcome,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            test_filter: test_filter.map(str::to_owned),
            tests_ran: false,
            executed_tests: None,
        }
    }
    pub fn succeeded(&self) -> bool {
        self.outcome == ProcessOutcome::Exited(0) && !self.truncated && self.cleanup_confirmed
    }
}

/// All system-manager commands have fixed executables, bounded output and a hard wait.
/// No caller-provided command, bus address, environment, or shell is accepted.

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
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
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

// One host-wide lease survives caller crashes. A later process must establish that the
// previous unit and its cgroup are empty before it may run another profile.
struct HostLease {
    _lock: File,
    receipt: PathBuf,
}
impl HostLease {
    fn acquire() -> io::Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let directory = PathBuf::from(format!("/run/user/{uid}"));
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other("untrusted runtime directory"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("krypper-offline-cargo.lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.len() > 4096
        {
            return Err(io::Error::other("untrusted execution lease"));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other("another offline profile is active"));
        }
        let mut lease = Self {
            _lock: file,
            receipt: directory.join("krypper-offline-cargo.receipt"),
        };
        let mut previous = String::new();
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lease.receipt)
        {
            Ok(mut receipt) => {
                let metadata = receipt.metadata()?;
                if !metadata.is_file()
                    || metadata.uid() != uid
                    || metadata.nlink() != 1
                    || metadata.len() > 4096
                {
                    return Err(io::Error::other("untrusted cleanup receipt"));
                }
                (&mut receipt).take(4097).read_to_string(&mut previous)?;
                if previous.is_empty() {
                    return Err(io::Error::other("incomplete cleanup receipt"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if !previous.is_empty() {
            let mut lines = previous.lines();
            let unit = lines.next().unwrap_or("");
            let id = unit
                .strip_prefix("krypper-test-")
                .and_then(|s| s.strip_suffix(".service"));
            if id.and_then(|s| Uuid::parse_str(s).ok()).is_none() {
                return Err(io::Error::other("unconfirmed execution lease"));
            }
            let state = bounded_host(host_command("/usr/bin/systemctl").args([
                "--user",
                "show",
                unit,
                "--property=ActiveState",
                "--value",
            ]))?;
            if !matches!(state.trim(), "inactive" | "failed") {
                return Err(io::Error::other("previous process cleanup is unconfirmed"));
            }
            if let Some(group) = lines.next() {
                let group = Path::new(group);
                if !group.starts_with("/sys/fs/cgroup")
                    || group.file_name().and_then(|s| s.to_str()) != Some(unit)
                    || group
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(io::Error::other("untrusted cleanup identity"));
                }
                if group.exists()
                    && !fs::read_to_string(group.join("cgroup.events"))?
                        .lines()
                        .any(|l| l == "populated 0")
                {
                    return Err(io::Error::other("previous cgroup is not empty"));
                }
            }
            lease.record("")?;
        }
        Ok(lease)
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

pub(super) struct Supervisor {
    pub(super) unit: String,
    pub(super) group: PathBuf,
    pub(super) cleaned: bool,
}
impl Supervisor {
    fn create(id: Uuid) -> io::Result<Self> {
        let unit = format!("krypper-test-{}.service", id.simple());
        // Only a trusted inert keeper starts here. No workspace code runs until verification.
        let creation = bounded_host(host_command("/usr/bin/systemd-run").args([
            "--user",
            "--quiet",
            "--collect",
            "--unit",
            &unit,
            "-p",
            "Delegate=yes",
            "-p",
            "MemoryMax=3221225472",
            "-p",
            "MemorySwapMax=0",
            "-p",
            "TasksMax=128",
            "-p",
            "CPUQuota=100%",
            "-p",
            "RuntimeMaxSec=195",
            "-p",
            "KillMode=control-group",
            "-p",
            "TimeoutStopSec=1",
            "-p",
            "SendSIGKILL=yes",
            "--",
            "/usr/bin/sleep",
            "195",
        ]));
        let mut supervisor = Self {
            unit,
            group: PathBuf::new(),
            cleaned: false,
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
        if memory != Some(MEMORY)
            || tasks != Some(TASKS)
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

pub(crate) fn run(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    run_with_bounds(root, policy, test_filter, WALL, OUTPUT, CPU_BUDGET)
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
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let wall = wall.min(WALL);
    let output = output.min(OUTPUT);
    let cpu_budget = cpu_budget.min(CPU_BUDGET);
    let mut result =
        ProcessResult::empty_with_filter(ProcessOutcome::SandboxUnavailable, test_filter);
    if !cfg!(target_os = "linux") || unsafe { libc::geteuid() } == 0 {
        return result;
    }
    if UNCERTAIN_CLEANUP.load(Ordering::SeqCst) {
        result.outcome = ProcessOutcome::OutcomeUnknown;
        return result;
    }
    let Ok(_guard) = ACTIVE.try_lock() else {
        return ProcessResult::empty_with_filter(ProcessOutcome::Denied, test_filter);
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
        Err(e) => {
            result.diagnostic = Some(e.to_string());
            return result;
        }
    };
    let unit = format!("krypper-test-{}.service", result.run_id.simple());
    if lease.record(&format!("{unit}\n")).is_err() {
        return result;
    }
    let mut supervisor = match Supervisor::create(result.run_id) {
        Ok(s) => s,
        Err(e) => {
            result.diagnostic = Some(e.to_string());
            result.cleanup_confirmed = !UNCERTAIN_CLEANUP.load(Ordering::SeqCst);
            if result.cleanup_confirmed {
                let _ = lease.record("");
            } else {
                result.outcome = ProcessOutcome::OutcomeUnknown;
            }
            return result;
        }
    };
    let mut launched = false;
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
            result.outcome = ProcessOutcome::Cancelled;
            return Ok(());
        }
        let mut command = sandbox(root, policy, &supervisor.group)?;
        command.args(cargo_command_args(test_filter));
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
        let start = Instant::now();
        let mut status: Option<ExitStatus> = None;
        let mut out_eof = false;
        let mut err_eof = false;
        loop {
            let remaining = output.saturating_sub(stdout.len() + stderr.len());
            out_eof |= drain(&mut out, &mut stdout, remaining)?;
            let remaining = output.saturating_sub(stdout.len() + stderr.len());
            err_eof |= drain(&mut err, &mut stderr, remaining)?;
            if stdout.len() + stderr.len() >= output {
                result.truncated = true;
                result.outcome = ProcessOutcome::OutputLimitExceeded;
                break;
            }
            if policy.cancelled.load(Ordering::SeqCst) {
                result.outcome = ProcessOutcome::Cancelled;
                break;
            }
            if start.elapsed() >= wall {
                result.outcome = ProcessOutcome::TimedOut;
                break;
            }
            let cpu = fs::read_to_string(supervisor.group.join("cpu.stat"))?;
            let usage = cpu
                .lines()
                .find_map(|line| {
                    line.strip_prefix("usage_usec ")
                        .and_then(|s| s.parse::<u64>().ok())
                })
                .ok_or_else(|| io::Error::other("CPU accounting unavailable"))?;
            if usage >= cpu_budget {
                result.outcome = ProcessOutcome::ResourceLimitExceeded {
                    resource: ProcessResource::Cpu,
                };
                break;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(exit) = status {
                if out_eof && err_eof {
                    result.outcome = if let Some(code) = exit.code() {
                        ProcessOutcome::Exited(code)
                    } else {
                        ProcessOutcome::Signaled(exit.signal().unwrap_or(0))
                    };
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        // Check kernel counters before removing the group. A killed process with a quota event
        // must not be confused with an ordinary exit or an invented successful result.
        for (name, field, resource) in [
            ("memory.events", "max", ProcessResource::Memory),
            ("pids.events", "max", ProcessResource::Processes),
        ] {
            if fs::read_to_string(supervisor.group.join(name)).is_ok_and(|s| {
                s.lines().any(|l| {
                    l.split_once(' ')
                        .is_some_and(|(k, v)| k == field && v.parse::<u64>().is_ok_and(|n| n > 0))
                })
            }) {
                result.outcome = ProcessOutcome::ResourceLimitExceeded { resource };
            }
        }
        if matches!(result.outcome, ProcessOutcome::Signaled(libc::SIGXCPU)) {
            result.outcome = ProcessOutcome::ResourceLimitExceeded {
                resource: ProcessResource::Cpu,
            };
        }
        if matches!(result.outcome, ProcessOutcome::Signaled(libc::SIGXFSZ)) {
            result.outcome = ProcessOutcome::ResourceLimitExceeded {
                resource: ProcessResource::FileSize,
            };
        }
        result.cleanup_confirmed = supervisor.clean();
        // cgroup.kill terminates bwrap as well as descendants; reap our direct child.
        if !reap(&mut child) {
            result.cleanup_confirmed = false;
        }
        result.stdout = String::from_utf8_lossy(&stdout).into_owned();
        result.stderr = String::from_utf8_lossy(&stderr).into_owned();
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
    if !result.cleanup_confirmed {
        UNCERTAIN_CLEANUP.store(true, Ordering::SeqCst);
        result.outcome = ProcessOutcome::OutcomeUnknown;
    }
    if result.cleanup_confirmed {
        let _ = lease.record("");
    }
    result
}
