//! Typed process outcomes and deterministic, evidence-based classification.

use serde::Serialize;

/// Terminal state of one fixed-profile attempt.
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

/// The condition that stopped the supervised loop, before cleanup readback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(target_os = "linux")]
pub(super) enum StopReason {
    OutputLimit,
    Cancelled,
    WallTimeout,
    CpuBudget,
    Exited(i32),
    Signaled(i32),
    Unknown,
}

/// Record a kernel limit event once, keeping a fixed precedence for audit and selection.
#[cfg(target_os = "linux")]
pub(super) fn record_event(events: &mut Vec<ProcessResource>, resource: ProcessResource) {
    if events.contains(&resource) {
        return;
    }
    events.push(resource);
    events.sort_by_key(|resource| match resource {
        ProcessResource::Processes => 0,
        ProcessResource::Memory => 1,
        ProcessResource::Cpu => 2,
        ProcessResource::FileSize => 3,
    });
}

/// Select one outcome. Unconfirmed cleanup and kernel limit events win over the loop's stop
/// reason so an interrupted or over-budget run can never be reported as a clean completion.
#[cfg(target_os = "linux")]
pub(super) fn resolve_outcome(
    stop: StopReason,
    events: &[ProcessResource],
    cleanup_confirmed: bool,
) -> ProcessOutcome {
    if !cleanup_confirmed {
        return ProcessOutcome::OutcomeUnknown;
    }
    if let Some(resource) = events.first() {
        return ProcessOutcome::ResourceLimitExceeded {
            resource: *resource,
        };
    }
    match stop {
        StopReason::OutputLimit => ProcessOutcome::OutputLimitExceeded,
        StopReason::Cancelled => ProcessOutcome::Cancelled,
        StopReason::WallTimeout => ProcessOutcome::TimedOut,
        StopReason::CpuBudget => ProcessOutcome::ResourceLimitExceeded {
            resource: ProcessResource::Cpu,
        },
        StopReason::Exited(code) => ProcessOutcome::Exited(code),
        StopReason::Signaled(signal) => ProcessOutcome::Signaled(signal),
        StopReason::Unknown => ProcessOutcome::OutcomeUnknown,
    }
}
