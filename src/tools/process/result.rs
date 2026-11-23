//! Stable process-attempt result shared by supported and unavailable backends.

use super::outcome::{ProcessOutcome, ProcessResource};
use serde::Serialize;
use uuid::Uuid;

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
            elapsed_ms: 0,
            output_bytes: 0,
            output_limit: 0,
            resource_events: Vec::new(),
            recovery_hold: false,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.outcome == ProcessOutcome::Exited(0)
            && !self.truncated
            && self.cleanup_confirmed
            && !self.recovery_hold
    }
}
