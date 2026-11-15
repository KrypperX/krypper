use crate::agent_loop::GoalTurnStopReason;
use crate::task_lifecycle::{TaskSession, TaskState};
use std::io;
use uuid::Uuid;

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
