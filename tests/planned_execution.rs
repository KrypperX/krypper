use krypper::agent_loop::{
    run_goal_turn_with_budget, GoalTurnResult, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use krypper::harness::execution::run_goal_turn_with_plan;
use krypper::task_lifecycle::{
    TaskHistoryTransition, TaskSession, TaskState, TurnActivitySummary, TurnOutcome, TurnState,
};
use krypper::{WorkspacePlan, WorkspacePlanError};
#[path = "../src/test_support.rs"]
mod test_support;
use std::fs;
use std::path::{Path, PathBuf};
use test_support::TempDir;
use uuid::Uuid;

fn running_turn(session: &mut TaskSession, goal: &str) -> (Uuid, Uuid) {
    let task = session.create_task(goal);
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (task_id, turn_id)
}

fn session_for(workspace: &Path, goal: &str) -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new_in_workspace(workspace).expect("bind workspace");
    let (task_id, turn_id) = running_turn(&mut session, goal);
    (session, task_id, turn_id)
}

fn assert_records_match(
    planned: &TaskSession,
    planned_task_id: Uuid,
    planned_turn_id: Uuid,
    reactive: &TaskSession,
    reactive_task_id: Uuid,
    reactive_turn_id: Uuid,
) {
    let planned_task = planned.get_task(planned_task_id).expect("planned task");
    let reactive_task = reactive.get_task(reactive_task_id).expect("reactive task");
    let planned_turn = planned_task
        .get_turn(planned_turn_id)
        .expect("planned turn");
    let reactive_turn = reactive_task
        .get_turn(reactive_turn_id)
        .expect("reactive turn");

    assert_eq!(planned_task.state(), reactive_task.state());
    assert_eq!(planned_turn.state(), reactive_turn.state());
    assert_eq!(planned_turn.outcome(), reactive_turn.outcome());
    assert_eq!(
        planned_turn.activity_history(),
        reactive_turn.activity_history()
    );
    assert_eq!(
        planned_task
            .activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        reactive_task
            .activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        planned_task
            .history()
            .last()
            .expect("planned history")
            .transition(),
        TaskHistoryTransition::Turn {
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded | TurnState::Failed,
            ..
        }
    ));
    assert!(matches!(
        reactive_task
            .history()
            .last()
            .expect("reactive history")
            .transition(),
        TaskHistoryTransition::Turn {
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded | TurnState::Failed,
            ..
        }
    ));
}

fn assert_same_observed_result(planned: &GoalTurnResult, reactive: &GoalTurnResult) {
    assert_eq!(planned.observations(), reactive.observations());
    assert_eq!(planned.actions_taken(), reactive.actions_taken());
    assert_eq!(planned.stop_reason(), reactive.stop_reason());
    assert_eq!(planned.outcome(), reactive.outcome());
}

#[test]
fn planned_and_reactive_paths_match_on_observed_success() {
    let root = TempDir::new();
    let planned_workspace = root.path().join("planned");
    let reactive_workspace = root.path().join("reactive");
    fs::create_dir(&planned_workspace).expect("create planned workspace");
    fs::create_dir(&reactive_workspace).expect("create reactive workspace");
    fs::write(planned_workspace.join("input.txt"), "fixture input").expect("write planned input");
    fs::write(reactive_workspace.join("input.txt"), "fixture input").expect("write reactive input");
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "unchanged").expect("write outside sentinel");
    let goal = "Read the fixture and create a result";
    let expected_result = "result created from observed fixture";

    let (mut planned_session, planned_task_id, planned_turn_id) =
        session_for(&planned_workspace, goal);
    let plan = WorkspacePlan::try_from_actions(
        vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from("input.txt"),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("result.txt"),
                contents: "fixture output".to_owned(),
            },
        ],
        2,
    )
    .expect("build bounded workspace plan");
    let mut planned_control_inputs = Vec::new();
    let mut planned_control = |received_goal: &str, observations: &[WorkspaceObservation]| {
        planned_control_inputs.push((received_goal.to_owned(), observations.to_vec()));
        WorkspaceAction::Complete {
            result: expected_result.to_owned(),
        }
    };
    let planned_result = run_goal_turn_with_plan(
        &mut planned_session,
        planned_task_id,
        planned_turn_id,
        goal,
        3,
        Ok(plan),
        &mut planned_control,
    )
    .expect("execute planned turn");

    let (mut reactive_session, reactive_task_id, reactive_turn_id) =
        session_for(&reactive_workspace, goal);
    let mut reactive_decisions = Vec::new();
    let mut reactive_source = |received_goal: &str, observations: &[WorkspaceObservation]| {
        reactive_decisions.push((received_goal.to_owned(), observations.to_vec()));
        match observations.len() {
            0 => WorkspaceAction::ReadFile {
                path: PathBuf::from("input.txt"),
            },
            1 => WorkspaceAction::CreateFile {
                path: PathBuf::from("result.txt"),
                contents: "fixture output".to_owned(),
            },
            _ => WorkspaceAction::Complete {
                result: expected_result.to_owned(),
            },
        }
    };
    let reactive_result = run_goal_turn_with_budget(
        &mut reactive_session,
        reactive_task_id,
        reactive_turn_id,
        goal,
        3,
        &mut reactive_source,
    )
    .expect("execute reactive turn");

    assert_eq!(planned_result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(planned_result.actions_taken(), 3);
    assert_same_observed_result(&planned_result, &reactive_result);
    assert_eq!(planned_control_inputs.len(), 1);
    assert_eq!(planned_control_inputs[0].0, goal);
    assert_eq!(planned_control_inputs[0].1, planned_result.observations());
    assert_eq!(reactive_decisions.len(), 3);
    assert_eq!(reactive_decisions[0].1, []);
    assert_eq!(reactive_decisions[1].1, reactive_result.observations()[..1]);
    assert_eq!(reactive_decisions[2].1, reactive_result.observations());
    assert_eq!(
        planned_result.observations(),
        [
            WorkspaceObservation::FileRead {
                path: PathBuf::from("input.txt"),
                contents: "fixture input".to_owned(),
            },
            WorkspaceObservation::FileCreated {
                path: PathBuf::from("result.txt"),
                bytes_written: "fixture output".len(),
            },
        ]
    );
    assert_eq!(
        fs::read_to_string(planned_workspace.join("result.txt")).expect("planned output"),
        "fixture output"
    );
    assert_eq!(
        fs::read_to_string(reactive_workspace.join("result.txt")).expect("reactive output"),
        "fixture output"
    );
    assert_eq!(
        fs::read_to_string(outside).expect("outside sentinel"),
        "unchanged"
    );
    assert_records_match(
        &planned_session,
        planned_task_id,
        planned_turn_id,
        &reactive_session,
        reactive_task_id,
        reactive_turn_id,
    );
}

#[test]
fn planned_denial_fails_before_mutation_and_reactive_denial_stops_later_operations() {
    let root = TempDir::new();
    let planned_workspace = root.path().join("planned");
    let reactive_workspace = root.path().join("reactive");
    fs::create_dir(&planned_workspace).expect("create planned workspace");
    fs::create_dir(&reactive_workspace).expect("create reactive workspace");
    fs::write(planned_workspace.join("input.txt"), "fixture input").expect("write planned input");
    fs::write(reactive_workspace.join("input.txt"), "fixture input").expect("write reactive input");
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "unchanged").expect("write outside sentinel");
    let goal = "Stop when the workspace denies a planned operation";

    let (mut planned_session, planned_task_id, planned_turn_id) =
        session_for(&planned_workspace, goal);
    let plan = WorkspacePlan::try_from_actions(
        vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from("input.txt"),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("../outside.txt"),
                contents: "must not escape".to_owned(),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("later.txt"),
                contents: "must not run".to_owned(),
            },
        ],
        3,
    )
    .expect("build plan containing denied path");
    let mut control_calls = 0;
    let mut planned_control = |_: &str, _: &[WorkspaceObservation]| {
        control_calls += 1;
        WorkspaceAction::Complete {
            result: "unexpected control call".to_owned(),
        }
    };
    let planned_result = run_goal_turn_with_plan(
        &mut planned_session,
        planned_task_id,
        planned_turn_id,
        goal,
        4,
        Ok(plan),
        &mut planned_control,
    )
    .expect("record denied planned operation");

    let (mut reactive_session, reactive_task_id, reactive_turn_id) =
        session_for(&reactive_workspace, goal);
    let mut reactive_calls = 0;
    let mut reactive_source = |_: &str, observations: &[WorkspaceObservation]| {
        reactive_calls += 1;
        match observations.len() {
            0 => WorkspaceAction::ReadFile {
                path: PathBuf::from("input.txt"),
            },
            1 => WorkspaceAction::CreateFile {
                path: PathBuf::from("../outside.txt"),
                contents: "must not escape".to_owned(),
            },
            _ => WorkspaceAction::CreateFile {
                path: PathBuf::from("later.txt"),
                contents: "must not run".to_owned(),
            },
        }
    };
    let reactive_result = run_goal_turn_with_budget(
        &mut reactive_session,
        reactive_task_id,
        reactive_turn_id,
        goal,
        4,
        &mut reactive_source,
    )
    .expect("record reactive denied operation");

    assert_eq!(
        planned_result.stop_reason(),
        GoalTurnStopReason::ActionFailed
    );
    assert_eq!(planned_result.actions_taken(), 1);
    assert_eq!(
        reactive_result.stop_reason(),
        GoalTurnStopReason::ActionFailed
    );
    assert_eq!(reactive_result.actions_taken(), 2);
    assert_eq!(control_calls, 0);
    assert_eq!(reactive_calls, 2);
    assert!(!planned_workspace.join("later.txt").exists());
    assert!(!reactive_workspace.join("later.txt").exists());
    assert_eq!(
        fs::read_to_string(outside).expect("outside sentinel"),
        "unchanged"
    );
    assert_eq!(
        planned_session
            .get_task(planned_task_id)
            .expect("planned task")
            .get_turn(planned_turn_id)
            .expect("planned turn")
            .state(),
        TurnState::Failed
    );
    assert_eq!(
        reactive_session
            .get_task(reactive_task_id)
            .expect("reactive task")
            .get_turn(reactive_turn_id)
            .expect("reactive turn")
            .state(),
        TurnState::Failed
    );
}

#[test]
fn an_invalid_plan_records_failure_without_executing_any_step() {
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let (mut session, task_id, turn_id) = session_for(&workspace, "reject malformed plan");
    let mut control_calls = 0;
    let mut control = |_: &str, _: &[WorkspaceObservation]| {
        control_calls += 1;
        WorkspaceAction::Complete {
            result: "must not run".to_owned(),
        }
    };

    let result = run_goal_turn_with_plan(
        &mut session,
        task_id,
        turn_id,
        "reject malformed plan",
        4,
        Err(WorkspacePlanError::InvalidOperation),
        &mut control,
    )
    .expect("record invalid plan failure");

    assert_eq!(result.actions_taken(), 1);
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.observations(),
        [WorkspaceObservation::ActionFailed { .. }]
    ));
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert_eq!(control_calls, 0);
    let task = session.get_task(task_id).expect("task");
    let turn = task.get_turn(turn_id).expect("turn");
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(
        turn.activity_history(),
        [TurnActivitySummary::ActionFailed {
            action: krypper::task_lifecycle::TurnActivityAction::Unsupported,
        }]
    );
}

#[test]
fn an_over_budget_plan_stops_before_later_operations() {
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let (mut session, task_id, turn_id) = session_for(&workspace, "enforce plan budget");
    let plan = WorkspacePlan::try_from_actions(
        ["first.txt", "second.txt", "third.txt"]
            .into_iter()
            .map(|path| WorkspaceAction::CreateFile {
                path: PathBuf::from(path),
                contents: path.to_owned(),
            })
            .collect(),
        3,
    )
    .expect("build over-budget plan");
    let mut control_calls = 0;
    let mut control = |_: &str, _: &[WorkspaceObservation]| {
        control_calls += 1;
        WorkspaceAction::Complete {
            result: "must not run".to_owned(),
        }
    };

    let result = run_goal_turn_with_plan(
        &mut session,
        task_id,
        turn_id,
        "enforce plan budget",
        2,
        Ok(plan),
        &mut control,
    )
    .expect("record budget exhaustion");

    assert_eq!(result.actions_taken(), 2);
    assert_eq!(
        result.stop_reason(),
        GoalTurnStopReason::ActionBudgetExhausted
    );
    assert!(workspace.join("first.txt").exists());
    assert!(workspace.join("second.txt").exists());
    assert!(!workspace.join("third.txt").exists());
    assert_eq!(control_calls, 0);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}

#[test]
fn partial_multi_file_failure_preserves_edit_and_failure_evidence() {
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    fs::write(workspace.join("first.txt"), "old first\n").expect("write first file");
    fs::write(workspace.join("second.txt"), "old second\n").expect("write second file");
    let (mut session, task_id, turn_id) = session_for(&workspace, "edit two files");
    let plan = WorkspacePlan::try_from_actions(
        vec![
            WorkspaceAction::ApplyPatch {
                path: PathBuf::from("first.txt"),
                patch: "--- a/first.txt\n+++ b/first.txt\n@@ -1 +1 @@\n-old first\n+new first\n"
                    .to_owned(),
            },
            WorkspaceAction::ApplyPatch {
                path: PathBuf::from("second.txt"),
                patch: "--- a/second.txt\n+++ b/second.txt\n@@ -1 +1 @@\n-not the current text\n+new second\n"
                    .to_owned(),
            },
        ],
        2,
    )
    .expect("build bounded two-file plan");
    let mut control = |_: &str, _: &[WorkspaceObservation]| WorkspaceAction::Complete {
        result: "must not claim success".to_owned(),
    };

    let result = run_goal_turn_with_plan(
        &mut session,
        task_id,
        turn_id,
        "edit two files",
        3,
        Ok(plan),
        &mut control,
    )
    .expect("preserve the failed turn result");

    assert_eq!(
        fs::read_to_string(workspace.join("first.txt")).unwrap(),
        "new first\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("second.txt")).unwrap(),
        "old second\n"
    );
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.observations(),
        [WorkspaceObservation::PatchApplied { path, .. }, WorkspaceObservation::PatchFailed { path: failed_path, .. }]
            if path == Path::new("first.txt") && failed_path == Path::new("second.txt")
    ));
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}
