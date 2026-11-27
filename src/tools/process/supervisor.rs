//! Product adapter between TaskSession-owned policy and the fixed sandbox runner.

use super::outcome::{ProcessOutcome, ProcessResource};
use super::policy::{validate_test_filter, OfflineCargoPolicy};
use super::result::ProcessResult;
use crate::tools::WorkspaceRoot;
use crate::ProcessExecutionProfile;
use krypper_sandbox::{
    run_offline_cargo, run_process, DirectoryDescriptors, LandlockRequirement,
    OfflineReadOnlyExecRequest, RunLimits, SandboxOutcome, SandboxResource, SandboxRunRequest,
    SandboxRunResult, SpawnAuthorizationGate,
};
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "linux")]
use std::time::Duration;
use uuid::Uuid;

#[cfg(target_os = "linux")]
struct RunOptions {
    wall: Duration,
    output: usize,
    cpu_budget_usec: u64,
    landlock_requirement: LandlockRequirement,
}

pub(crate) fn run(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    run_with_landlock_requirement(
        root,
        policy,
        test_filter,
        spawn_gate,
        LandlockRequirement::Optional,
    )
}

/// Run one typed direct-argv request through the same supervisor and grant gate as fixed Cargo.
pub(crate) fn run_read_only_exec(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    request: &OfflineReadOnlyExecRequest,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    #[cfg(target_os = "linux")]
    {
        if super::effective_uid() == 0 {
            return ProcessResult::empty_read_only_exec(ProcessOutcome::SandboxUnavailable);
        }
        if policy.cancellation().load(Ordering::SeqCst) {
            return ProcessResult::empty_read_only_exec(ProcessOutcome::Cancelled);
        }
        if [&policy.toolchain, &policy.registry]
            .iter()
            .any(|runtime| paths_overlap(root.path(), runtime.path()))
        {
            return ProcessResult::denied_read_only_exec();
        }
        let directories = match directory_descriptors_for_read_only_exec(
            root,
            policy,
            request.working_directory(),
        ) {
            Ok(directories) => directories,
            Err(message) => {
                let mut result =
                    ProcessResult::empty_read_only_exec(ProcessOutcome::SandboxSetupFailed);
                result.diagnostic = Some(message);
                return result;
            }
        };
        let requirements = request.requirements();
        let limits = match requirements.profile_limits().to_run_limits() {
            Ok(limits) => limits,
            Err(_) => return ProcessResult::denied_read_only_exec(),
        };
        execute_read_only_exec(
            policy,
            request,
            requirements,
            limits,
            directories,
            spawn_gate,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, policy, request, spawn_gate);
        ProcessResult::empty_read_only_exec(ProcessOutcome::SandboxUnavailable)
    }
}

fn run_with_landlock_requirement(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    spawn_gate: SpawnAuthorizationGate,
    landlock_requirement: LandlockRequirement,
) -> ProcessResult {
    #[cfg(target_os = "linux")]
    {
        let limits = policy.profile().limits();
        let cpu_budget_usec = match limits.cpu_ms.checked_mul(1_000) {
            Some(value) => value,
            None => return ProcessResult::empty_with_filter(ProcessOutcome::Denied, test_filter),
        };
        run_with_bounds_and_options(
            root,
            policy,
            test_filter,
            RunOptions {
                wall: Duration::from_secs(limits.wall_secs),
                output: limits.output_bytes,
                cpu_budget_usec,
                landlock_requirement,
            },
            spawn_gate,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        let limits = match policy.profile().limits().to_run_limits() {
            Ok(limits) => limits,
            Err(_) => return ProcessResult::empty_with_filter(ProcessOutcome::Denied, test_filter),
        };
        execute(
            policy,
            test_filter,
            limits,
            None,
            spawn_gate,
            landlock_requirement,
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_with_landlock_requirement_for_test(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    requirement: LandlockRequirement,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    run_with_landlock_requirement(root, policy, test_filter, spawn_gate, requirement)
}

/// Lower-only bounds let the Linux host fixtures exercise timeout and output paths quickly.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_with_bounds(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget_usec: u64,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    run_with_bounds_and_options(
        root,
        policy,
        test_filter,
        RunOptions {
            wall,
            output,
            cpu_budget_usec,
            landlock_requirement: LandlockRequirement::Optional,
        },
        spawn_gate,
    )
}

#[cfg(target_os = "linux")]
fn run_with_bounds_and_options(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    options: RunOptions,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    let RunOptions {
        wall,
        output,
        cpu_budget_usec,
        landlock_requirement,
    } = options;
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let profile = policy.profile().limits();
    let profile_wall_millis = match profile.wall_secs.checked_mul(1_000) {
        Some(value) => value,
        None => {
            return setup_failed(
                test_filter,
                "profile wall-seconds conversion overflow",
                profile.output_bytes,
            )
        }
    };
    let profile_cpu_usec = match profile.cpu_ms.checked_mul(1_000) {
        Some(value) => value,
        None => {
            return setup_failed(
                test_filter,
                "profile CPU-milliseconds conversion overflow",
                profile.output_bytes,
            )
        }
    };
    let wall_millis = match u64::try_from(wall.as_millis()) {
        Ok(value) => value,
        Err(_) => {
            return setup_failed(
                test_filter,
                "effective wall limit conversion overflow",
                profile.output_bytes,
            )
        }
    };
    if wall_millis > profile_wall_millis
        || cpu_budget_usec > profile_cpu_usec
        || output > profile.output_bytes
    {
        return setup_failed(
            test_filter,
            "effective limits exceed the profile ceiling",
            profile.output_bytes,
        );
    }
    let limits = RunLimits::new(
        wall_millis,
        cpu_budget_usec,
        profile.memory_bytes,
        output,
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
    let output_limit = output;
    let directories = match directory_descriptors(root, policy) {
        Ok(directories) => Some(directories),
        Err(error) => return setup_failed(test_filter, error, output_limit),
    };
    execute(
        policy,
        test_filter,
        limits,
        directories,
        spawn_gate,
        landlock_requirement,
    )
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

#[cfg(target_os = "linux")]
fn directory_descriptors_for_read_only_exec(
    root: &WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    cwd: &str,
) -> Result<DirectoryDescriptors, String> {
    let workspace = root
        .process_descriptor()
        .map_err(|error| error.to_string())?;
    let toolchain = policy
        .toolchain
        .process_descriptor()
        .map_err(|error| error.to_string())?;
    let registry = policy
        .registry
        .process_descriptor()
        .map_err(|error| error.to_string())?;
    let directories = DirectoryDescriptors::new(workspace, toolchain, registry);
    if cwd == "." {
        Ok(directories)
    } else {
        let workspace_cwd = root
            .process_subdirectory_descriptor(cwd)
            .map_err(|error| error.to_string())?;
        Ok(directories.with_workspace_cwd(workspace_cwd))
    }
}

fn execute(
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    limits: RunLimits,
    directories: Option<DirectoryDescriptors>,
    spawn_gate: SpawnAuthorizationGate,
    landlock_requirement: LandlockRequirement,
) -> ProcessResult {
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let run_id = Uuid::new_v4();
    let Some(requirements) = policy.profile().execution_requirements().cloned() else {
        return ProcessResult::empty_with_filter(ProcessOutcome::Denied, test_filter);
    };
    let request = SandboxRunRequest::new_with_requirements(
        run_id,
        directories,
        test_filter.map(str::to_owned),
        limits,
        policy.cancellation(),
        requirements,
    )
    .with_landlock_requirement(landlock_requirement)
    .with_spawn_gate(spawn_gate);
    from_runner(
        run_offline_cargo(request),
        test_filter,
        ProcessExecutionProfile::OfflineCargoTest,
    )
}

#[cfg(target_os = "linux")]
fn execute_read_only_exec(
    policy: &OfflineCargoPolicy,
    _request: &OfflineReadOnlyExecRequest,
    requirements: krypper_sandbox::ValidatedExecutionRequirements,
    limits: RunLimits,
    directories: DirectoryDescriptors,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    let request = SandboxRunRequest::new_with_requirements(
        Uuid::new_v4(),
        Some(directories),
        None,
        limits,
        policy.cancellation(),
        requirements,
    )
    .with_landlock_requirement(LandlockRequirement::Required)
    .with_spawn_gate(spawn_gate);
    from_runner(
        run_process(request),
        None,
        ProcessExecutionProfile::OfflineReadOnlyExecV1,
    )
}

#[cfg(target_os = "linux")]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(target_os = "linux")]
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

fn from_runner(
    raw: SandboxRunResult,
    test_filter: Option<&str>,
    profile: ProcessExecutionProfile,
) -> ProcessResult {
    let mut result = ProcessResult::empty_with_filter(map_outcome(raw.outcome), test_filter);
    result.profile = profile;
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
    result.sandbox_evidence = raw.sandbox_evidence;
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

#[cfg(all(test, target_os = "linux"))]
pub(super) fn record_raw_output(result: &mut ProcessResult, stdout: Vec<u8>, stderr: Vec<u8>) {
    result.output_bytes = stdout.len().saturating_add(stderr.len());
    result.stdout = String::from_utf8_lossy(&stdout).into_owned();
    result.stderr = String::from_utf8_lossy(&stderr).into_owned();
}

#[cfg(test)]
mod isolation_evidence_tests {
    use super::from_runner;
    use crate::tools::process::outcome::{ProcessOutcome, ProcessResource};
    use crate::ProcessExecutionProfile;
    use krypper_sandbox::{
        SandboxBackend, SandboxIsolationEvidence, SandboxOutcome, SandboxResource, SandboxRunResult,
    };
    use uuid::Uuid;

    fn runner_result(evidence: Option<SandboxIsolationEvidence>) -> SandboxRunResult {
        SandboxRunResult {
            run_id: Uuid::new_v4(),
            outcome: SandboxOutcome::Exited(7),
            stdout: b"stdout".to_vec(),
            stderr: b"stderr".to_vec(),
            truncated: true,
            diagnostic: Some("cleanup diagnostic".to_owned()),
            cleanup_confirmed: false,
            elapsed_ms: 17,
            output_bytes: 12,
            output_limit: 256,
            resource_events: vec![SandboxResource::Memory, SandboxResource::Cpu],
            recovery_hold: true,
            test_evidence_available: false,
            sandbox_evidence: evidence,
        }
    }

    #[test]
    fn adapter_preserves_sandbox_evidence_and_existing_result_fields() {
        let evidence = SandboxIsolationEvidence::new(Some(SandboxBackend::Bubblewrap));
        let raw = runner_result(Some(evidence.clone()));
        let run_id = raw.run_id;
        let result = from_runner(raw, None, ProcessExecutionProfile::OfflineCargoTest);

        assert_eq!(result.run_id, run_id);
        assert_eq!(result.outcome, ProcessOutcome::Exited(7));
        assert_eq!(result.stdout, "stdout");
        assert_eq!(result.stderr, "stderr");
        assert!(result.truncated);
        assert_eq!(result.diagnostic.as_deref(), Some("cleanup diagnostic"));
        assert!(!result.cleanup_confirmed);
        assert_eq!(result.elapsed_ms, 17);
        assert_eq!(result.output_bytes, 12);
        assert_eq!(result.output_limit, 256);
        assert_eq!(
            result.resource_events,
            [ProcessResource::Memory, ProcessResource::Cpu]
        );
        assert!(result.recovery_hold);
        assert_eq!(result.sandbox_evidence, Some(evidence));
    }

    #[test]
    fn absent_evidence_keeps_the_legacy_json_shape() {
        let result = from_runner(
            runner_result(None),
            None,
            ProcessExecutionProfile::OfflineCargoTest,
        );
        let json = serde_json::to_value(result).expect("serialize process result");

        assert!(json.get("sandbox_evidence").is_none());
    }
}
