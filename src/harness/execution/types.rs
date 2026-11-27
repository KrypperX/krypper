//! Harness-private executor state around the shared task-turn contracts.

use crate::contracts::goal_turn::{GoalTurnError, GoalTurnStopReason, WorkspaceObservation};
use crate::task_lifecycle::{TurnActivitySummary, TurnOutcome};

pub(super) const MAX_READ_RANGE_LINES: usize = 200;
pub(super) const MAX_READ_RANGE_BYTES: usize = 64 * 1024;

pub(super) enum ActionExecution {
    Observation(WorkspaceObservation),
    Complete(String),
    Continue,
    Cancel,
    Failed {
        observation: WorkspaceObservation,
        error: GoalTurnError,
    },
}

pub(super) struct TurnCompletionData {
    pub(super) activity: Vec<TurnActivitySummary>,
    pub(super) actions_taken: usize,
    pub(super) stop_reason: GoalTurnStopReason,
    pub(super) outcome: TurnOutcome,
}

#[cfg(test)]
mod decision_failure_code_tests {
    use super::GoalTurnError;
    use crate::contracts::goal_turn::{DecisionFailureCode, WorkspaceAction};

    #[test]
    fn action_helper_parses_only_known_reserved_failure_codes() {
        assert_eq!(
            WorkspaceAction::Unsupported {
                action: "decision_failure:provider_unavailable".to_owned(),
            }
            .decision_failure_code(),
            Some(DecisionFailureCode::ProviderUnavailable)
        );
        let status_action = WorkspaceAction::Unsupported {
            action: "decision_failure:provider_rejected:401".to_owned(),
        };
        assert_eq!(
            status_action.decision_failure_code(),
            Some(DecisionFailureCode::ProviderRejected)
        );
        assert_eq!(
            DecisionFailureCode::http_status_from_action_label(
                "decision_failure:provider_rejected:401"
            ),
            Some(401)
        );
        for action in [
            "provider_unavailable",
            "decision_failure:provider_unavailable:secret",
            "decision_failure:request_too_large:401",
            "decision_failure:future_code",
        ] {
            assert_eq!(
                WorkspaceAction::Unsupported {
                    action: action.to_owned(),
                }
                .decision_failure_code(),
                None
            );
        }
        assert_eq!(WorkspaceAction::Continue.decision_failure_code(), None);
        assert_eq!(
            GoalTurnError::UnsupportedAction {
                action: "decision_failure:provider_unavailable:503".to_owned(),
            }
            .to_string(),
            "model decision failed: provider_unavailable (HTTP 503)"
        );
        assert_eq!(
            GoalTurnError::UnsupportedAction {
                action: "unregistered_tool".to_owned(),
            }
            .to_string(),
            "unsupported workspace action 'unregistered_tool'"
        );
    }
}
