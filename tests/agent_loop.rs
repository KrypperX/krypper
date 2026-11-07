use krypper::agent_loop::{
    run_goal_turn, run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use krypper::task_lifecycle::{
    TaskHistoryTransition, TaskSession, TaskState, TurnActivityAction, TurnActivitySummary,
    TurnOutcome, TurnState, WorkspaceReadError,
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
        turn.activity_history(),
        [TurnActivitySummary::FileRead { bytes_read: 20 }]
    );
    assert_eq!(
        task.activity_history()[0].summary(),
        TurnActivitySummary::FileRead { bytes_read: 20 }
    );
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
    let mut decision_calls = 0;
    let mut decisions = |_goal: &str| {
        decision_calls += 1;
        WorkspaceAction::ReadFile {
            path: PathBuf::from("note.txt"),
        }
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
    drop(decisions);
    assert_eq!(decision_calls, 0);
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

#[test]
fn bounded_goal_turn_passes_goal_and_ordered_observations_to_each_decision() {
    let workspace = TempDir::new();
    fs::write(workspace.path().join("first.txt"), "first observation").unwrap();
    fs::create_dir(workspace.path().join("docs")).unwrap();
    fs::write(workspace.path().join("docs/guide.txt"), "guide").unwrap();
    let goal = "Inspect the workspace in steps";
    let mut session = TaskSession::new();
    let (task_id, turn_id) =
        with_current_dir(workspace.path(), || running_turn(&mut session, goal));
    let mut decision_inputs = Vec::new();
    let mut decisions = |received_goal: &str, observations: &[WorkspaceObservation]| {
        decision_inputs.push((received_goal.to_owned(), observations.to_vec()));
        match observations.len() {
            0 => WorkspaceAction::ReadFile {
                path: PathBuf::from("first.txt"),
            },
            1 => WorkspaceAction::ListDirectory {
                path: PathBuf::from("docs"),
            },
            _ => WorkspaceAction::Complete {
                result: "workspace inspected".to_owned(),
            },
        }
    };

    let result =
        run_goal_turn_with_budget(&mut session, task_id, turn_id, goal, 3, &mut decisions).unwrap();

    assert_eq!(decision_inputs.len(), 3);
    assert!(decision_inputs
        .iter()
        .all(|(seen_goal, _)| seen_goal == goal));
    assert!(decision_inputs[0].1.is_empty());
    assert_eq!(decision_inputs[1].1, result.observations()[..1]);
    assert_eq!(decision_inputs[2].1, result.observations());
    assert_eq!(result.actions_taken(), 3);
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        result.observations(),
        &[
            WorkspaceObservation::FileRead {
                path: PathBuf::from("first.txt"),
                contents: "first observation".to_owned(),
            },
            WorkspaceObservation::DirectoryListed {
                path: PathBuf::from("docs"),
                entries: vec![std::ffi::OsString::from("guide.txt")],
            },
        ]
    );
    let expected_outcome = TurnOutcome::Success {
        result: "workspace inspected".to_owned(),
    };
    assert_eq!(result.outcome(), &expected_outcome);
    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(turn.state(), TurnState::Succeeded);
    assert_eq!(turn.outcome(), Some(&expected_outcome));
    let expected_activity = [
        TurnActivitySummary::FileRead { bytes_read: 17 },
        TurnActivitySummary::DirectoryListed { entry_count: 1 },
        TurnActivitySummary::Completed,
    ];
    assert_eq!(turn.activity_history(), expected_activity);
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| (event.turn_id(), event.summary()))
            .collect::<Vec<_>>(),
        expected_activity.map(|summary| (turn_id, summary))
    );
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(matches!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Turn {
            turn_id: recorded_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded,
        } if recorded_turn_id == turn_id
    ));
}

#[test]
fn bounded_goal_turn_stops_when_the_action_budget_is_exhausted() {
    let workspace = TempDir::new();
    fs::write(workspace.path().join("note.txt"), "observation").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "Read repeatedly")
    });
    let mut decision_inputs = Vec::new();
    let mut decisions = |goal: &str, observations: &[WorkspaceObservation]| {
        decision_inputs.push((goal.to_owned(), observations.to_vec()));
        WorkspaceAction::ReadFile {
            path: PathBuf::from("note.txt"),
        }
    };

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "Read repeatedly",
        2,
        &mut decisions,
    )
    .unwrap();

    assert_eq!(decision_inputs.len(), 2);
    assert_eq!(decision_inputs[0].1.len(), 0);
    assert_eq!(decision_inputs[1].1.len(), 1);
    assert_eq!(result.actions_taken(), 2);
    assert_eq!(result.observations().len(), 2);
    assert_eq!(
        result.stop_reason(),
        GoalTurnStopReason::ActionBudgetExhausted
    );
    let expected_outcome = TurnOutcome::Failure {
        detail: "action budget exhausted after 2 actions".to_owned(),
    };
    let expected_activity = [
        TurnActivitySummary::FileRead { bytes_read: 11 },
        TurnActivitySummary::FileRead { bytes_read: 11 },
        TurnActivitySummary::BudgetExhausted { actions: 2 },
    ];
    assert_eq!(result.outcome(), &expected_outcome);
    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), Some(&expected_outcome));
    assert_eq!(turn.activity_history(), expected_activity);
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        expected_activity
    );
    assert_eq!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );
}

#[test]
fn bounded_goal_turn_returns_safe_action_failures_as_observations() {
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside_file = root.path().join("outside.txt");
    fs::write(&outside_file, "stay outside").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) =
        with_current_dir(&workspace, || running_turn(&mut session, "Read one file"));
    let mut decisions =
        |_goal: &str, _observations: &[WorkspaceObservation]| WorkspaceAction::ReadFile {
            path: PathBuf::from("../outside.txt"),
        };

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "Read one file",
        2,
        &mut decisions,
    )
    .unwrap();

    assert_eq!(result.actions_taken(), 1);
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.observations(),
        [WorkspaceObservation::ActionFailed {
            error: krypper::agent_loop::GoalTurnError::WorkspaceRead(
                WorkspaceReadError::File { message }
            ),
            ..
        }] if message.contains("path must stay inside the current directory")
    ));
    assert!(matches!(
        result.outcome(),
        TurnOutcome::Failure { detail }
            if detail.contains("path must stay inside the current directory")
    ));
    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), Some(result.outcome()));
    let expected_activity = TurnActivitySummary::ActionFailed {
        action: TurnActivityAction::ReadFile,
    };
    assert_eq!(turn.activity_history(), [expected_activity]);
    assert_eq!(task.activity_history()[0].summary(), expected_activity);
    assert_eq!(fs::read_to_string(outside_file).unwrap(), "stay outside");
}

#[test]
fn bounded_goal_turn_records_unsupported_action_failure_without_action_details() {
    let workspace = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "Perform a supported action")
    });
    let mut decisions =
        |_goal: &str, _observations: &[WorkspaceObservation]| WorkspaceAction::Unsupported {
            action: "write_file secret.txt".to_owned(),
        };

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "Perform a supported action",
        2,
        &mut decisions,
    )
    .unwrap();

    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(
        turn.activity_history(),
        [TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::Unsupported,
        }]
    );
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), Some(result.outcome()));
    assert!(matches!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Turn {
            turn_id: recorded_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        } if recorded_turn_id == turn_id
    ));
}

#[test]
fn bounded_goal_turn_validates_ownership_and_state_before_deciding() {
    let workspace = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(workspace.path(), || {
        let task = session.create_task("Pending task");
        let task_id = task.id();
        let turn_id = task.create_turn().unwrap();
        (task_id, turn_id)
    });
    let (second_task_id, foreign_turn_id) = with_current_dir(workspace.path(), || {
        running_turn(&mut session, "Second task")
    });
    let history = session.get_task(task_id).unwrap().history().to_vec();
    let activity_history = session
        .get_task(task_id)
        .unwrap()
        .activity_history()
        .to_vec();
    let second_history = session.get_task(second_task_id).unwrap().history().to_vec();
    let second_activity_history = session
        .get_task(second_task_id)
        .unwrap()
        .activity_history()
        .to_vec();
    let mut decision_calls = 0;
    let mut decisions = |_goal: &str, _observations: &[WorkspaceObservation]| {
        decision_calls += 1;
        WorkspaceAction::Complete {
            result: "must not run".to_owned(),
        }
    };

    let error = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "Complete this turn",
        2,
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
    assert_eq!(task.activity_history(), activity_history);
    assert_eq!(task.get_turn(turn_id).unwrap().state(), TurnState::Pending);
    assert_eq!(task.get_turn(turn_id).unwrap().outcome(), None);
    assert!(task
        .get_turn(turn_id)
        .unwrap()
        .activity_history()
        .is_empty());

    let error = run_goal_turn_with_budget(
        &mut session,
        task_id,
        foreign_turn_id,
        "Run only for the owner",
        2,
        &mut decisions,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        krypper::agent_loop::GoalTurnError::WorkspaceRead(
            WorkspaceReadError::ForeignTurn {
                task_id: requested_task_id,
                turn_id: actual_turn_id,
                owner_task_id,
            }
        ) if requested_task_id == task_id
            && actual_turn_id == foreign_turn_id
            && owner_task_id == second_task_id
    ));
    drop(decisions);
    assert_eq!(decision_calls, 0);
    assert_eq!(session.get_task(task_id).unwrap().history(), history);
    assert_eq!(
        session.get_task(task_id).unwrap().activity_history(),
        activity_history
    );
    assert_eq!(
        session.get_task(second_task_id).unwrap().history(),
        second_history
    );
    assert_eq!(
        session.get_task(second_task_id).unwrap().activity_history(),
        second_activity_history
    );
    let foreign_turn = session
        .get_task(second_task_id)
        .unwrap()
        .get_turn(foreign_turn_id)
        .unwrap();
    assert_eq!(foreign_turn.state(), TurnState::Running);
    assert_eq!(foreign_turn.outcome(), None);
    assert!(foreign_turn.activity_history().is_empty());
}
