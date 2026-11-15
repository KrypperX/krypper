use krypper::harness::execution::{
    self, GoalTurnError, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use krypper::task_lifecycle::{
    TaskHistoryTransition, TaskSession, TaskState, TurnActivityAction, TurnActivitySummary,
    TurnOutcome, TurnState,
};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("krypper-harness-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("create temporary root");
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).expect("create task workspace");
        Self(workspace)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(self.0.parent().expect("workspace has a temporary root"))
            .expect("remove temporary root");
    }
}

fn start_turn(session: &mut TaskSession, goal: &str) -> (Uuid, Uuid) {
    let task_id = session.create_task(goal).id();
    session
        .get_task_mut(task_id)
        .expect("created task")
        .transition(TaskState::Running)
        .expect("start task");
    let turn_id = session
        .get_task_mut(task_id)
        .expect("running task")
        .create_turn()
        .expect("create turn");
    session
        .get_task_mut(task_id)
        .expect("running task")
        .transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (task_id, turn_id)
}

#[test]
fn harness_execution_keeps_agent_loop_types_and_ordered_task_behavior() {
    let workspace = TempWorkspace::new();
    let mut session = TaskSession::new_in_workspace(&workspace.0).expect("bind task workspace");
    let goal = "Create and read a workspace note";
    let (task_id, turn_id) = start_turn(&mut session, goal);

    let mut observations_seen = Vec::new();
    let mut decisions = |_: &str, observations: &[WorkspaceObservation]| {
        observations_seen.push(observations.to_vec());
        match observations.len() {
            0 => WorkspaceAction::CreateFile {
                path: PathBuf::from("note.txt"),
                contents: "same task-turn behavior".to_owned(),
            },
            1 => WorkspaceAction::ReadFile {
                path: PathBuf::from("note.txt"),
            },
            _ => WorkspaceAction::Complete {
                result: "done".to_owned(),
            },
        }
    };

    let result: execution::GoalTurnResult = execution::run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        goal,
        3,
        &mut decisions,
    )
    .expect("run bounded task turn");

    assert_eq!(
        observations_seen,
        vec![
            vec![],
            vec![WorkspaceObservation::FileCreated {
                path: PathBuf::from("note.txt"),
                bytes_written: "same task-turn behavior".len(),
            }],
            vec![
                WorkspaceObservation::FileCreated {
                    path: PathBuf::from("note.txt"),
                    bytes_written: "same task-turn behavior".len(),
                },
                WorkspaceObservation::FileRead {
                    path: PathBuf::from("note.txt"),
                    contents: "same task-turn behavior".to_owned(),
                },
            ],
        ]
    );
    assert_eq!(result.actions_taken(), 3);
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        result.outcome(),
        Some(&TurnOutcome::Success {
            result: "done".to_owned(),
        })
    );
    assert_eq!(result.read_result(), "same task-turn behavior");
    assert_eq!(
        result.observations(),
        &[
            execution::WorkspaceObservation::FileCreated {
                path: PathBuf::from("note.txt"),
                bytes_written: "same task-turn behavior".len(),
            },
            execution::WorkspaceObservation::FileRead {
                path: PathBuf::from("note.txt"),
                contents: "same task-turn behavior".to_owned(),
            },
        ]
    );

    let task = session.get_task(task_id).expect("task remains in session");
    let turn = task.get_turn(turn_id).expect("turn remains in task");
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(turn.state(), TurnState::Succeeded);
    assert_eq!(
        turn.activity_history(),
        [
            TurnActivitySummary::FileCreated {
                bytes_written: "same task-turn behavior".len(),
            },
            TurnActivitySummary::FileRead {
                bytes_read: "same task-turn behavior".len(),
            },
            TurnActivitySummary::Completed,
        ]
    );
    assert_eq!(turn.outcome(), result.outcome());
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        turn.activity_history().to_vec()
    );
    assert_eq!(
        task.history()
            .last()
            .expect("turn lifecycle event")
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded,
        }
    );
}

#[test]
fn harness_execution_records_decision_budget_stop_and_failure_outcome() {
    let workspace = TempWorkspace::new();
    let mut session = TaskSession::new_in_workspace(&workspace.0).expect("bind task workspace");
    let goal = "Create a file before the decision budget ends";
    let (task_id, turn_id) = start_turn(&mut session, goal);
    let contents = "created before budget";
    let mut decisions = 0;
    let mut decision_source = |_: &str, _: &[WorkspaceObservation]| {
        decisions += 1;
        WorkspaceAction::CreateFile {
            path: PathBuf::from("budget.txt"),
            contents: contents.to_owned(),
        }
    };

    let result = execution::run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        goal,
        1,
        &mut decision_source,
    )
    .expect("run bounded task turn");

    assert_eq!(decisions, 1);
    assert_eq!(result.actions_taken(), 1);
    assert_eq!(
        result.stop_reason(),
        GoalTurnStopReason::ActionBudgetExhausted
    );
    assert_eq!(
        result.outcome(),
        Some(&TurnOutcome::Failure {
            detail: "action budget exhausted after 1 actions".to_owned(),
        })
    );
    assert_eq!(
        result.observations(),
        &[WorkspaceObservation::FileCreated {
            path: PathBuf::from("budget.txt"),
            bytes_written: contents.len(),
        }]
    );
    assert_eq!(
        fs::read_to_string(workspace.0.join("budget.txt")).expect("created file"),
        contents
    );

    let task = session.get_task(task_id).expect("task remains in session");
    let turn = task.get_turn(turn_id).expect("turn remains in task");
    let activity = [
        TurnActivitySummary::FileCreated {
            bytes_written: contents.len(),
        },
        TurnActivitySummary::BudgetExhausted { actions: 1 },
    ];
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), result.outcome());
    assert_eq!(turn.activity_history(), activity);
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        activity
    );
    assert_eq!(
        task.history()
            .last()
            .expect("turn lifecycle event")
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );
}

#[test]
fn harness_execution_rejects_workspace_escape_without_changing_outside_file() {
    let workspace = TempWorkspace::new();
    let outside_file = workspace
        .0
        .parent()
        .expect("workspace has a temporary root")
        .join("outside.txt");
    fs::write(&outside_file, "original contents").expect("write outside sentinel");
    let mut session = TaskSession::new_in_workspace(&workspace.0).expect("bind task workspace");
    let goal = "Create a file inside the task workspace";
    let (task_id, turn_id) = start_turn(&mut session, goal);
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| WorkspaceAction::CreateFile {
        path: PathBuf::from("../outside.txt"),
        contents: "must not overwrite the sentinel".to_owned(),
    };

    let result = execution::run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        goal,
        2,
        &mut decisions,
    )
    .expect("run bounded task turn");

    assert_eq!(result.actions_taken(), 1);
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.observations(),
        [WorkspaceObservation::FileCreationFailed { path, error }]
            if path == &PathBuf::from("../outside.txt")
                && matches!(error, GoalTurnError::WorkspaceWrite(_))
    ));
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert_eq!(
        fs::read_to_string(&outside_file).expect("outside sentinel remains"),
        "original contents"
    );

    let task = session.get_task(task_id).expect("task remains in session");
    let turn = task.get_turn(turn_id).expect("turn remains in task");
    let activity = [TurnActivitySummary::ActionFailed {
        action: TurnActivityAction::CreateFile,
    }];
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), result.outcome());
    assert_eq!(turn.activity_history(), activity);
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        activity
    );
    assert_eq!(
        task.history()
            .last()
            .expect("turn lifecycle event")
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );
}
