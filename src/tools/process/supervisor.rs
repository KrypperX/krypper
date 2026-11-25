//! Product adapter between TaskSession-owned policy and the fixed sandbox runner.

use super::outcome::{ProcessOutcome, ProcessResource};
use super::policy::{validate_test_filter, OfflineCargoPolicy};
use super::result::ProcessResult;
use crate::tools::WorkspaceRoot;
use krypper_sandbox::{
    run_offline_cargo, DirectoryDescriptors, RunLimits, SandboxOutcome, SandboxResource,
    SandboxRunRequest, SandboxRunResult,
};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::time::Duration;
use uuid::Uuid;

pub(crate) fn run(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    #[cfg(target_os = "linux")]
    {
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        let limits = RunLimits::offline_cargo_test();
        execute(policy, test_filter, limits, None)
    }
}

/// Lower-only bounds let the Linux host fixtures exercise timeout and output paths quickly.
#[cfg(target_os = "linux")]
pub(crate) fn run_with_bounds(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget_usec: u64,
) -> ProcessResult {
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let profile = policy.profile().limits();
    let wall_millis = wall
        .min(Duration::from_secs(profile.wall_secs))
        .as_millis()
        .min(u64::MAX as u128) as u64;
    let limits = RunLimits::new(
        wall_millis,
        cpu_budget_usec.min(profile.cpu_ms.saturating_mul(1_000)),
        profile.memory_bytes,
        output.min(profile.output_bytes),
        profile.processes,
    );
    let limits = match limits {
        Ok(limits) => limits,
        Err(error) => return setup_failed(test_filter, error, profile.output_bytes),
    };
    if super::effective_uid() == 0 {
        return ProcessResult::empty_with_filter(ProcessOutcome::SandboxUnavailable, test_filter);
    }
    if policy.cancellation().load(Ordering::SeqCst) {
        return ProcessResult::empty_with_filter(ProcessOutcome::Cancelled, test_filter);
    }
    if [&policy.toolchain, &policy.registry]
        .iter()
        .any(|runtime| paths_overlap(root.path(), runtime.path()))
    {
        return ProcessResult::denied(test_filter);
    }
    let output_limit = output.min(profile.output_bytes);
    let directories = match directory_descriptors(root, policy) {
        Ok(directories) => Some(directories),
        Err(error) => return setup_failed(test_filter, error, output_limit),
    };
    execute(policy, test_filter, limits, directories)
}

#[cfg(target_os = "linux")]
fn directory_descriptors(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
) -> Result<DirectoryDescriptors, String> {
    Ok(DirectoryDescriptors::new(
        root.process_descriptor()
            .map_err(|error| error.to_string())?,
        policy
            .toolchain
            .process_descriptor()
            .map_err(|error| error.to_string())?,
        policy
            .registry
            .process_descriptor()
            .map_err(|error| error.to_string())?,
    ))
}

fn execute(
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    limits: RunLimits,
    directories: Option<DirectoryDescriptors>,
) -> ProcessResult {
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let run_id = Uuid::new_v4();
    let request = SandboxRunRequest::new(
        run_id,
        directories,
        test_filter.map(str::to_owned),
        limits,
        policy.cancellation(),
    );
    from_runner(run_offline_cargo(request), test_filter)
}

#[cfg(target_os = "linux")]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn setup_failed(
    test_filter: Option<&str>,
    diagnostic: impl Into<String>,
    output_limit: usize,
) -> ProcessResult {
    let mut result =
        ProcessResult::empty_with_filter(ProcessOutcome::SandboxSetupFailed, test_filter);
    result.diagnostic = Some(diagnostic.into());
    result.output_limit = output_limit;
    result
}

fn from_runner(raw: SandboxRunResult, test_filter: Option<&str>) -> ProcessResult {
    let mut result = ProcessResult::empty_with_filter(map_outcome(raw.outcome), test_filter);
    result.run_id = raw.run_id;
    result.stdout = String::from_utf8_lossy(&raw.stdout).into_owned();
    result.stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
    result.truncated = raw.truncated;
    result.diagnostic = raw.diagnostic;
    result.cleanup_confirmed = raw.cleanup_confirmed;
    result.elapsed_ms = raw.elapsed_ms;
    result.output_bytes = raw.output_bytes;
    result.output_limit = raw.output_limit;
    result.resource_events = raw.resource_events.into_iter().map(map_resource).collect();
    result.recovery_hold = raw.recovery_hold;
    if raw.test_evidence_available {
        record_test_evidence(&mut result);
    }
    result
}

fn map_outcome(outcome: SandboxOutcome) -> ProcessOutcome {
    match outcome {
        SandboxOutcome::Denied => ProcessOutcome::Denied,
        SandboxOutcome::InvalidTestFilter => ProcessOutcome::InvalidTestFilter,
        SandboxOutcome::SandboxUnavailable => ProcessOutcome::SandboxUnavailable,
        SandboxOutcome::SandboxSetupFailed => ProcessOutcome::SandboxSetupFailed,
        SandboxOutcome::SpawnFailed => ProcessOutcome::SpawnFailed,
        SandboxOutcome::Exited(code) => ProcessOutcome::Exited(code),
        SandboxOutcome::Signaled(signal) => ProcessOutcome::Signaled(signal),
        SandboxOutcome::TimedOut => ProcessOutcome::TimedOut,
        SandboxOutcome::Cancelled => ProcessOutcome::Cancelled,
        SandboxOutcome::ResourceLimitExceeded { resource } => {
            ProcessOutcome::ResourceLimitExceeded {
                resource: map_resource(resource),
            }
        }
        SandboxOutcome::OutputLimitExceeded => ProcessOutcome::OutputLimitExceeded,
        SandboxOutcome::OutcomeUnknown => ProcessOutcome::OutcomeUnknown,
    }
}

fn map_resource(resource: SandboxResource) -> ProcessResource {
    match resource {
        SandboxResource::Cpu => ProcessResource::Cpu,
        SandboxResource::Memory => ProcessResource::Memory,
        SandboxResource::Processes => ProcessResource::Processes,
        SandboxResource::FileSize => ProcessResource::FileSize,
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
