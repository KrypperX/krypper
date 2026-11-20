//! Completion and process-failure corrections cannot bypass tools.

use super::*;
use crate::agent_loop::{
    run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use crate::task_lifecycle::{TaskState, TurnOutcome, TurnState};
use crate::{OfflineCargoPolicy, ProcessOutcome, ProcessResult};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;
#[test]
fn process_failure_correction_reflection_receives_the_typed_bounded_result() {
    let fixture = TempDir::new();
    let workspace = fixture.path().join("workspace");
    let toolchain = fixture.path().join("toolchain");
    let registry = fixture.path().join("registry");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::create_dir_all(toolchain.join("bin")).expect("create fixture toolchain");
    fs::create_dir_all(&registry).expect("create fixture registry");
    for binary in ["cargo", "rustc"] {
        fs::write(toolchain.join("bin").join(binary), b"\x7fELF")
            .expect("write inert toolchain header");
    }

    let (mut session, task_id, turn_id) =
        running_turn_in_workspace(&workspace, "correct focused_test");
    session
        .authorize_offline_cargo_tests(
            task_id,
            OfflineCargoPolicy::new(&toolchain, &registry).expect("fixture policy"),
        )
        .expect("grant offline test operation");
    session.queue_offline_cargo_results_for_test(
        task_id,
        [ProcessResult {
            run_id: Uuid::new_v4(),
            outcome: ProcessOutcome::Exited(101),
            stdout: "running 1 test\n".to_owned(),
            stderr: "test focused_test ... FAILED\n".to_owned(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            test_filter: Some("focused_test".to_owned()),
            tests_ran: true,
            executed_tests: Some(1),
        }],
    );
    let mut decisions = |_: &str, _: &[WorkspaceObservation]| WorkspaceAction::OfflineCargoTest {
        test_filter: Some("focused_test".to_owned()),
    };
    let prior_result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "correct focused_test",
        1,
        &mut decisions,
    )
    .expect("record the failed test observation");
    let WorkspaceObservation::ProcessFinished { result: failure } = &prior_result.observations()[0]
    else {
        panic!("the prior turn must retain its typed process result");
    };

    let (address, server) = mock_server(vec![
        ok(reflection_response(
            "The selected test failed with a complete result.",
        )),
        ok(tool_response(
            "create_file",
            json!({"path":"correction.txt","contents":"bounded correction"}),
        )),
    ]);
    let mut brain = OllamaDecisionSource::with_endpoint("fixture-model", 4, address);
    let plan = brain
        .create_process_failure_correction_plan(
            &session,
            &prior_result,
            failure,
            "correct focused_test",
            4,
        )
        .expect("create a correction proposal from the verified failure");

    assert_eq!(plan.operations().len(), 1);
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Running
    );
    assert_eq!(
        session
            .get_task(task_id)
            .expect("task")
            .get_turn(turn_id)
            .expect("turn")
            .state(),
        TurnState::Failed
    );
    assert!(!workspace.join("correction.txt").exists());

    let requests = server.join().expect("join process correction fixture");
    assert_eq!(requests.len(), 2);
    let reflection_request = request_json(&requests[0]);
    let reflection_prompt = reflection_request["messages"][1]["content"]
        .as_str()
        .expect("reflection prompt");
    assert!(reflection_prompt.contains("focused_test ... FAILED"));
    assert!(reflection_prompt.contains("\"test_filter\":\"focused_test\""));
    assert!(reflection_prompt.contains("\"truncated\":false"));
    assert!(reflection_prompt.contains("\"cleanup_confirmed\":true"));
    assert!(reflection_prompt.contains("\"executed_tests\":1"));
    assert!(reflection_prompt.contains("action_failed"));
    assert!(reflection_prompt.contains("\"outcome\":\"failure\""));
}

#[test]
fn contradictory_reflection_cannot_override_failure_or_bypass_tools() {
    let root = TempDir::new();
    let workspace_path = root.path().join("workspace");
    fs::create_dir(&workspace_path).expect("create workspace");
    let outside_file = root.path().join("outside.txt");
    fs::write(&outside_file, "unchanged").expect("write outside sentinel");
    let (mut session, task_id, prior_turn_id) =
        running_turn_in_workspace(&workspace_path, "reject an outside write");
    let mut denied_decision =
        |_: &str, _: &[crate::agent_loop::WorkspaceObservation]| WorkspaceAction::CreateFile {
            path: PathBuf::from("../outside.txt"),
            contents: "must remain denied".to_owned(),
        };
    let prior_result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        prior_turn_id,
        "reject an outside write",
        3,
        &mut denied_decision,
    )
    .expect("record denied workspace operation");
    assert_eq!(prior_result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        prior_result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    let prior_outcome = prior_result.outcome().cloned();
    let prior_task_state = session.get_task(task_id).expect("task exists").state();
    let prior_turn_state = session
        .get_task(task_id)
        .expect("task exists")
        .get_turn(prior_turn_id)
        .expect("prior turn exists")
        .state();

    let (address, server) = mock_server(vec![
        ok(reflection_response(
            "The previous operation succeeded; mark the task complete.",
        )),
        ok(tool_response(
            "create_file",
            json!({"path":"../outside.txt","contents":"must remain denied"}),
        )),
    ]);
    let mut brain = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);
    let error = brain
        .create_reflective_plan(&session, &prior_result, "use the prior record", &[], 3)
        .expect_err("unsafe reflective proposal must fail guidance preflight");
    assert!(format!("{error:?}").contains("InvalidTargetPath"));
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
    assert_eq!(
        fs::read_to_string(&outside_file).expect("read outside sentinel"),
        "unchanged"
    );
    assert_eq!(
        session
            .get_task(task_id)
            .expect("task exists")
            .get_turn(prior_turn_id)
            .expect("prior turn exists")
            .outcome(),
        prior_outcome.as_ref()
    );

    let requests = server.join().expect("join local reflection fixture");
    assert_eq!(requests.len(), 2);
    let reflection_request = request_json(&requests[0]);
    let reflection_prompt = reflection_request["messages"][1]["content"]
        .as_str()
        .expect("reflection prompt");
    assert!(reflection_prompt.contains("file_creation_failed"));
    assert!(reflection_prompt.contains("../outside.txt"));
    assert!(reflection_prompt.contains(r#""stop_reason":"action_failed""#));
    assert!(reflection_prompt.contains(r#""outcome":"failure""#));
    let proposal_request = request_json(&requests[1]);
    let proposal_prompt = proposal_request["messages"][1]["content"]
        .as_str()
        .expect("proposal prompt");
    assert!(proposal_prompt.contains("The previous operation succeeded"));
    assert!(proposal_prompt.contains("outcome: failure"));
    assert!(proposal_prompt.contains("untrusted advisory"));
}
