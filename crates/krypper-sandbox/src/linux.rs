//! Process supervision: leases, bounded pipes, reaping and the two run entry points.

use crate::host::{effective_uid, host_command};
use crate::sandbox::sandbox;
use crate::{RunLimits, SandboxOutcome, SandboxResource, SandboxRunRequest, SandboxRunResult};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod process;
mod recovery;

use process::{bounded_host, drain, nonblocking, reap, ManagedChild};
use recovery::{
    hold_for_recovery, is_cleanup_uncertain, mark_cleanup_uncertain, try_active_lock,
    ActiveLockError, HostLease, HostLeaseError,
};
const POST_EXIT_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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

fn record_cgroup_limit_events(group: &Path, events: &mut Vec<SandboxResource>) {
    for (name, field, resource) in [
        ("memory.events", "max", SandboxResource::Memory),
        ("pids.events", "max", SandboxResource::Processes),
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

pub(super) struct Supervisor {
    pub(super) unit: String,
    pub(super) group: PathBuf,
    pub(super) cleaned: bool,
    pub(super) limits: RunLimits,
}
impl Supervisor {
    fn create(id: Uuid, limits: RunLimits) -> io::Result<Self> {
        let unit = format!("krypper-test-{}.service", id.simple());
        let memory = format!("MemoryMax={}", limits.memory_bytes);
        let tasks = format!("TasksMax={}", limits.processes);
        let runtime_secs = limits.wall_millis.div_ceil(1_000).saturating_add(15);
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
        supervisor.group = control_group_path(Path::new(group.trim()))?;
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

fn control_group_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(io::Error::other("invalid cgroup identity"));
    }
    let relative = path
        .strip_prefix("/")
        .map_err(|_| io::Error::other("invalid cgroup identity"))?;
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

#[cfg(test)]
mod path_invariant_tests {
    use super::control_group_path;
    use std::path::Path;

    #[test]
    fn control_group_paths_must_be_absolute_and_contained() {
        assert_eq!(
            control_group_path(Path::new("/user.slice/krypper-test.service"))
                .expect("validated absolute cgroup path"),
            Path::new("/sys/fs/cgroup/user.slice/krypper-test.service")
        );
        assert!(control_group_path(Path::new("user.slice/krypper-test.service")).is_err());
        assert!(control_group_path(Path::new("/user.slice/../other.service")).is_err());
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if !self.clean() {
            mark_cleanup_uncertain();
        }
    }
}

pub(super) fn run(request: SandboxRunRequest) -> SandboxRunResult {
    run_with_containment(request, effective_uid() != 0)
}

#[cfg(test)]
fn run_without_containment(request: SandboxRunRequest) -> SandboxRunResult {
    run_with_containment(request, false)
}

fn run_with_containment(request: SandboxRunRequest, contained: bool) -> SandboxRunResult {
    let SandboxRunRequest {
        run_id,
        directories,
        test_filter,
        limits,
        cancelled,
    } = request;
    let Some(limits) = RunLimits::new(
        limits.wall_millis,
        limits.cpu_budget_usec,
        limits.memory_bytes,
        limits.output_bytes,
        limits.processes,
    )
    .ok() else {
        let mut result = SandboxRunResult::empty(run_id, SandboxOutcome::SandboxSetupFailed, 0);
        result.diagnostic = Some("invalid fixed offline Cargo limits".into());
        return result;
    };
    let output = limits.output_bytes;
    let mut result = SandboxRunResult::empty(run_id, SandboxOutcome::SandboxUnavailable, 0);
    if validate_test_filter(test_filter.as_deref()).is_err() {
        result.outcome = SandboxOutcome::InvalidTestFilter;
        result.diagnostic = Some("test_filter is invalid".into());
        return result;
    }
    if !contained {
        return result;
    }
    let Some(directories) = directories else {
        result.diagnostic = Some("captured sandbox directory descriptors unavailable".into());
        return result;
    };
    if !directories.all_are_directories() {
        result.outcome = SandboxOutcome::SandboxSetupFailed;
        result.diagnostic = Some("captured sandbox descriptor is not a directory".into());
        return result;
    }
    if is_cleanup_uncertain() {
        hold_for_recovery(&mut result, "previous process cleanup is unconfirmed");
        return result;
    }
    let _guard = match try_active_lock() {
        Ok(guard) => guard,
        Err(ActiveLockError::Busy) => {
            return SandboxRunResult::empty(run_id, SandboxOutcome::Denied, 0);
        }
        Err(ActiveLockError::Poisoned) => {
            hold_for_recovery(
                &mut result,
                "process supervisor lock is poisoned; restart to recheck the persisted cleanup receipt",
            );
            return result;
        }
    };
    if cancelled.load(Ordering::SeqCst) {
        result.outcome = SandboxOutcome::Cancelled;
        return result;
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
    let mut supervisor = match Supervisor::create(result.run_id, limits) {
        Ok(s) => s,
        Err(e) => {
            result.diagnostic = Some(e.to_string());
            result.cleanup_confirmed = !is_cleanup_uncertain();
            if result.cleanup_confirmed {
                let _ = lease.record("");
            } else {
                result.outcome = SandboxOutcome::OutcomeUnknown;
                result.recovery_hold = true;
            }
            return result;
        }
    };
    result.output_limit = output;
    let mut launched = false;
    let mut stop = StopReason::Unknown;
    let started = Instant::now();
    let execution = (|| -> io::Result<()> {
        lease.record(&format!(
            "{}\n{}\n",
            supervisor.unit,
            supervisor.group.display()
        ))?;
        let mut probe = sandbox(&directories, &supervisor.group)?;
        probe.args(["--", "/usr/bin/true"]);
        if !bounded_host(&mut probe)?.is_empty() {
            return Err(io::Error::other("unexpected sandbox output"));
        }
        supervisor.verify()?;
        if cancelled.load(Ordering::SeqCst) {
            settle_before_launch(&mut result, &mut supervisor, StopReason::Cancelled);
            return Ok(());
        }
        let mut command = sandbox(&directories, &supervisor.group)?;
        command.args(cargo_command_args(test_filter.as_deref()));
        result.outcome = SandboxOutcome::SpawnFailed;
        let mut child = ManagedChild::new(command.spawn()?);
        launched = true;
        let (mut out, mut err) = {
            let child = child.child_mut()?;
            let out = child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing stdout"))?;
            let err = child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("missing stderr"))?;
            (out, err)
        };
        nonblocking(&out)?;
        nonblocking(&err)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out_eof = false;
        let mut err_eof = false;
        let mut events = Vec::new();
        let mut cgroup_events_recorded = false;
        let mut output_drain_confirmed = true;
        let wall = Duration::from_millis(limits.wall_millis);
        let cpu_budget = limits.cpu_budget_usec;
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
            if cancelled.load(Ordering::SeqCst) {
                stop = StopReason::Cancelled;
                break;
            }
            if let Some(exit) = child.child_mut()?.try_wait()? {
                let cpu_budget_exceeded = cpu_usage_usec(&supervisor.group)? >= cpu_budget;
                record_cgroup_limit_events(&supervisor.group, &mut events);
                cgroup_events_recorded = true;
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
                        || cancelled.load(Ordering::SeqCst),
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
        if !cgroup_events_recorded {
            record_cgroup_limit_events(&supervisor.group, &mut events);
        }
        if matches!(stop, StopReason::Signaled(libc::SIGXCPU)) {
            record_event(&mut events, SandboxResource::Cpu);
        }
        if matches!(stop, StopReason::Signaled(libc::SIGXFSZ)) {
            record_event(&mut events, SandboxResource::FileSize);
        }
        result.cleanup_confirmed = supervisor.clean() && output_drain_confirmed;
        if !reap(child.child_mut()?) {
            result.cleanup_confirmed = false;
        }
        result.outcome = resolve_outcome(stop, &events, result.cleanup_confirmed);
        result.output_bytes = stdout.len().saturating_add(stderr.len());
        result.stdout = stdout;
        result.stderr = stderr;
        result.test_evidence_available = true;
        result.resource_events = events;
        Ok(())
    })();
    if let Err(e) = execution {
        result.diagnostic = Some(e.to_string());
        if launched {
            result.outcome = SandboxOutcome::OutcomeUnknown;
        } else if result.outcome == SandboxOutcome::SandboxUnavailable {
            result.outcome = SandboxOutcome::SandboxSetupFailed;
        }
    }
    result.cleanup_confirmed &= supervisor.clean();
    result.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if !result.cleanup_confirmed {
        mark_cleanup_uncertain();
        result.outcome = SandboxOutcome::OutcomeUnknown;
        result.recovery_hold = true;
    }
    if result.cleanup_confirmed {
        let _ = lease.record("");
    }
    result
}

fn settle_before_launch(
    result: &mut SandboxRunResult,
    supervisor: &mut Supervisor,
    stop: StopReason,
) {
    result.cleanup_confirmed = supervisor.clean();
    result.outcome = resolve_outcome(stop, &[], result.cleanup_confirmed);
}

fn cargo_command_args(test_filter: Option<&str>) -> Vec<std::ffi::OsString> {
    let mut args = [
        "--",
        "/toolchain/bin/cargo",
        "test",
        "--offline",
        "--locked",
        "--all-targets",
    ]
    .into_iter()
    .map(std::ffi::OsString::from)
    .collect::<Vec<_>>();
    if let Some(test_filter) = test_filter {
        args.push("--".into());
        args.push(test_filter.into());
    }
    args
}

fn validate_test_filter(test_filter: Option<&str>) -> Result<(), &'static str> {
    const MAX_TEST_FILTER_BYTES: usize = 256;
    let Some(test_filter) = test_filter else {
        return Ok(());
    };
    if test_filter.is_empty()
        || test_filter.len() > MAX_TEST_FILTER_BYTES
        || test_filter.chars().any(char::is_control)
        || test_filter.trim_start().starts_with('-')
    {
        return Err("test_filter is invalid");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    OutputLimit,
    Cancelled,
    WallTimeout,
    CpuBudget,
    Exited(i32),
    Signaled(i32),
    Unknown,
}

fn record_event(events: &mut Vec<SandboxResource>, resource: SandboxResource) {
    if events.contains(&resource) {
        return;
    }
    events.push(resource);
    events.sort_by_key(|resource| match resource {
        SandboxResource::Processes => 0,
        SandboxResource::Memory => 1,
        SandboxResource::Cpu => 2,
        SandboxResource::FileSize => 3,
    });
}

fn resolve_outcome(
    stop: StopReason,
    events: &[SandboxResource],
    cleanup_confirmed: bool,
) -> SandboxOutcome {
    if !cleanup_confirmed {
        return SandboxOutcome::OutcomeUnknown;
    }
    if let Some(resource) = events.first() {
        return SandboxOutcome::ResourceLimitExceeded {
            resource: *resource,
        };
    }
    match stop {
        StopReason::OutputLimit => SandboxOutcome::OutputLimitExceeded,
        StopReason::Cancelled => SandboxOutcome::Cancelled,
        StopReason::WallTimeout => SandboxOutcome::TimedOut,
        StopReason::CpuBudget => SandboxOutcome::ResourceLimitExceeded {
            resource: SandboxResource::Cpu,
        },
        StopReason::Exited(code) => SandboxOutcome::Exited(code),
        StopReason::Signaled(signal) => SandboxOutcome::Signaled(signal),
        StopReason::Unknown => SandboxOutcome::OutcomeUnknown,
    }
}

#[cfg(test)]
mod internal_outcome_tests {
    use super::{
        record_event, resolve_outcome, run_without_containment, SandboxRunRequest, StopReason,
    };
    use crate::{RunLimits, SandboxOutcome, SandboxResource};
    use std::fs::File;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn cgroup_readback_rejects_missing_and_unlimited_controls() {
        let path = std::env::temp_dir().join(format!("krypper-sandbox-cgroup-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let limits = RunLimits::offline_cargo_test();
        let supervisor = std::mem::ManuallyDrop::new(super::Supervisor {
            unit: "unused".into(),
            group: path.clone(),
            cleaned: true,
            limits,
        });
        assert!(supervisor.verify().is_err());
        for (name, value) in [
            ("memory.max", limits.memory_bytes.to_string()),
            ("memory.swap.max", "0".into()),
            ("pids.max", limits.processes.to_string()),
            ("cpu.max", "100000 100000".into()),
            ("cgroup.procs", "".into()),
            ("cgroup.kill", "".into()),
        ] {
            std::fs::write(path.join(name), value).unwrap();
        }
        assert!(supervisor.verify().is_ok());
        for name in ["memory.max", "pids.max", "cpu.max"] {
            let original = std::fs::read(path.join(name)).unwrap();
            std::fs::write(path.join(name), "max").unwrap();
            assert!(supervisor.verify().is_err(), "{name} must fail closed");
            std::fs::write(path.join(name), original).unwrap();
        }
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn cancellation_after_probe_resolves_before_any_child_launch() {
        let path = std::env::temp_dir().join(format!("krypper-sandbox-cancel-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let mut supervisor = super::Supervisor {
            unit: "unused".into(),
            group: path.clone(),
            cleaned: true,
            limits: RunLimits::offline_cargo_test(),
        };
        let mut result = crate::SandboxRunResult::empty(
            Uuid::new_v4(),
            SandboxOutcome::SandboxUnavailable,
            256 * 1024,
        );
        super::settle_before_launch(&mut result, &mut supervisor, StopReason::Cancelled);
        assert_eq!(result.outcome, SandboxOutcome::Cancelled);
        assert!(result.cleanup_confirmed);
        assert_eq!(result.output_bytes, 0);
        assert!(result.resource_events.is_empty());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn missing_containment_never_launches_workspace_code() {
        let workspace =
            std::env::temp_dir().join(format!("krypper-sandbox-uncontained-{}", Uuid::new_v4()));
        std::fs::create_dir(&workspace).unwrap();
        let marker = workspace.join("launched");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname=\"sandbox_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\nbuild=\"build.rs\"\n",
        ).unwrap();
        std::fs::write(
            workspace.join("build.rs"),
            format!(
                "fn main() {{ std::fs::write({:?}, b\"launched\").unwrap(); }}",
                marker
            ),
        )
        .unwrap();
        let directories = crate::DirectoryDescriptors::new(
            File::open(&workspace).unwrap(),
            File::open(&workspace).unwrap(),
            File::open(&workspace).unwrap(),
        );
        let request = SandboxRunRequest::new(
            Uuid::new_v4(),
            Some(directories),
            None,
            RunLimits::new(2_000, 1_000_000, 1024 * 1024, 8192, 1).unwrap(),
            Arc::new(AtomicBool::new(false)),
        );
        let result = run_without_containment(request);
        assert_eq!(result.outcome, SandboxOutcome::SandboxUnavailable);
        assert!(result.cleanup_confirmed);
        assert!(!marker.exists(), "no workspace code may launch");
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn busy_supervisor_keeps_the_unlaunched_result_shape() {
        let workspace =
            std::env::temp_dir().join(format!("krypper-sandbox-busy-{}", Uuid::new_v4()));
        std::fs::create_dir(&workspace).unwrap();
        let directories = crate::DirectoryDescriptors::new(
            File::open(&workspace).unwrap(),
            File::open(&workspace).unwrap(),
            File::open(&workspace).unwrap(),
        );
        let request = SandboxRunRequest::new(
            Uuid::new_v4(),
            Some(directories),
            None,
            RunLimits::offline_cargo_test(),
            Arc::new(AtomicBool::new(false)),
        );

        let result = {
            let _guard = super::recovery::try_active_lock().unwrap();
            super::run_with_containment(request, true)
        };

        assert_eq!(result.outcome, SandboxOutcome::Denied);
        assert_eq!(result.output_limit, 0);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn outcome_resolution_is_deterministic_for_limits_cancellation_and_cleanup() {
        use SandboxOutcome::*;
        let cases = [
            (StopReason::OutputLimit, vec![], true, OutputLimitExceeded),
            (StopReason::Cancelled, vec![], true, Cancelled),
            (StopReason::WallTimeout, vec![], true, TimedOut),
            (
                StopReason::CpuBudget,
                vec![],
                true,
                ResourceLimitExceeded {
                    resource: SandboxResource::Cpu,
                },
            ),
            (StopReason::Exited(0), vec![], true, Exited(0)),
            (StopReason::Exited(101), vec![], true, Exited(101)),
            (StopReason::Signaled(9), vec![], true, Signaled(9)),
            (StopReason::Unknown, vec![], true, OutcomeUnknown),
            (
                StopReason::WallTimeout,
                vec![SandboxResource::Memory],
                true,
                ResourceLimitExceeded {
                    resource: SandboxResource::Memory,
                },
            ),
            (
                StopReason::Cancelled,
                vec![SandboxResource::Processes],
                true,
                ResourceLimitExceeded {
                    resource: SandboxResource::Processes,
                },
            ),
            (
                StopReason::Exited(0),
                vec![SandboxResource::FileSize],
                true,
                ResourceLimitExceeded {
                    resource: SandboxResource::FileSize,
                },
            ),
            (StopReason::Exited(0), vec![], false, OutcomeUnknown),
            (
                StopReason::Cancelled,
                vec![SandboxResource::Cpu],
                false,
                OutcomeUnknown,
            ),
        ];
        for (stop, events, cleanup_confirmed, expected) in cases {
            assert_eq!(resolve_outcome(stop, &events, cleanup_confirmed), expected);
        }
    }

    #[test]
    fn kernel_limit_events_are_deduplicated_and_ordered() {
        let mut events = Vec::new();
        for resource in [
            SandboxResource::FileSize,
            SandboxResource::Processes,
            SandboxResource::Memory,
            SandboxResource::Cpu,
            SandboxResource::Memory,
        ] {
            record_event(&mut events, resource);
        }
        assert_eq!(
            events,
            [
                SandboxResource::Processes,
                SandboxResource::Memory,
                SandboxResource::Cpu,
                SandboxResource::FileSize
            ]
        );
        assert_eq!(
            resolve_outcome(StopReason::Exited(0), &events, true),
            SandboxOutcome::ResourceLimitExceeded {
                resource: SandboxResource::Processes
            }
        );
    }
}

#[cfg(test)]
mod post_exit_drain_tests {
    use super::process::nonblocking;
    use super::{drain_after_child_exit, PostExitDrain};
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
