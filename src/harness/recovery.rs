use crate::task_lifecycle::{TaskSession, TaskSessionCheckpointError, TaskState, TurnState};
use std::fmt;
use std::io;
use std::path::Path;
use uuid::Uuid;

/// The restored session and the fresh task turn prepared for CLI execution.
#[derive(Debug)]
pub(crate) struct PreparedResume {
    pub(crate) session: TaskSession,
    pub(crate) task_id: Uuid,
    pub(crate) turn_id: Uuid,
    pub(crate) goal: String,
}

/// A checkpoint could not be prepared for the next task turn.
#[derive(Debug)]
pub(crate) enum ResumeError {
    Checkpoint(TaskSessionCheckpointError),
    TaskCount(usize),
    TaskNotRunning(TaskState),
    SessionSetup(io::Error),
}

impl fmt::Display for ResumeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checkpoint(error) => error.fmt(formatter),
            Self::TaskCount(count) => {
                write!(
                    formatter,
                    "checkpoint must contain exactly one task, found {count}"
                )
            }
            Self::TaskNotRunning(state) => {
                write!(formatter, "checkpoint task must be running, found {state}")
            }
            Self::SessionSetup(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ResumeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Checkpoint(error) => Some(error),
            Self::SessionSetup(error) => Some(error),
            Self::TaskCount(_) | Self::TaskNotRunning(_) => None,
        }
    }
}

/// Restores a checkpoint, validates its single running task, and starts that task's next turn.
///
/// Core remains responsible for checkpoint parsing, workspace binding, and lifecycle changes.
pub(crate) fn prepare_resumed_session(
    checkpoint: &[u8],
    workspace_path: Option<&Path>,
) -> Result<PreparedResume, ResumeError> {
    let restored = match workspace_path {
        Some(path) => TaskSession::restore_checkpoint_in_workspace(checkpoint, path),
        None => TaskSession::restore_checkpoint(checkpoint),
    };
    let mut session = restored.map_err(ResumeError::Checkpoint)?;

    let tasks = session.list_tasks();
    if tasks.len() != 1 {
        return Err(ResumeError::TaskCount(tasks.len()));
    }
    let (task_id, goal) = {
        let task = &tasks[0];
        if task.state() != TaskState::Running {
            return Err(ResumeError::TaskNotRunning(task.state()));
        }
        (task.id(), task.description().to_owned())
    };

    let turn_id = {
        let task = session.get_task_mut(task_id).ok_or_else(|| {
            ResumeError::SessionSetup(io::Error::other(
                "restored task was not found in its session",
            ))
        })?;
        let turn_id = task
            .create_turn()
            .map_err(io::Error::other)
            .map_err(ResumeError::SessionSetup)?;
        task.transition_turn(turn_id, TurnState::Running)
            .map_err(io::Error::other)
            .map_err(ResumeError::SessionSetup)?;
        turn_id
    };

    Ok(PreparedResume {
        session,
        task_id,
        turn_id,
        goal,
    })
}

#[cfg(test)]
mod tests {
    use super::{prepare_resumed_session, ResumeError};
    use crate::task_lifecycle::{
        TaskSession, TaskSessionCheckpointError, TaskState, TurnOutcome, TurnState,
    };
    use crate::CURRENT_DIR_TEST_LOCK;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    struct TempWorkspace(PathBuf);

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("krypper-recovery-{}", Uuid::new_v4()));
            fs::create_dir(&path).expect("create temporary workspace");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove temporary workspace");
        }
    }

    struct RestoreCurrentDir(PathBuf);

    impl RestoreCurrentDir {
        fn enter(path: &Path) -> Self {
            let previous = std::env::current_dir().expect("read current directory");
            std::env::set_current_dir(path).expect("set temporary workspace");
            Self(previous)
        }
    }

    impl Drop for RestoreCurrentDir {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    fn running_task_checkpoint(workspace: &Path) -> (Vec<u8>, Uuid, Uuid) {
        let mut session = TaskSession::new_in_workspace(workspace).expect("bind workspace");
        let task = session.create_task("resume the stored task");
        task.transition(TaskState::Running).expect("start task");
        let task_id = task.id();
        let first_turn_id = task.create_turn().expect("create original turn");
        task.transition_turn(first_turn_id, TurnState::Running)
            .expect("start original turn");
        session
            .complete_turn(
                task_id,
                first_turn_id,
                TurnOutcome::Failure {
                    detail: "checkpoint boundary".to_owned(),
                },
            )
            .expect("complete original turn");
        (
            session.export_checkpoint().expect("export checkpoint"),
            task_id,
            first_turn_id,
        )
    }

    #[test]
    fn resume_preserves_task_and_starts_a_fresh_active_turn() {
        let workspace = TempWorkspace::new();
        let (checkpoint, task_id, first_turn_id) = running_task_checkpoint(workspace.path());

        let resumed = prepare_resumed_session(&checkpoint, Some(workspace.path()))
            .expect("prepare resumed task");

        assert_eq!(resumed.task_id, task_id);
        assert_eq!(resumed.goal, "resume the stored task");
        assert_ne!(resumed.turn_id, first_turn_id);
        let task = resumed
            .session
            .get_task(task_id)
            .expect("restored task remains in session");
        assert_eq!(task.state(), TaskState::Running);
        assert_eq!(task.turns().len(), 2);
        assert_eq!(task.turns()[0].id(), first_turn_id);
        assert_eq!(task.turns()[0].state(), TurnState::Failed);
        assert_eq!(task.turns()[1].id(), resumed.turn_id);
        assert_eq!(task.turns()[1].state(), TurnState::Running);
    }

    #[test]
    fn resume_rejects_a_malformed_checkpoint() {
        let error = prepare_resumed_session(b"not a checkpoint", None)
            .expect_err("malformed checkpoint must be rejected");

        assert!(matches!(
            error,
            ResumeError::Checkpoint(TaskSessionCheckpointError::InvalidCheckpoint)
        ));
    }

    #[test]
    fn resume_requires_exactly_one_task_before_checking_its_state() {
        let workspace = TempWorkspace::new();
        let mut session = TaskSession::new_in_workspace(workspace.path()).expect("bind workspace");
        session.create_task("pending task");
        session
            .create_task("running task")
            .transition(TaskState::Running)
            .expect("start second task");
        let checkpoint = session.export_checkpoint().expect("export checkpoint");

        let error = prepare_resumed_session(&checkpoint, Some(workspace.path()))
            .expect_err("multiple tasks must be rejected");

        assert!(matches!(error, ResumeError::TaskCount(2)));
    }

    #[test]
    fn resume_rejects_a_single_task_that_is_not_running() {
        let workspace = TempWorkspace::new();
        let mut session = TaskSession::new_in_workspace(workspace.path()).expect("bind workspace");
        session.create_task("pending task");
        let checkpoint = session.export_checkpoint().expect("export checkpoint");

        let error = prepare_resumed_session(&checkpoint, Some(workspace.path()))
            .expect_err("non-running task must be rejected");

        assert!(matches!(
            error,
            ResumeError::TaskNotRunning(TaskState::Pending)
        ));
    }

    #[test]
    fn resume_rejects_a_different_workspace_binding() {
        let stored_workspace = TempWorkspace::new();
        let requested_workspace = TempWorkspace::new();
        let (checkpoint, task_id, _) = running_task_checkpoint(stored_workspace.path());

        let error = prepare_resumed_session(&checkpoint, Some(requested_workspace.path()))
            .expect_err("different workspace identity must be rejected");

        assert!(matches!(
            error,
            ResumeError::Checkpoint(TaskSessionCheckpointError::WorkspaceMismatch {
                task_id: mismatch_id
            }) if mismatch_id == task_id
        ));
    }

    #[test]
    fn resume_without_override_uses_the_current_workspace() {
        let workspace = TempWorkspace::new();
        let (checkpoint, task_id, _) = running_task_checkpoint(workspace.path());
        let _lock = CURRENT_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = RestoreCurrentDir::enter(workspace.path());

        let resumed =
            prepare_resumed_session(&checkpoint, None).expect("restore from the current workspace");

        assert_eq!(resumed.task_id, task_id);
        assert_eq!(resumed.session.list_tasks().len(), 1);
    }
}
