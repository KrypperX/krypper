//! Converts supervisor stop reasons and recorded resource events to outcomes.

use crate::{SandboxOutcome, SandboxResource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopReason {
    OutputLimit,
    Cancelled,
    WallTimeout,
    CpuBudget,
    Exited(i32),
    Signaled(i32),
    Unknown,
}

pub(super) fn resolve_outcome(
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
mod tests {
    use super::{resolve_outcome, StopReason};
    use crate::{SandboxOutcome, SandboxResource};

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
}
