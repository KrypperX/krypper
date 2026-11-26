//! Process supervision: leases, bounded pipes, reaping and the two run entry points.

use crate::host::effective_uid;
use crate::sandbox::sandbox;
use crate::seccomp::compile_filter;
use crate::{RunLimits, SandboxOutcome, SandboxRunRequest, SandboxRunResult};
use std::io;
use std::sync::atomic::Ordering;
use std::time::Instant;

mod cgroup;
mod outcome;
mod process;
mod recovery;
mod supervision;

#[cfg(test)]
use cgroup::record_event;
use cgroup::Supervisor;
use outcome::{resolve_outcome, StopReason};
use process::{bounded_host, ManagedChild};
use recovery::{
    hold_for_recovery, is_cleanup_uncertain, mark_cleanup_uncertain, try_active_lock,
    ActiveLockError, HostLease, HostLeaseError,
};
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
    let filter = match compile_filter() {
        Ok(filter) => filter,
        Err(error) => {
            result.outcome = SandboxOutcome::SandboxSetupFailed;
            result.diagnostic = Some(format!("fixed syscall filter setup failed: {error}"));
            return result;
        }
    };
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
                let _ = lease.clear_receipt();
            } else {
                result.outcome = SandboxOutcome::OutcomeUnknown;
                result.recovery_hold = true;
            }
            return result;
        }
    };
    result.output_limit = output;
    let mut launched = false;
    let started = Instant::now();
    let execution = (|| -> io::Result<()> {
        lease.record(&format!(
            "{}\n{}\n",
            supervisor.unit(),
            supervisor.group().display()
        ))?;
        let mut probe = sandbox(&directories, supervisor.group(), &filter)?;
        probe.args(["--", "/usr/bin/true"]);
        if !bounded_host(&mut probe)?.is_empty() {
            return Err(io::Error::other("unexpected sandbox output"));
        }
        supervisor.verify()?;
        if cancelled.load(Ordering::SeqCst) {
            settle_before_launch(&mut result, &mut supervisor, StopReason::Cancelled);
            return Ok(());
        }
        let mut command = sandbox(&directories, supervisor.group(), &filter)?;
        command.args(cargo_command_args(test_filter.as_deref()));
        result.outcome = SandboxOutcome::SpawnFailed;
        let mut child = ManagedChild::new(command.spawn()?);
        launched = true;
        let (out, err) = {
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
        supervision::supervise(
            supervision::PayloadInputs {
                child: &mut child,
                supervisor: &mut supervisor,
                limits,
                output_limit: output,
                cancelled: &cancelled,
                started,
            },
            out,
            err,
        )?
        .apply_to(&mut result);
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
        let _ = lease.clear_receipt();
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
    fn cancellation_after_probe_resolves_before_any_child_launch() {
        let path = std::env::temp_dir().join(format!("krypper-sandbox-cancel-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let mut supervisor =
            super::cgroup::Supervisor::test_fixture(path.clone(), RunLimits::offline_cargo_test());
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
