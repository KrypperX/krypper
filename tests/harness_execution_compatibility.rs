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

#[allow(dead_code)]
fn compatibility_type_paths_are_identical(
    action: krypper::agent_loop::WorkspaceAction,
    result: krypper::agent_loop::GoalTurnResult,
    failure: krypper::agent_loop::DecisionFailureCode,
) -> (
    krypper::harness::execution::WorkspaceAction,
    krypper::harness::execution::GoalTurnResult,
    krypper::harness::execution::DecisionFailureCode,
) {
    (action, result, failure)
}

#[allow(dead_code)]
fn completion_types_keep_the_verification_path(
    verification: krypper::harness::verification::CompletionVerification,
) -> krypper::harness::verification::CompletionVerification {
    verification
}

#[path = "../src/test_support.rs"]
mod test_support;

struct TempWorkspace {
    _root: test_support::TempDir,
    workspace: PathBuf,
}

impl TempWorkspace {
    fn new() -> Self {
        let root = test_support::TempDir::with_prefix("krypper-harness-");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("create task workspace");
        Self {
            _root: root,
            workspace,
        }
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
    let mut session =
        TaskSession::new_in_workspace(&workspace.workspace).expect("bind task workspace");
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
    let mut session =
        TaskSession::new_in_workspace(&workspace.workspace).expect("bind task workspace");
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
        fs::read_to_string(workspace.workspace.join("budget.txt")).expect("created file"),
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
    let outside_file = workspace._root.path().join("outside.txt");
    fs::write(&outside_file, "original contents").expect("write outside sentinel");
    let mut session =
        TaskSession::new_in_workspace(&workspace.workspace).expect("bind task workspace");
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

#[test]
fn harness_execution_keeps_success_and_denied_cross_run_read_isolated() {
    const ACTION_BUDGET: usize = 3;
    const SEED: &str = "equivalent starting contents";
    const SENTINEL: &str = "private sentinel";

    let workspace_group = TempWorkspace::new();
    let root = workspace_group._root.path();
    let success_workspace = root.join("success");
    let failed_workspace = root.join("failure");
    fs::create_dir(&success_workspace).expect("create success workspace");
    fs::create_dir(&failed_workspace).expect("create failure workspace");
    for workspace in [&success_workspace, &failed_workspace] {
        fs::write(workspace.join("seed.txt"), SEED).expect("write equivalent seed");
        fs::write(workspace.join("outside-sentinel.txt"), SENTINEL)
            .expect("write equivalent sentinel");
    }
    let outside_sentinel = success_workspace.join("outside-sentinel.txt");

    let success_goal = "case success: read the seed and write a private result";
    let mut success_session =
        TaskSession::new_in_workspace(&success_workspace).expect("bind success workspace");
    let (success_task_id, success_turn_id) = start_turn(&mut success_session, success_goal);
    let success_result = "success result";
    let mut success_decision_records = Vec::new();
    let success = {
        let mut success_decisions = |goal: &str, observations: &[WorkspaceObservation]| {
            let action = match observations.len() {
                0 => WorkspaceAction::ReadFile {
                    path: PathBuf::from("seed.txt"),
                },
                1 => WorkspaceAction::CreateFile {
                    path: PathBuf::from("success.txt"),
                    contents: success_result.to_owned(),
                },
                2 => WorkspaceAction::Complete {
                    result: success_result.to_owned(),
                },
                _ => panic!("success case received an unexpected decision"),
            };
            success_decision_records.push((goal.to_owned(), observations.to_vec(), action.clone()));
            action
        };
        execution::run_goal_turn_with_budget(
            &mut success_session,
            success_task_id,
            success_turn_id,
            success_goal,
            ACTION_BUDGET,
            &mut success_decisions,
        )
        .expect("run success case")
    };
    assert_eq!(success.task_id(), success_task_id);
    assert_eq!(success.turn_id(), success_turn_id);
    assert_eq!(success.actions_taken(), ACTION_BUDGET);
    assert_eq!(success.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        success.outcome(),
        Some(&TurnOutcome::Success {
            result: success_result.to_owned(),
        })
    );
    let success_observations = vec![
        WorkspaceObservation::FileRead {
            path: PathBuf::from("seed.txt"),
            contents: SEED.to_owned(),
        },
        WorkspaceObservation::FileCreated {
            path: PathBuf::from("success.txt"),
            bytes_written: success_result.len(),
        },
    ];
    assert_eq!(success.observations(), success_observations.as_slice());
    assert_eq!(
        success_decision_records,
        vec![
            (
                success_goal.to_owned(),
                vec![],
                WorkspaceAction::ReadFile {
                    path: PathBuf::from("seed.txt"),
                },
            ),
            (
                success_goal.to_owned(),
                vec![success_observations[0].clone()],
                WorkspaceAction::CreateFile {
                    path: PathBuf::from("success.txt"),
                    contents: success_result.to_owned(),
                },
            ),
            (
                success_goal.to_owned(),
                success_observations.clone(),
                WorkspaceAction::Complete {
                    result: success_result.to_owned(),
                },
            ),
        ]
    );
    let success_task = success_session
        .get_task(success_task_id)
        .expect("success task remains in its session");
    let success_turn = success_task
        .get_turn(success_turn_id)
        .expect("success turn remains in its task");
    let success_activity = [
        TurnActivitySummary::FileRead {
            bytes_read: SEED.len(),
        },
        TurnActivitySummary::FileCreated {
            bytes_written: success_result.len(),
        },
        TurnActivitySummary::Completed,
    ];
    assert_eq!(success_turn.state(), TurnState::Succeeded);
    assert_eq!(success_task.state(), TaskState::Running);
    assert_eq!(success_turn.outcome(), success.outcome());
    assert_eq!(success_turn.activity_history(), success_activity);
    assert_eq!(
        success_task
            .activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        success_activity
    );
    assert_eq!(
        success_task
            .history()
            .last()
            .expect("success lifecycle record")
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id: success_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded,
        }
    );

    let failure_goal = "case denied cross-run read: keep another run's sentinel private";
    let peer_sentinel_path = PathBuf::from("../success/outside-sentinel.txt");
    let mut failure_session =
        TaskSession::new_in_workspace(&failed_workspace).expect("bind failure workspace");
    let (failure_task_id, failure_turn_id) = start_turn(&mut failure_session, failure_goal);
    let failure_result = "failure case result";
    let mut failure_decision_records = Vec::new();
    let failure = {
        let mut failure_decisions = |goal: &str, observations: &[WorkspaceObservation]| {
            let action = match observations.len() {
                0 => WorkspaceAction::ReadFile {
                    path: PathBuf::from("seed.txt"),
                },
                1 => WorkspaceAction::CreateFile {
                    path: PathBuf::from("failure.txt"),
                    contents: failure_result.to_owned(),
                },
                2 => WorkspaceAction::ReadFile {
                    path: peer_sentinel_path.clone(),
                },
                _ => panic!("failure case received an unexpected decision"),
            };
            failure_decision_records.push((goal.to_owned(), observations.to_vec(), action.clone()));
            action
        };
        execution::run_goal_turn_with_budget(
            &mut failure_session,
            failure_task_id,
            failure_turn_id,
            failure_goal,
            ACTION_BUDGET,
            &mut failure_decisions,
        )
        .expect("record denied cross-run read as a failed turn")
    };
    assert_ne!(success_task_id, failure_task_id);
    assert_eq!(failure.task_id(), failure_task_id);
    assert_eq!(failure.turn_id(), failure_turn_id);
    assert_eq!(failure.actions_taken(), ACTION_BUDGET);
    assert_eq!(failure.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        failure.outcome(),
        Some(TurnOutcome::Failure { detail })
            if !detail.is_empty() && !detail.contains(SENTINEL)
    ));
    assert_eq!(failure.read_result(), SEED);
    assert!(!failure.read_result().contains(SENTINEL));
    let failure_observations = vec![
        WorkspaceObservation::FileRead {
            path: PathBuf::from("seed.txt"),
            contents: SEED.to_owned(),
        },
        WorkspaceObservation::FileCreated {
            path: PathBuf::from("failure.txt"),
            bytes_written: failure_result.len(),
        },
        WorkspaceObservation::ActionFailed {
            action: WorkspaceAction::ReadFile {
                path: peer_sentinel_path.clone(),
            },
            error: match failure.observations().last() {
                Some(WorkspaceObservation::ActionFailed { error, .. }) => error.clone(),
                observation => {
                    panic!("expected a denied cross-run read, got {observation:?}")
                }
            },
        },
    ];
    assert_eq!(failure.observations(), failure_observations.as_slice());
    assert!(matches!(
        failure.observations().last(),
        Some(WorkspaceObservation::ActionFailed {
            action: WorkspaceAction::ReadFile { path },
            error: GoalTurnError::WorkspaceRead(_),
        }) if path == &peer_sentinel_path
    ));
    assert_eq!(
        failure_decision_records,
        vec![
            (
                failure_goal.to_owned(),
                vec![],
                WorkspaceAction::ReadFile {
                    path: PathBuf::from("seed.txt"),
                },
            ),
            (
                failure_goal.to_owned(),
                vec![failure_observations[0].clone()],
                WorkspaceAction::CreateFile {
                    path: PathBuf::from("failure.txt"),
                    contents: failure_result.to_owned(),
                },
            ),
            (
                failure_goal.to_owned(),
                failure_observations[..2].to_vec(),
                WorkspaceAction::ReadFile {
                    path: peer_sentinel_path.clone(),
                },
            ),
        ]
    );
    let failure_task = failure_session
        .get_task(failure_task_id)
        .expect("failure task remains in its session");
    let failure_turn = failure_task
        .get_turn(failure_turn_id)
        .expect("failure turn remains in its task");
    let failure_activity = [
        TurnActivitySummary::FileRead {
            bytes_read: SEED.len(),
        },
        TurnActivitySummary::FileCreated {
            bytes_written: failure_result.len(),
        },
        TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::ReadFile,
        },
    ];
    assert_eq!(failure_turn.state(), TurnState::Failed);
    assert_eq!(failure_task.state(), TaskState::Running);
    assert_eq!(failure_turn.outcome(), failure.outcome());
    assert_eq!(failure_turn.activity_history(), failure_activity);
    assert_eq!(
        failure_task
            .activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        failure_activity
    );
    assert_eq!(
        failure_task
            .history()
            .last()
            .expect("failure lifecycle record")
            .transition(),
        TaskHistoryTransition::Turn {
            turn_id: failure_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Failed,
        }
    );

    assert_eq!(
        fs::read_to_string(&outside_sentinel).expect("outside-workspace sentinel remains"),
        SENTINEL
    );
    assert_eq!(
        fs::read_to_string(success_workspace.join("success.txt"))
            .expect("success result stays in its workspace"),
        success_result
    );
    assert!(!failed_workspace.join("success.txt").exists());
    assert_eq!(
        fs::read_to_string(failed_workspace.join("failure.txt"))
            .expect("failure result stays in its workspace"),
        failure_result
    );
    assert!(!success_workspace.join("failure.txt").exists());
    assert_eq!(
        fs::read_to_string(success_workspace.join("seed.txt")).expect("success seed remains"),
        SEED
    );
    assert_eq!(
        fs::read_to_string(failed_workspace.join("seed.txt")).expect("failure seed remains"),
        SEED
    );
    assert_eq!(
        fs::read_to_string(failed_workspace.join("outside-sentinel.txt"))
            .expect("failure workspace sentinel remains"),
        SENTINEL
    );
}
