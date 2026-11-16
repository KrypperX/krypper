use krypper::harness::execution::{
    run_goal_turn_with_budget, run_goal_turn_with_completion_correction, WorkspaceAction,
    WorkspaceObservation,
};
use krypper::harness::verification::{
    CompletionCheckEvidence, CompletionCheckEvidenceStatus, CompletionEvidenceSource,
    CompletionVerificationStatus, RequiredCompletionCheck, MAX_COMPLETION_CHECKS,
};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnState};
use krypper::{WorkspacePlan, WorkspacePlanError};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use uuid::Uuid;

struct TempWorkspace {
    root: PathBuf,
    workspace: PathBuf,
}

impl TempWorkspace {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("krypper-completion-correction-{}", Uuid::new_v4()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create fixture workspace");
        Self { root, workspace }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove fixture workspace");
    }
}

fn running_turn(workspace: &Path, goal: &str) -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new_in_workspace(workspace).expect("bind fixture workspace");
    let task_id = session.create_task(goal).id();
    let task = session.get_task_mut(task_id).expect("created task");
    task.transition(TaskState::Running).expect("start task");
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (session, task_id, turn_id)
}

fn required_check() -> RequiredCompletionCheck {
    RequiredCompletionCheck {
        id: "artifact_exists".to_owned(),
        description: "The expected fixture artifact exists".to_owned(),
    }
}

fn evidence(status: CompletionCheckEvidenceStatus, summary: &str) -> Vec<CompletionCheckEvidence> {
    vec![CompletionCheckEvidence {
        check_id: "artifact_exists".to_owned(),
        status,
        source: CompletionEvidenceSource::DeterministicLocalCheck,
        summary: summary.to_owned(),
    }]
}

fn complete_action() -> WorkspaceAction {
    WorkspaceAction::Complete {
        result: "initial work complete".to_owned(),
    }
}

#[test]
fn initial_passing_checks_succeed_without_requesting_a_correction() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
    let mut planner_calls = 0;

    let result = run_goal_turn_with_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        4,
        None,
        &[required_check()],
        |_, _| evidence(CompletionCheckEvidenceStatus::Passed, "fixture passed"),
        |_, _, _, _, _, _| {
            planner_calls += 1;
            Err(WorkspacePlanError::InvalidResponse)
        },
        &mut decisions,
    )
    .expect("run opt-in completion flow");

    assert_eq!(planner_calls, 0);
    assert!(result.correction_result().is_none());
    assert_eq!(
        result.initial_verification().status,
        CompletionVerificationStatus::Passed
    );
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Succeeded
    );
}

#[test]
fn failed_or_unknown_check_gets_one_correction_then_passes() {
    for initial_status in [
        CompletionCheckEvidenceStatus::Failed,
        CompletionCheckEvidenceStatus::Unknown,
    ] {
        let workspace = TempWorkspace::new();
        let artifact = workspace.workspace.join("expected.txt");
        let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
        let remaining_decisions = Rc::new(Cell::new(4));
        let decision_counter = Rc::clone(&remaining_decisions);
        let mut decisions = move |_: &str, _: &[WorkspaceObservation]| {
            decision_counter.set(decision_counter.get().saturating_sub(1));
            complete_action()
        };
        let mut checks = 0;
        let mut planner_calls = 0;
        let planner_counter = Rc::clone(&remaining_decisions);

        let result = run_goal_turn_with_completion_correction(
            &mut session,
            task_id,
            turn_id,
            "fixture task",
            4,
            None,
            &[required_check()],
            |_, _| {
                checks += 1;
                if artifact.is_file() {
                    evidence(CompletionCheckEvidenceStatus::Passed, "expected.txt exists")
                } else if checks == 1 {
                    evidence(initial_status, "initial fixture check did not pass")
                } else {
                    evidence(
                        CompletionCheckEvidenceStatus::Failed,
                        "expected.txt is absent",
                    )
                }
            },
            |_, session, initial_result, verification, _, remaining_actions| {
                planner_calls += 1;
                assert_eq!(
                    session
                        .get_task(initial_result.task_id())
                        .expect("task remains in session")
                        .state(),
                    TaskState::Running
                );
                assert_eq!(
                    verification.status,
                    match initial_status {
                        CompletionCheckEvidenceStatus::Failed => {
                            CompletionVerificationStatus::Failed
                        }
                        CompletionCheckEvidenceStatus::Unknown => {
                            CompletionVerificationStatus::Unknown
                        }
                        CompletionCheckEvidenceStatus::Passed => unreachable!(),
                    }
                );
                assert_eq!(remaining_actions, 3);
                planner_counter.set(planner_counter.get().saturating_sub(2));
                WorkspacePlan::try_from_actions(
                    vec![WorkspaceAction::CreateFile {
                        path: PathBuf::from("expected.txt"),
                        contents: "fixture".to_owned(),
                    }],
                    1,
                )
            },
            &mut decisions,
        )
        .expect("run one bounded correction");

        assert_eq!(planner_calls, 1);
        assert_eq!(checks, 2);
        assert_eq!(remaining_decisions.get(), 0);
        assert_eq!(
            result
                .correction_result()
                .expect("correction ran")
                .actions_taken(),
            2
        );
        assert!(
            result.initial_result().actions_taken()
                + result
                    .correction_result()
                    .expect("correction ran")
                    .actions_taken()
                <= 4
        );
        assert_eq!(
            result.final_verification().status,
            CompletionVerificationStatus::Passed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Succeeded
        );
    }
}

#[test]
fn failed_checks_after_the_single_correction_leave_the_task_failed() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
    let mut checks = 0;
    let mut planner_calls = 0;

    let result = run_goal_turn_with_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        4,
        None,
        &[required_check()],
        |_, _| {
            checks += 1;
            evidence(
                CompletionCheckEvidenceStatus::Failed,
                "fixture remains incomplete",
            )
        },
        |_, _, _, _, _, _| {
            planner_calls += 1;
            WorkspacePlan::try_from_actions(
                vec![WorkspaceAction::CreateFile {
                    path: PathBuf::from("attempted.txt"),
                    contents: "one attempt".to_owned(),
                }],
                1,
            )
        },
        &mut decisions,
    )
    .expect("run one correction");

    assert_eq!(planner_calls, 1);
    assert_eq!(checks, 2);
    assert_eq!(
        result.final_verification().status,
        CompletionVerificationStatus::Failed
    );
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn duplicate_or_truncated_post_correction_evidence_cannot_succeed() {
    for duplicate in [true, false] {
        let workspace = TempWorkspace::new();
        let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
        let mut checks = 0;

        let result = run_goal_turn_with_completion_correction(
            &mut session,
            task_id,
            turn_id,
            "fixture task",
            4,
            None,
            &[required_check()],
            |_, _| {
                checks += 1;
                if checks == 1 {
                    return evidence(
                        CompletionCheckEvidenceStatus::Failed,
                        "initial fixture check failed",
                    );
                }

                if duplicate {
                    vec![
                        CompletionCheckEvidence {
                            check_id: "artifact_exists".to_owned(),
                            status: CompletionCheckEvidenceStatus::Passed,
                            source: CompletionEvidenceSource::DeterministicLocalCheck,
                            summary: "first passing record".to_owned(),
                        },
                        CompletionCheckEvidence {
                            check_id: "artifact_exists".to_owned(),
                            status: CompletionCheckEvidenceStatus::Passed,
                            source: CompletionEvidenceSource::DeterministicLocalCheck,
                            summary: "second passing record".to_owned(),
                        },
                    ]
                } else {
                    (0..=MAX_COMPLETION_CHECKS)
                        .map(|index| CompletionCheckEvidence {
                            check_id: if index == 0 {
                                "artifact_exists".to_owned()
                            } else {
                                format!("extra_{index}")
                            },
                            status: CompletionCheckEvidenceStatus::Passed,
                            source: CompletionEvidenceSource::DeterministicLocalCheck,
                            summary: "fixture pass".to_owned(),
                        })
                        .collect()
                }
            },
            |_, _, _, _, _, _| {
                WorkspacePlan::try_from_actions(
                    vec![WorkspaceAction::CreateFile {
                        path: PathBuf::from("attempted.txt"),
                        contents: "one correction".to_owned(),
                    }],
                    1,
                )
            },
            &mut decisions,
        )
        .expect("run one correction and check its evidence");

        assert_eq!(
            result.final_verification().status,
            CompletionVerificationStatus::Unknown
        );
        if duplicate {
            assert_eq!(result.final_verification().omitted_evidence, 0);
        } else {
            assert_eq!(result.final_verification().omitted_evidence, 1);
        }
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Failed
        );
    }
}

#[test]
fn denied_correction_uses_task_session_safety_and_stops() {
    let workspace = TempWorkspace::new();
    let outside = workspace.root.join("outside.txt");
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
    let mut planner_calls = 0;
    let mut checks = 0;

    let result = run_goal_turn_with_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        4,
        None,
        &[required_check()],
        |_, _| {
            checks += 1;
            evidence(
                CompletionCheckEvidenceStatus::Failed,
                "fixture check failed",
            )
        },
        |_, _, _, _, _, _| {
            planner_calls += 1;
            WorkspacePlan::try_from_actions(
                vec![WorkspaceAction::CreateFile {
                    path: PathBuf::from("../outside.txt"),
                    contents: "must not escape".to_owned(),
                }],
                1,
            )
        },
        &mut decisions,
    )
    .expect("run denied correction through Harness");

    let correction = result
        .correction_result()
        .expect("correction was attempted");
    assert_eq!(planner_calls, 1);
    assert_eq!(checks, 2);
    assert_eq!(correction.actions_taken(), 1);
    assert!(matches!(
        correction.stop_reason(),
        krypper::harness::execution::GoalTurnStopReason::ActionFailed
    ));
    assert!(!outside.exists());
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn invalid_correction_plan_stops_after_one_attempt() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
    let mut planner_calls = 0;

    let result = run_goal_turn_with_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        4,
        None,
        &[required_check()],
        |_, _| {
            evidence(
                CompletionCheckEvidenceStatus::Failed,
                "fixture check failed",
            )
        },
        |_, _, _, _, _, _| {
            planner_calls += 1;
            Err(WorkspacePlanError::InvalidOperation)
        },
        &mut decisions,
    )
    .expect("record invalid plan as a failed correction turn");

    let correction = result
        .correction_result()
        .expect("one correction attempted");
    assert_eq!(planner_calls, 1);
    assert_eq!(correction.actions_taken(), 1);
    assert!(matches!(
        correction.stop_reason(),
        krypper::harness::execution::GoalTurnStopReason::ActionFailed
    ));
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn exhausted_action_budget_does_not_request_a_correction() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| complete_action();
    let mut planner_calls = 0;

    let result = run_goal_turn_with_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        1,
        None,
        &[required_check()],
        |_, _| {
            evidence(
                CompletionCheckEvidenceStatus::Failed,
                "fixture check failed",
            )
        },
        |_, _, _, _, _, _| {
            planner_calls += 1;
            Err(WorkspacePlanError::InvalidResponse)
        },
        &mut decisions,
    )
    .expect("finish without exceeding the action budget");

    assert_eq!(planner_calls, 0);
    assert!(result.correction_result().is_none());
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn reactive_default_execution_and_workspace_safety_are_unchanged() {
    let workspace = TempWorkspace::new();
    let artifact = workspace.workspace.join("reactive.txt");
    let (mut session, task_id, turn_id) = running_turn(&workspace.workspace, "fixture task");
    let mut actions = 0;
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| {
        actions += 1;
        if actions == 1 {
            WorkspaceAction::CreateFile {
                path: PathBuf::from("reactive.txt"),
                contents: "unchanged default".to_owned(),
            }
        } else {
            complete_action()
        }
    };

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        2,
        &mut decisions,
    )
    .expect("run existing reactive path");

    assert_eq!(result.actions_taken(), 2);
    assert!(artifact.is_file());
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Running
    );
}
