use krypper::agent_loop::{run_goal_turn, WorkspaceAction};
use krypper::task_lifecycle::{
    TaskHistoryTransition, TaskSession, TaskState, TurnOutcome, TurnState, WorkspaceReadError,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-agent-loop-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct RestoreCurrentDir(PathBuf);

impl RestoreCurrentDir {
    fn enter(path: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(previous)
    }
}

impl Drop for RestoreCurrentDir {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

fn with_current_dir<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    let _lock = CURRENT_DIR_LOCK.lock().unwrap();
    let _restore = RestoreCurrentDir::enter(path);
    operation()
}

fn running_turn(session: &mut TaskSession, goal: &str) -> (Uuid, Uuid) {
    let task = session.create_task(goal);
    task.transition(TaskState::Running).unwrap();
    let task_id = task.id();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();
    (task_id, turn_id)
}

#[test]
fn goal_turn_reads_one_file_and_records_the_matching_outcome() {
    let workspace = TempDir::new();
    fs::write(workspace.path().join("note.txt"), "bounded task context").unwrap();
    let goal = "Read the task context";
    let mut session = TaskSession::new();
    let (task_id, turn_id) =
        with_current_dir(workspace.path(), || running_turn(&mut session, goal));
    let mut received_goal = None;
    let mut decisions_called = 0;
    let mut decisions = |selected_goal: &str| {
        decisions_called += 1;
        received_goal = Some(selected_goal.to_owned());
        WorkspaceAction::ReadFile {
            path: PathBuf::from("note.txt"),
        }
    };

    let result = run_goal_turn(&mut session, task_id, turn_id, goal, &mut decisions).unwrap();

    assert_eq!(received_goal.as_deref(), Some(goal));
    assert_eq!(decisions_called, 1);
    assert_eq!(result.task_id(), task_id);
    assert_eq!(result.turn_id(), turn_id);
    assert_eq!(result.read_result(), "bounded task context");
    let expected_outcome = TurnOutcome::Success {
        result: "bounded task context".to_owned(),
    };
    assert_eq!(result.outcome(), &expected_outcome);
    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(turn.state(), TurnState::Succeeded);
    assert_eq!(turn.outcome(), Some(&expected_outcome));
    assert_eq!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded,
        }
    );
}

#[test]
fn rejected_workspace_read_fails_the_turn_with_a_matching_outcome() {
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside_file = root.path().join("outside.txt");
    fs::write(&outside_file, "stay outside").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) =
        with_current_dir(&workspace, || running_turn(&mut session, "Read a file"));
    let mut decisions = |_goal: &str| WorkspaceAction::ReadFile {
        path: PathBuf::from("../outside.txt"),
    };

    let error = run_goal_turn(
        &mut session,
        task_id,
        turn_id,
        "Read a file",
        &mut decisions,
    )
    .unwrap_err();

    assert!(error
        .to_string()
        .contains("path must stay inside the current directory"));
    let turn = session
        .get_task(task_id)
        .unwrap()
        .get_turn(turn_id)
        .unwrap();
    assert_eq!(turn.state(), TurnState::Failed);
    let outcome = TurnOutcome::Failure {
        detail: error.to_string(),
    };
    assert_eq!(turn.outcome(), Some(&outcome));
    assert_eq!(
        session
            .get_task(task_id)
            .unwrap()
            .history()
            .last()
            .unwrap()
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );
    assert_eq!(fs::read_to_string(outside_file).unwrap(), "stay outside");
}

#[test]
fn foreign_turn_errors_leave_both_tasks_and_histories_unchanged() {
    let workspace = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, _) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "First task")
    });
    let (second_task_id, foreign_turn_id) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "Second task")
    });
    let first_history = session.get_task(task_id).unwrap().history().to_vec();
    let second_history = session.get_task(second_task_id).unwrap().history().to_vec();
    let mut decisions = |_goal: &str| WorkspaceAction::ReadFile {
        path: PathBuf::from("note.txt"),
    };

    let error = run_goal_turn(
        &mut session,
        task_id,
        foreign_turn_id,
        "Read a file",
        &mut decisions,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        krypper::agent_loop::GoalTurnError::WorkspaceRead(WorkspaceReadError::ForeignTurn { .. })
    ));
    assert_eq!(session.get_task(task_id).unwrap().history(), first_history);
    assert_eq!(
        session.get_task(second_task_id).unwrap().history(),
        second_history
    );
    assert_eq!(
        session
            .get_task(second_task_id)
            .unwrap()
            .get_turn(foreign_turn_id)
            .unwrap()
            .state(),
        TurnState::Running
    );
    assert_eq!(
        session
            .get_task(second_task_id)
            .unwrap()
            .get_turn(foreign_turn_id)
            .unwrap()
            .outcome(),
        None
    );
}

#[test]
fn nonrunning_turn_errors_leave_its_state_and_history_unchanged() {
    let workspace = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(workspace.path(), || {
        let task = session.create_task("Pending task");
        let task_id = task.id();
        let turn_id = task.create_turn().unwrap();
        (task_id, turn_id)
    });
    let history = session.get_task(task_id).unwrap().history().to_vec();
    let mut decisions = |_goal: &str| WorkspaceAction::ReadFile {
        path: PathBuf::from("note.txt"),
    };

    let error = run_goal_turn(
        &mut session,
        task_id,
        turn_id,
        "Read a file",
        &mut decisions,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        krypper::agent_loop::GoalTurnError::WorkspaceRead(
            WorkspaceReadError::TurnNotRunning {
                turn_id: actual_turn_id,
                state: TurnState::Pending,
            }
        ) if actual_turn_id == turn_id
    ));
    let task = session.get_task(task_id).unwrap();
    assert_eq!(task.history(), history);
    assert_eq!(task.get_turn(turn_id).unwrap().state(), TurnState::Pending);
    assert_eq!(task.get_turn(turn_id).unwrap().outcome(), None);
}

#[test]
fn unsupported_action_fails_the_turn_without_running_a_workspace_operation() {
    let workspace = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "Create a file")
    });
    let mut received_goal = None;
    let mut decisions = |goal: &str| {
        received_goal = Some(goal.to_owned());
        WorkspaceAction::Unsupported {
            action: "write_file".to_owned(),
        }
    };

    let error = run_goal_turn(
        &mut session,
        task_id,
        turn_id,
        "Create a file",
        &mut decisions,
    )
    .unwrap_err();

    assert_eq!(received_goal.as_deref(), Some("Create a file"));
    assert!(matches!(
        error,
        krypper::agent_loop::GoalTurnError::UnsupportedAction { ref action }
            if action == "write_file"
    ));
    let turn = session
        .get_task(task_id)
        .unwrap()
        .get_turn(turn_id)
        .unwrap();
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(
        turn.outcome(),
        Some(&TurnOutcome::Failure {
            detail: error.to_string(),
        })
    );
    assert_eq!(
        session
            .get_task(task_id)
            .unwrap()
            .history()
            .last()
            .unwrap()
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );
    assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}
