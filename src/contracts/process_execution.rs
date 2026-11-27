//! Provider-independent result vocabulary for one fixed-profile process attempt.
//!
//! Tools owns process authorization and execution. These shared types carry the verified result
//! to Core, Brain, Harness, Streaming, and public API consumers without making the data contract
//! depend on the Tools implementation module.

use krypper_sandbox::SandboxIsolationEvidence;
use serde::Serialize;
use uuid::Uuid;

/// Terminal state of one fixed-profile attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    Denied,
    InvalidTestFilter,
    NoTestsMatched,
    SandboxUnavailable,
    SandboxSetupFailed,
    /// The requested direct host process is unavailable on this platform.
    HostExecutionUnavailable,
    SpawnFailed,
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Cancelled,
    ResourceLimitExceeded {
        resource: ProcessResource,
    },
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

/// Host-owned process profile represented by one result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessExecutionProfile {
    /// The legacy fixed offline Cargo test profile.
    OfflineCargoTest,
    /// The bounded direct-argv offline/read-only profile.
    OfflineReadOnlyExecV1,
    /// Direct host process execution after task-level Full Access confirmation.
    FullAccessHostProcessV1,
}

impl ProcessExecutionProfile {
    fn is_offline_cargo_test(&self) -> bool {
        *self == Self::OfflineCargoTest
    }
}

/// Bounded process output and the verified typed outcome of one attempt.
///
/// Renderers must escape terminal controls and treat output as untrusted evidence, never as
/// instructions. No output is checkpointed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessResult {
    #[serde(serialize_with = "serialize_run_id")]
    pub run_id: Uuid,
    pub outcome: ProcessOutcome,
    /// Host-selected profile that produced the result. The fixed Cargo value is omitted from
    /// serialized legacy results to preserve their existing public representation.
    #[serde(skip_serializing_if = "ProcessExecutionProfile::is_offline_cargo_test")]
    pub profile: ProcessExecutionProfile,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub diagnostic: Option<String>,
    /// Whether this profile's cleanup boundary is verified. For Full Access this covers only
    /// the original process group and captured pipes; escaped descendants are not discoverable.
    pub cleanup_confirmed: bool,
    pub test_filter: Option<String>,
    pub tests_ran: bool,
    pub executed_tests: Option<usize>,
    /// Observed wall time of the attempt in milliseconds.
    pub elapsed_ms: u64,
    /// Retained stdout and stderr bytes; never exceeds `output_limit` for a launched run.
    pub output_bytes: usize,
    /// Combined raw output ceiling enforced for this run.
    pub output_limit: usize,
    /// Kernel limit events observed before cleanup, in fixed precedence order.
    pub resource_events: Vec<ProcessResource>,
    /// True when an earlier run left cleanup unconfirmed and this run was held.
    pub recovery_hold: bool,
    /// Optional typed sandbox setup evidence. Omitted for legacy and non-sandbox results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_evidence: Option<SandboxIsolationEvidence>,
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

    pub(crate) fn empty(outcome: ProcessOutcome) -> Self {
        Self::empty_with_filter(outcome, None)
    }

    pub(crate) fn empty_with_filter(outcome: ProcessOutcome, test_filter: Option<&str>) -> Self {
        Self {
            run_id: Uuid::new_v4(),
            outcome,
            profile: ProcessExecutionProfile::OfflineCargoTest,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            test_filter: test_filter.map(str::to_owned),
            tests_ran: false,
            executed_tests: None,
            elapsed_ms: 0,
            output_bytes: 0,
            output_limit: 0,
            resource_events: Vec::new(),
            recovery_hold: false,
            sandbox_evidence: None,
        }
    }

    pub(crate) fn denied_read_only_exec() -> Self {
        let mut result = Self::empty(ProcessOutcome::Denied);
        result.profile = ProcessExecutionProfile::OfflineReadOnlyExecV1;
        result
    }

    pub(crate) fn denied_host_process() -> Self {
        let mut result = Self::empty(ProcessOutcome::Denied);
        result.profile = ProcessExecutionProfile::FullAccessHostProcessV1;
        result.diagnostic =
            Some("host execution requires an explicitly confirmed Full Access task".into());
        result
    }

    pub(crate) fn host_process_recovery_hold() -> Self {
        let mut result = Self::denied_host_process();
        result.cleanup_confirmed = false;
        result.recovery_hold = true;
        result.diagnostic = Some(
            "host process was not started because cleanup of an earlier host process was not confirmed; a detached descendant may remain".into(),
        );
        result
    }

    pub(crate) fn empty_host_process(outcome: ProcessOutcome) -> Self {
        let mut result = Self::empty(outcome);
        result.profile = ProcessExecutionProfile::FullAccessHostProcessV1;
        result.diagnostic = Some(
            "direct host execution has no Krypper sandbox, workspace fence, or egress restriction; process-group cleanup covers only the original group and captured pipes, so escaped descendants may remain undetected; outer OS policy still applies".into(),
        );
        result
    }

    pub(crate) fn empty_read_only_exec(outcome: ProcessOutcome) -> Self {
        let mut result = Self::empty(outcome);
        result.profile = ProcessExecutionProfile::OfflineReadOnlyExecV1;
        result
    }

    /// Reports whether the direct process completed successfully and profile-specific cleanup
    /// was verified. Full Access cannot establish that a process did not escape its process group.
    pub fn succeeded(&self) -> bool {
        self.outcome == ProcessOutcome::Exited(0)
            && !self.truncated
            && self.cleanup_confirmed
            && !self.recovery_hold
    }
}
