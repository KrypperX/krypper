//! Linux payload child supervision and bounded output collection.

use super::cgroup::{cpu_usage_usec, record_cgroup_limit_events, record_event, Supervisor};
use super::outcome::{resolve_outcome, StopReason};
use super::process::{
    drain, drain_after_child_exit, nonblocking, reap, ManagedChild, PostExitDrain,
};
use crate::{RunLimits, SandboxOutcome, SandboxResource, SandboxRunResult};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const POST_EXIT_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct PayloadResult {
    outcome: SandboxOutcome,
    cleanup_confirmed: bool,
    truncated: bool,
    output_bytes: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    resource_events: Vec<SandboxResource>,
    diagnostic: Option<String>,
}

pub(super) struct PayloadInputs<'a> {
    pub(super) child: &'a mut ManagedChild,
    pub(super) supervisor: &'a mut Supervisor,
    pub(super) limits: RunLimits,
    pub(super) output_limit: usize,
    pub(super) cancelled: &'a AtomicBool,
    pub(super) started: Instant,
}

impl PayloadResult {
    pub(super) fn apply_to(self, result: &mut SandboxRunResult) {
        result.outcome = self.outcome;
        result.cleanup_confirmed = self.cleanup_confirmed;
        result.truncated = self.truncated;
        result.output_bytes = self.output_bytes;
        result.stdout = self.stdout;
        result.stderr = self.stderr;
        result.test_evidence_available = true;
        result.resource_events = self.resource_events;
        if self.diagnostic.is_some() {
            result.diagnostic = self.diagnostic;
        }
    }
}

pub(super) fn supervise(
    inputs: PayloadInputs<'_>,
    mut out: impl Read + AsRawFd,
    mut err: impl Read + AsRawFd,
) -> io::Result<PayloadResult> {
    let PayloadInputs {
        child,
        supervisor,
        limits,
        output_limit,
        cancelled,
        started,
    } = inputs;
    nonblocking(&out)?;
    nonblocking(&err)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut out_eof = false;
    let mut err_eof = false;
    let mut events = Vec::new();
    let mut cgroup_events_recorded = false;
    let mut output_drain_confirmed = true;
    let mut truncated = false;
    let mut diagnostic = None;
    let wall = Duration::from_millis(limits.wall_millis);
    let cpu_budget = limits.cpu_budget_usec;

    let stop = loop {
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        out_eof |= drain(&mut out, &mut stdout, remaining)?;
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        err_eof |= drain(&mut err, &mut stderr, remaining)?;
        if stdout.len() + stderr.len() >= output_limit {
            truncated = true;
            break StopReason::OutputLimit;
        }
        if cancelled.load(Ordering::SeqCst) {
            break StopReason::Cancelled;
        }
        if let Some(exit) = child.child_mut()?.try_wait()? {
            let cpu_budget_exceeded = cpu_usage_usec(supervisor.group())? >= cpu_budget;
            record_cgroup_limit_events(supervisor.group(), &mut events);
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
                    output_limit,
                    POST_EXIT_PIPE_DRAIN_TIMEOUT,
                    || cancelled.load(Ordering::SeqCst),
                )?
            };
            break match post_exit_drain {
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
                    truncated = true;
                    StopReason::OutputLimit
                }
                PostExitDrain::Cancelled => StopReason::Cancelled,
                PostExitDrain::DeadlineExpired => {
                    truncated = true;
                    output_drain_confirmed = false;
                    diagnostic =
                        Some("output pipes remained open after bounded cgroup cleanup".into());
                    StopReason::Unknown
                }
            };
        }
        if started.elapsed() >= wall {
            break StopReason::WallTimeout;
        }
        if cpu_usage_usec(supervisor.group())? >= cpu_budget {
            break StopReason::CpuBudget;
        }
        thread::sleep(Duration::from_millis(5));
    };

    if !cgroup_events_recorded {
        record_cgroup_limit_events(supervisor.group(), &mut events);
    }
    if matches!(stop, StopReason::Signaled(libc::SIGXCPU)) {
        record_event(&mut events, SandboxResource::Cpu);
    }
    if matches!(stop, StopReason::Signaled(libc::SIGXFSZ)) {
        record_event(&mut events, SandboxResource::FileSize);
    }

    let mut cleanup_confirmed = supervisor.clean() && output_drain_confirmed;
    if !reap(child.child_mut()?) {
        cleanup_confirmed = false;
    }
    let outcome = resolve_outcome(stop, &events, cleanup_confirmed);
    Ok(PayloadResult {
        outcome,
        cleanup_confirmed,
        truncated,
        output_bytes: stdout.len().saturating_add(stderr.len()),
        stdout,
        stderr,
        resource_events: events,
        diagnostic,
    })
}

#[cfg(test)]
mod tests {
    use super::{supervise, PayloadInputs};
    use crate::linux::cgroup::Supervisor;
    use crate::linux::process::ManagedChild;
    use crate::{RunLimits, SandboxOutcome, SandboxResource, SandboxRunResult};
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;
    use uuid::Uuid;

    fn run_shell(
        script: &str,
        output_limit: usize,
        cancelled: bool,
        with_resource_events: bool,
    ) -> SandboxRunResult {
        let group = std::env::temp_dir().join(format!("krypper-supervision-{}", Uuid::new_v4()));
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cpu.stat"), "usage_usec 0\n").unwrap();
        if with_resource_events {
            fs::write(group.join("memory.events"), "low 0\nmax 2\n").unwrap();
            fs::write(group.join("pids.events"), "max 1\n").unwrap();
        }

        let limits = RunLimits::offline_cargo_test();
        let mut supervisor = Supervisor::test_fixture(group.clone(), limits);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = super::super::process::ManagedChild::new(command.spawn().unwrap());
        let (out, err) = {
            let child = child.child_mut().unwrap();
            (child.stdout.take().unwrap(), child.stderr.take().unwrap())
        };
        let cancelled = AtomicBool::new(cancelled);
        let payload = supervise(
            PayloadInputs {
                child: &mut child,
                supervisor: &mut supervisor,
                limits,
                output_limit,
                cancelled: &cancelled,
                started: Instant::now(),
            },
            out,
            err,
        )
        .unwrap();
        let mut result = SandboxRunResult::empty(
            Uuid::new_v4(),
            SandboxOutcome::SandboxUnavailable,
            output_limit,
        );
        payload.apply_to(&mut result);
        fs::remove_dir_all(group).unwrap();
        result
    }

    #[test]
    fn output_limit_keeps_separate_streams_under_one_combined_cap() {
        let result = run_shell("printf out; printf err >&2", 5, false, false);

        assert_eq!(result.outcome, SandboxOutcome::OutputLimitExceeded);
        assert!(result.truncated);
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"er");
        assert_eq!(result.output_bytes, 5);
        assert!(result.cleanup_confirmed);
        assert!(result.test_evidence_available);
    }

    #[test]
    fn cgroup_events_are_reported_in_canonical_order() {
        let result = run_shell("exit 0", 1024, false, true);

        assert_eq!(
            result.resource_events,
            [SandboxResource::Processes, SandboxResource::Memory]
        );
        assert_eq!(
            result.outcome,
            SandboxOutcome::ResourceLimitExceeded {
                resource: SandboxResource::Processes
            }
        );
    }

    #[test]
    fn cancellation_after_payload_launch_is_reported_after_cleanup() {
        let result = run_shell("sleep 0.01", 1024, true, false);

        assert_eq!(result.outcome, SandboxOutcome::Cancelled);
        assert!(result.cleanup_confirmed);
        assert!(result.resource_events.is_empty());
    }

    #[test]
    fn inherited_pipe_deadline_makes_payload_result_unknown() {
        struct ProcessGroup(libc::pid_t);
        impl Drop for ProcessGroup {
            fn drop(&mut self) {
                // SAFETY: this test created the process group with the child PID as its ID.
                unsafe {
                    libc::kill(-self.0, libc::SIGKILL);
                }
            }
        }

        let group = std::env::temp_dir().join(format!("krypper-supervision-{}", Uuid::new_v4()));
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cpu.stat"), "usage_usec 0\n").unwrap();
        let limits = RunLimits::offline_cargo_test();
        let mut supervisor = Supervisor::test_fixture(group.clone(), limits);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "(sleep 30) & printf 'leader output\\n'; exit 0"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::new(command.spawn().unwrap());
        let process_group = ProcessGroup(child.child_mut().unwrap().id() as libc::pid_t);
        let (out, err) = {
            let child = child.child_mut().unwrap();
            (child.stdout.take().unwrap(), child.stderr.take().unwrap())
        };
        let cancelled = AtomicBool::new(false);
        let payload = supervise(
            PayloadInputs {
                child: &mut child,
                supervisor: &mut supervisor,
                limits,
                output_limit: 1024,
                cancelled: &cancelled,
                started: Instant::now(),
            },
            out,
            err,
        )
        .unwrap();
        let mut result =
            SandboxRunResult::empty(Uuid::new_v4(), SandboxOutcome::SandboxUnavailable, 1024);
        payload.apply_to(&mut result);

        assert_eq!(result.outcome, SandboxOutcome::OutcomeUnknown);
        assert!(!result.cleanup_confirmed);
        assert!(result.truncated);
        assert_eq!(
            result.diagnostic.as_deref(),
            Some("output pipes remained open after bounded cgroup cleanup")
        );
        drop(process_group);
        fs::remove_dir_all(group).unwrap();
    }
}
