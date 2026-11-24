//! xAI Responses provider contract.

use super::super::*;
use super::openai::*;
use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
};
use crate::task_lifecycle::TurnState;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
#[test]
fn xai_responses_request_uses_fixed_endpoint_auth_and_shared_contract() {
    let (address, server) = mock_server(vec![ok(json!({
        "status":"completed",
        "output":[openai_function_call(
            "create_file",
            json!({"path":"summary.txt", "contents":"from xAI"})
        )]
    })
    .to_string())]);
    let api_key = "local-xai-test-key";
    let endpoint = format!("http://{address}/v1/responses");
    let mut source = ResponsesDecisionSource::with_endpoint(
        "grok-local-test",
        ResponsesProvider::Xai,
        api_key,
        3,
        endpoint,
    );
    let action = source.choose_action(
        "Summarize the note",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("note.txt"),
            contents: "local-only fixture text".to_owned(),
        }],
    );
    let requests = server.join().expect("join xAI mock server");
    let raw_request = &requests[0];
    let first_line = String::from_utf8_lossy(raw_request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    let body = request_body(raw_request);

    assert_eq!(
        ResponsesProvider::Xai.endpoint(),
        "https://api.x.ai/v1/responses"
    );
    assert!(first_line.starts_with("POST /v1/responses "));
    assert_eq!(
        request_header(raw_request, "authorization").as_deref(),
        Some("Bearer local-xai-test-key")
    );
    assert_eq!(body["model"], "grok-local-test");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], false);
    assert!(body["input"].as_str().is_some_and(|text| {
        text.contains("Summarize the note")
            && text.contains("local-only fixture text")
            && text.contains("Action slots remaining, including this decision: 3")
    }));
    assert!(body["tools"].as_array().is_some_and(|tools| {
        !tools.is_empty() && tools.iter().all(|tool| tool["type"] == "function")
    }));
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert!(!body.to_string().contains(api_key));
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: "from xAI".to_owned(),
        }
    );
}

#[test]
fn xai_responses_accepts_one_bounded_action_through_workspace_safety() {
    let (address, server) = mock_server(vec![
        ok(json!({
            "status":"completed",
            "output":[openai_function_call(
                "create_file",
                json!({"path":"summary.txt", "contents":"created safely"})
            )]
        })
        .to_string()),
        ok(json!({
            "status":"completed",
            "output":[openai_function_call("complete_task", json!({"result":"done"}))]
        })
        .to_string()),
    ]);
    let workspace = TempDir::new();
    let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = ResponsesDecisionSource::with_endpoint(
            "grok-local-test",
            ResponsesProvider::Xai,
            "local-xai-test-key",
            2,
            format!("http://{address}/v1/responses"),
        );
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a summary file",
            2,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("run xAI actions through the bounded task loop");
    let requests = server.join().expect("join xAI workspace mock");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(result.actions_taken(), 2);
    assert_eq!(
        fs::read_to_string(workspace.path().join("summary.txt"))
            .expect("read file created under the workspace"),
        "created safely"
    );
    assert_eq!(requests.len(), 2);
    assert!(request_body(&requests[0])["input"]
        .as_str()
        .is_some_and(|text| text.contains("Action slots remaining, including this decision: 2")));
    assert!(request_body(&requests[1])["input"]
        .as_str()
        .is_some_and(|text| text.contains("Action slots remaining, including this decision: 1")));
    let task = session.get_task(task_id).expect("task exists");
    assert_eq!(
        task.get_turn(turn_id).expect("turn exists").state(),
        TurnState::Succeeded
    );
}

#[test]
fn xai_multiple_calls_never_execute_a_workspace_action() {
    let (address, server) = mock_server(vec![ok(json!({
        "status":"completed",
        "output":[
            openai_function_call("create_file", json!({"path":"must-not-exist.txt", "contents":"no"})),
            openai_function_call("complete_task", json!({"result":"done"}))
        ]
    })
    .to_string())]);
    let workspace = TempDir::new();
    let result = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = ResponsesDecisionSource::with_endpoint(
            "grok-local-test",
            ResponsesProvider::Xai,
            "local-xai-test-key",
            1,
            format!("http://{address}/v1/responses"),
        );
        run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a file only from one supported call",
            1,
            &mut source,
        )
    })
    .expect("record a rejected multiple-call response as a failed turn");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(!workspace.path().join("must-not-exist.txt").exists());
    assert_eq!(
        server.join().expect("join xAI invalid-response mock").len(),
        1
    );
}

#[test]
fn xai_responses_parser_rejects_every_ambiguous_or_unsupported_response_shape() {
    let one_call = openai_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"status":"completed", "output":[]})
            .to_string()
            .into_bytes(),
        json!({"status":"incomplete", "output":[one_call.clone()]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), one_call.clone()]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), {"type":"function"}]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), {"name":"create_file", "arguments":"{}"}]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), {"type":"unrecognized_item"}]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[openai_function_call("run_shell", json!({}))]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[{
            "type":"function_call", "name":"complete_task", "arguments":"{invalid"
        }]})
        .to_string()
        .into_bytes(),
        json!({"status":"completed", "choices":[{"message":{}}, {"message":{}}], "output":[one_call]})
            .to_string()
            .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(
            parse_responses_response(&response, ModelProvider::Xai, &all_advertised_tools())
                .is_err()
        );
    }
}

#[test]
fn xai_errors_never_echo_api_keys_or_response_bodies() {
    let api_key = "local-xai-error-key";
    let (address, server) = mock_server(vec![MockResponse {
        status: 401,
        body: json!({"error":{"message":api_key}}).to_string(),
    }]);
    let mut source = ResponsesDecisionSource::with_endpoint(
        "grok-local-test",
        ResponsesProvider::Xai,
        api_key,
        1,
        format!("http://{address}/v1/responses"),
    );
    let action = source.choose_action("Test a failed xAI request", &[]);
    let requests = server.join().expect("join xAI error mock server");
    let recorded = String::from_utf8_lossy(&requests[0]);

    assert!(recorded.contains(api_key));
    assert!(!request_body(&requests[0]).to_string().contains(api_key));
    let WorkspaceAction::Unsupported { action } = action else {
        panic!("xAI HTTP error should fail as an unsupported bounded action");
    };
    assert!(action.contains("401"));
    assert!(!action.contains(api_key));
    assert!(!action.contains("error\":{\"message"));
}
