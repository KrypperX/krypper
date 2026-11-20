//! Native, plan and Responses payload parsing through the decision path.

use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction};
use crate::task_lifecycle::TurnOutcome;
use serde_json::{json, Value};
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
#[test]
fn malformed_and_unsupported_model_calls_fail_the_active_turn() {
    for response in [
        ok("not json".to_owned()),
        ok(tool_response(
            "run_shell",
            json!({"command":"touch must-not-exist"}),
        )),
    ] {
        let (address, server) = mock_server(vec![response]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject malformed or unsupported calls",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("model call failure is recorded in the turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(fs::read_dir(workspace.path())
            .expect("list workspace")
            .next()
            .is_none());
        assert_eq!(server.join().expect("join mock server").len(), 1);
    }
}

#[test]
fn parser_maps_only_one_supported_tool_call() {
    let action = parse_model_response(
            br#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"create_file","arguments":{"path":"notes.txt","contents":"hello"}}}]}}"#,
        )
        .expect("parse supported action");
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("notes.txt"),
            contents: "hello".to_owned()
        }
    );

    let patch_action = parse_model_response(
        br#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"apply_patch","arguments":{"path":"notes with spaces.txt","patch":"--- a/notes with spaces.txt\n+++ b/notes with spaces.txt\n@@ -1 +1 @@\n-old\n+new\n"}}}]}}"#,
    )
    .expect("parse registered patch action");
    assert_eq!(
        patch_action,
        WorkspaceAction::ApplyPatch {
            path: PathBuf::from("notes with spaces.txt"),
            patch: "--- a/notes with spaces.txt\n+++ b/notes with spaces.txt\n@@ -1 +1 @@\n-old\n+new\n".to_owned()
        }
    );
    assert!(parse_model_response(
        tool_response(
            "apply_patch",
            json!({"path":"notes.txt", "patch":"x".repeat(32 * 1024 + 1)})
        )
        .as_bytes()
    )
    .is_err());

    assert!(parse_model_response(b"not json").is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"run_shell","arguments":{"command":"touch unsafe"}}}]}}"#
        )
        .is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"complete_task","arguments":{"result":"done"}}},{"function":{"name":"complete_task","arguments":{"result":"also done"}}}]}}"#
        )
        .is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"create_file","arguments":{"path":"notes.txt","contents":"hello","command":"unsafe"}}}]}}"#
        )
        .is_err());
}

#[test]
fn parser_rejects_tool_calls_without_an_assistant_role() {
    assert!(parse_model_response(
        br#"{"message":{"tool_calls":[{"function":{"name":"create_file","arguments":{"path":"unsafe.txt","contents":"must not write"}}}]}}"#
    )
    .is_err());
    assert!(parse_model_response(
        br#"{"message":{"role":"user","tool_calls":[{"function":{"name":"create_file","arguments":{"path":"unsafe.txt","contents":"must not write"}}}]}}"#
    )
    .is_err());
}

#[test]
fn http_and_response_decoding_failures_become_failed_actions() {
    for response in [
        MockResponse {
            status: 503,
            body: "service unavailable".to_owned(),
        },
        ok("{invalid json".to_owned()),
    ] {
        let (address, server) = mock_server(vec![response]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Fail clearly",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("model failure is recorded in the turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(matches!(
            result.outcome(),
            Some(TurnOutcome::Failure { .. })
        ));
        assert_eq!(server.join().expect("join mock server").len(), 1);
    }

    let workspace = TempDir::new();
    let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint(
            "mock-model",
            1,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        );
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Fail when the service cannot be reached",
            1,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("connection failure is recorded in the turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert_failed_turn(&result, &session, task_id, turn_id);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}

#[test]
fn responses_streaming_request_preserves_decision_payload() {
    let request = json!({
        "model": "gpt-test",
        "input": "inspect workspace",
        "tools": [{"type": "function", "name": "inspect"}],
        "stream": false
    });
    let bytes = serde_json::to_vec(&request).expect("serialize request");
    let result = enable_responses_streaming(&bytes).expect("enable SSE");
    let converted: Value = serde_json::from_slice(&result).expect("parse converted request");
    assert_eq!(converted["stream"], true);
    assert_eq!(converted["model"], request["model"]);
    assert_eq!(converted["input"], request["input"]);
    assert_eq!(converted["tools"], request["tools"]);
    assert_eq!(request["stream"], false);
}
