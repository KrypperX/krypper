use crate::agent_loop::GoalTurnStopReason;
use crate::task_lifecycle::{TaskSession, TaskState};
use std::io;
use uuid::Uuid;

/// Maximum number of required checks returned by one verification call.
pub const MAX_COMPLETION_CHECKS: usize = 32;
/// Maximum number of bytes returned for a check description or evidence summary.
pub const MAX_COMPLETION_TEXT_BYTES: usize = 256;
const MAX_COMPLETION_ID_BYTES: usize = 64;

/// A caller-defined outcome that must be checked independently before task success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredCompletionCheck {
    pub id: String,
    pub description: String,
}

/// The result supplied by a deterministic check outside Brain/provider output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionCheckEvidenceStatus {
    Passed,
    Failed,
    Unknown,
}

/// The kind of concrete evidence used to evaluate a completion check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionEvidenceSource {
    TaskSessionObservation,
    DeterministicLocalCheck,
}

/// Evidence supplied by a Harness caller for one required check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCheckEvidence {
    pub check_id: String,
    pub status: CompletionCheckEvidenceStatus,
    pub source: CompletionEvidenceSource,
    pub summary: String,
}

/// The outcome of a check or complete verification request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionVerificationStatus {
    Passed,
    Failed,
    Unknown,
}

/// A bounded result for one required completion check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCompletionCheck {
    pub id: String,
    pub description: String,
    pub status: CompletionVerificationStatus,
    pub evidence_source: Option<CompletionEvidenceSource>,
    pub evidence_summary: Option<String>,
}

/// Structured verification output; omitted input counts prevent truncation from passing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionVerification {
    pub status: CompletionVerificationStatus,
    pub checks: Vec<VerifiedCompletionCheck>,
    pub omitted_checks: usize,
    pub omitted_evidence: usize,
}

/// Reconcile a task using caller-supplied checks that are independent of model completion claims.
///
/// A completed turn succeeds only when at least one required check is present and every check has
/// exactly one passing evidence record. Missing, duplicate, unknown, or truncated evidence fails
/// closed. This method only records the resulting task state through `TaskSession`; it does not
/// inspect or mutate the workspace. Callers should derive evidence from recorded observations or
/// deterministic local checks, never from Brain/provider completion text.
pub fn reconcile_task_state_with_checks(
    session: &mut TaskSession,
    task_id: Uuid,
    stop_reason: GoalTurnStopReason,
    required_checks: &[RequiredCompletionCheck],
    evidence: &[CompletionCheckEvidence],
) -> io::Result<CompletionVerification> {
    let verification = verify_completion_checks(required_checks, evidence);
    let next_state = match stop_reason {
        GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted => Some(
            if verification.status == CompletionVerificationStatus::Passed {
                TaskState::Succeeded
            } else {
                TaskState::Failed
            },
        ),
        GoalTurnStopReason::Continued | GoalTurnStopReason::Cancelled => None,
        GoalTurnStopReason::ActionBudgetExhausted | GoalTurnStopReason::ActionFailed => {
            Some(TaskState::Failed)
        }
    };

    if let Some(next_state) = next_state {
        session
            .get_task_mut(task_id)
            .ok_or_else(|| io::Error::other("task was not found in its session"))?
            .transition(next_state)
            .map_err(io::Error::other)?;
    }

    Ok(verification)
}

/// Evaluate required checks without changing task or workspace state.
pub fn verify_completion_checks(
    required_checks: &[RequiredCompletionCheck],
    evidence: &[CompletionCheckEvidence],
) -> CompletionVerification {
    let omitted_checks = required_checks.len().saturating_sub(MAX_COMPLETION_CHECKS);
    let omitted_evidence = evidence.len().saturating_sub(MAX_COMPLETION_CHECKS);
    let input_truncated = omitted_checks > 0 || omitted_evidence > 0;
    let checks = required_checks
        .iter()
        .take(MAX_COMPLETION_CHECKS)
        .map(|required| {
            let valid_id = !required.id.trim().is_empty()
                && required.id.len() <= MAX_COMPLETION_ID_BYTES
                && !input_truncated
                && required_checks
                    .iter()
                    .filter(|other| other.id == required.id)
                    .count()
                    == 1;
            let matches = evidence
                .iter()
                .take(MAX_COMPLETION_CHECKS)
                .filter(|item| item.check_id == required.id)
                .collect::<Vec<_>>();

            let (status, evidence_source, evidence_summary) = if !valid_id || matches.len() != 1 {
                (CompletionVerificationStatus::Unknown, None, None)
            } else {
                let item = matches[0];
                let summary = (!item.summary.trim().is_empty())
                    .then(|| bounded_text(&item.summary, MAX_COMPLETION_TEXT_BYTES));
                let status = match item.status {
                    CompletionCheckEvidenceStatus::Passed if summary.is_some() => {
                        CompletionVerificationStatus::Passed
                    }
                    CompletionCheckEvidenceStatus::Failed => CompletionVerificationStatus::Failed,
                    CompletionCheckEvidenceStatus::Passed
                    | CompletionCheckEvidenceStatus::Unknown => {
                        CompletionVerificationStatus::Unknown
                    }
                };
                (status, Some(item.source), summary)
            };

            VerifiedCompletionCheck {
                id: bounded_text(&required.id, MAX_COMPLETION_ID_BYTES),
                description: bounded_text(&required.description, MAX_COMPLETION_TEXT_BYTES),
                status,
                evidence_source,
                evidence_summary,
            }
        })
        .collect::<Vec<_>>();

    let status = if checks
        .iter()
        .any(|check| check.status == CompletionVerificationStatus::Failed)
    {
        CompletionVerificationStatus::Failed
    } else if checks.is_empty()
        || input_truncated
        || checks
            .iter()
            .any(|check| check.status != CompletionVerificationStatus::Passed)
    {
        CompletionVerificationStatus::Unknown
    } else {
        CompletionVerificationStatus::Passed
    };

    CompletionVerification {
        status,
        checks,
        omitted_checks,
        omitted_evidence,
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    let marker = "…";
    let max_prefix_bytes = max_bytes.saturating_sub(marker.len());
    let mut end = 0;
    for (start, character) in value.char_indices() {
        let next = start + character.len_utf8();
        if next > max_prefix_bytes {
            break;
        }
        end = next;
    }
    format!("{}{}", &value[..end], marker)
}

pub(crate) fn reconcile_task_state(
    session: &mut TaskSession,
    task_id: Uuid,
    stop_reason: GoalTurnStopReason,
) -> io::Result<()> {
    let next_state = match stop_reason {
        GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted => {
            Some(TaskState::Succeeded)
        }
        GoalTurnStopReason::Continued | GoalTurnStopReason::Cancelled => None,
        GoalTurnStopReason::ActionBudgetExhausted | GoalTurnStopReason::ActionFailed => {
            Some(TaskState::Failed)
        }
    };

    if let Some(next_state) = next_state {
        session
            .get_task_mut(task_id)
            .ok_or_else(|| io::Error::other("task was not found in its session"))?
            .transition(next_state)
            .map_err(io::Error::other)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reconcile_task_state;
    use crate::agent_loop::GoalTurnStopReason;
    use crate::task_lifecycle::{TaskSession, TaskState};
    use uuid::Uuid;

    fn running_task() -> (TaskSession, Uuid) {
        let mut session = TaskSession::new();
        let task_id = session.create_task("goal").id();
        session
            .get_task_mut(task_id)
            .expect("created task")
            .transition(TaskState::Running)
            .expect("start task");
        (session, task_id)
    }

    fn state(session: &TaskSession, task_id: Uuid) -> TaskState {
        session.get_task(task_id).expect("task exists").state()
    }

    fn lifecycle_sequence_high_water(session: &TaskSession, task_id: Uuid) -> u64 {
        session
            .snapshot()
            .tasks()
            .iter()
            .find(|task| task.id() == task_id)
            .expect("task snapshot exists")
            .lifecycle_sequence_high_water()
    }

    #[test]
    fn completed_stop_reason_succeeds_task_through_session() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(&mut session, task_id, GoalTurnStopReason::Completed)
            .expect("reconcile completed turn");

        assert_eq!(state(&session, task_id), TaskState::Succeeded);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 2);
    }

    #[test]
    fn action_completed_stop_reason_succeeds_task() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(&mut session, task_id, GoalTurnStopReason::ActionCompleted)
            .expect("reconcile completed action");

        assert_eq!(state(&session, task_id), TaskState::Succeeded);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 2);
    }

    #[test]
    fn action_budget_exhausted_stop_reason_fails_task() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(
            &mut session,
            task_id,
            GoalTurnStopReason::ActionBudgetExhausted,
        )
        .expect("reconcile exhausted action budget");

        assert_eq!(state(&session, task_id), TaskState::Failed);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 2);
    }

    #[test]
    fn action_failed_stop_reason_fails_task() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(&mut session, task_id, GoalTurnStopReason::ActionFailed)
            .expect("reconcile failed action");

        assert_eq!(state(&session, task_id), TaskState::Failed);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 2);
    }

    #[test]
    fn continued_stop_reason_leaves_task_state_unchanged() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(&mut session, task_id, GoalTurnStopReason::Continued)
            .expect("reconcile continued turn");

        assert_eq!(state(&session, task_id), TaskState::Running);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 1);
    }

    #[test]
    fn cancelled_stop_reason_leaves_task_state_unchanged() {
        let (mut session, task_id) = running_task();

        reconcile_task_state(&mut session, task_id, GoalTurnStopReason::Cancelled)
            .expect("reconcile cancelled turn");

        assert_eq!(state(&session, task_id), TaskState::Running);
        assert_eq!(lifecycle_sequence_high_water(&session, task_id), 1);
    }

    #[test]
    fn terminal_stop_reason_preserves_missing_task_error() {
        let mut session = TaskSession::new();

        let error =
            reconcile_task_state(&mut session, Uuid::new_v4(), GoalTurnStopReason::Completed)
                .expect_err("missing task must be reported");

        assert_eq!(error.to_string(), "task was not found in its session");
    }
}
