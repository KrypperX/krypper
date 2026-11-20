//! Reflection after a verified turn and the correction budget.

use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction};
use crate::harness::execution::run_goal_turn_with_plan;
use crate::task_lifecycle::{TurnOutcome, TurnState};
use serde_json::json;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
#[test]
fn opt_in_reflection_uses_a_verified_success_and_only_proposes_a_bounded_plan() {
    let workspace = TempDir::new();
    let (mut session, task_id, prior_turn_id) =
        running_turn_in_workspace(workspace.path(), "record an observed file creation");
    let mut prior_decisions =
        |_: &str, observations: &[crate::agent_loop::WorkspaceObservation]| {
            if observations.is_empty() {
                WorkspaceAction::CreateFile {
                    path: PathBuf::from("observed.txt"),
                    contents: "created by Harness".to_owned(),
                }
            } else {
                WorkspaceAction::Complete {
                    result: "prior turn completed".to_owned(),
                }
            }
        };
    let prior_result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        prior_turn_id,
        "record an observed file creation",
        2,
        &mut prior_decisions,
    )
    .expect("record successful prior turn");
    assert_eq!(prior_result.stop_reason(), GoalTurnStopReason::Completed);
    let prior_task_state = session.get_task(task_id).expect("task exists").state();
    let prior_turn_state = session
        .get_task(task_id)
        .expect("task exists")
        .get_turn(prior_turn_id)
        .expect("prior turn exists")
        .state();

    let (address, server) = mock_server(vec![
        ok(reflection_response(
            "The recorded file creation can guide a follow-up.",
        )),
        ok(tool_response(
            "create_file",
            json!({"path":"follow-up.txt","contents":"proposed by Brain"}),
        )),
    ]);
    let mut brain = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);
    let plan = brain
        .create_reflective_plan(&session, &prior_result, "use the prior record", &[], 3)
        .expect("create a reflective plan from the recorded success");
    assert_eq!(plan.operations().len(), 1);
    assert_eq!(
        session.get_task(task_id).expect("task exists").state(),
        prior_task_state
    );
    assert_eq!(
        session
            .get_task(task_id)
            .expect("task exists")
            .get_turn(prior_turn_id)
            .expect("prior turn exists")
            .state(),
        prior_turn_state
    );
    assert!(!workspace.path().join("follow-up.txt").exists());

    let requests = server.join().expect("join local reflection fixture");
    assert_eq!(requests.len(), 2);
    let reflection_request = request_json(&requests[0]);
    let reflection_prompt = reflection_request["messages"][1]["content"]
        .as_str()
        .expect("reflection prompt");
    assert!(reflection_prompt.contains("file_created"));
    assert!(reflection_prompt.contains("observed.txt"));
    assert!(reflection_prompt.contains(r#""stop_reason":"completed""#));
    assert!(reflection_prompt.contains(r#""outcome":"success""#));
    assert!(!reflection_prompt.contains("prior turn completed"));

    let proposal_request = request_json(&requests[1]);
    let proposal_prompt = proposal_request["messages"][1]["content"]
        .as_str()
        .expect("proposal prompt");
    assert!(proposal_prompt.contains("The recorded file creation can guide a follow-up."));
    assert!(proposal_prompt.contains("outcome: success"));
    assert!(proposal_prompt.contains("untrusted advisory"));

    let next_turn_id = {
        let task = session.get_task_mut(task_id).expect("task exists");
        let turn_id = task.create_turn().expect("create follow-up turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("start follow-up turn");
        turn_id
    };
    let mut control =
        |_: &str, _: &[crate::agent_loop::WorkspaceObservation]| WorkspaceAction::Complete {
            result: "follow-up complete".to_owned(),
        };
    let next_result = run_goal_turn_with_plan(
        &mut session,
        task_id,
        next_turn_id,
        "use the prior record",
        3,
        Ok(plan),
        &mut control,
    )
    .expect("Harness executes the proposed follow-up");
    assert_eq!(next_result.actions_taken(), 2);
    assert!(next_result.actions_taken() <= 3);
    assert_eq!(
        fs::read_to_string(workspace.path().join("follow-up.txt"))
            .expect("read Harness-created follow-up"),
        "proposed by Brain"
    );
    assert_eq!(
        session
            .get_task(task_id)
            .expect("task exists")
            .get_turn(prior_turn_id)
            .expect("prior turn exists")
            .outcome(),
        Some(&TurnOutcome::Success {
            result: "prior turn completed".to_owned()
        })
    );
}

#[test]
fn completion_correction_is_not_requested_after_checks_pass() {
    let (mut session, task_id, turn_id) = running_turn();
    let mut decisions =
        |_: &str, _: &[crate::agent_loop::WorkspaceObservation]| WorkspaceAction::Complete {
            result: "initial work complete".to_owned(),
        };
    let initial_result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "fixture task",
        2,
        &mut decisions,
    )
    .expect("complete initial turn");
    let checks = [crate::harness::verification::RequiredCompletionCheck {
        id: "fixture_check".to_owned(),
        description: "Fixture check".to_owned(),
    }];
    let evidence = [crate::harness::verification::CompletionCheckEvidence {
        check_id: "fixture_check".to_owned(),
        status: crate::harness::verification::CompletionCheckEvidenceStatus::Passed,
        source: crate::harness::verification::CompletionEvidenceSource::DeterministicLocalCheck,
        summary: "fixture passed".to_owned(),
    }];
    let verification = crate::harness::verification::verify_completion_checks(&checks, &evidence);
    let mut brain = OllamaDecisionSource::with_endpoint(
        "fixture-model",
        3,
        "127.0.0.1:1".parse().expect("loopback address"),
    );

    let plan = brain
        .create_completion_correction_plan(
            &session,
            &initial_result,
            "fixture task",
            initial_result.observations(),
            &verification,
            3,
        )
        .expect("passing checks do not request a correction");

    assert!(plan.is_none());
}

#[test]
fn reflective_plan_verifies_continue_then_enforces_the_future_budget() {
    let workspace = TempDir::new();
    let (mut session, task_id, turn_id) =
        running_turn_in_workspace(workspace.path(), "record an exhausted prior turn");
    let mut continue_turn =
        |_: &str, _: &[crate::agent_loop::WorkspaceObservation]| WorkspaceAction::Continue;
    let prior_result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "record an exhausted prior turn",
        1,
        &mut continue_turn,
    )
    .expect("record exhausted prior turn");
    assert_eq!(
        prior_result.stop_reason(),
        GoalTurnStopReason::ActionBudgetExhausted
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind unused local endpoint");
    let address = listener.local_addr().expect("read unused local endpoint");
    drop(listener);
    let mut brain = OllamaDecisionSource::with_endpoint("fixture-model", 2, address);

    let error = brain
        .create_reflective_plan(&session, &prior_result, "follow up", &[], 2)
        .expect_err("reflection must reserve budget for a later operation and control");

    assert!(matches!(error, BrainReflectionError::ActionBudgetExhausted));
}
