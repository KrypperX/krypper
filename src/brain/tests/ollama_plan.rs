//! Planning through the local Ollama fixture.

use super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use serde_json::{json, Value};
use std::path::PathBuf;
#[test]
fn ollama_plan_fixture_preserves_workspace_call_order() {
    let response = json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"read_file","arguments":{"path":"input.txt"}}},
                {"function":{"name":"create_file","arguments":{
                    "path":"result.txt",
                    "contents":"fixture output"
                }}}
            ]
        }
    });
    let response = serde_json::to_vec(&response).expect("encode response fixture");

    let plan = parse_model_plan_response(&response, 2, &all_advertised_tools())
        .expect("parse bounded plan");

    assert_eq!(
        plan.operations(),
        [
            WorkspaceAction::ReadFile {
                path: PathBuf::from("input.txt"),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("result.txt"),
                contents: "fixture output".to_owned(),
            },
        ]
    );
}

#[test]
fn ollama_plan_fixtures_reject_malformed_unsupported_and_over_limit_calls() {
    let malformed = serde_json::to_vec(&json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"read_file","arguments":{"path":7}}}
            ]
        }
    }))
    .expect("encode malformed fixture");
    assert_eq!(
        parse_model_plan_response(&malformed, 2, &all_advertised_tools()),
        Err(WorkspacePlanError::InvalidOperation)
    );

    let unsupported = serde_json::to_vec(&json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"complete_task","arguments":{"result":"done"}}}
            ]
        }
    }))
    .expect("encode unsupported fixture");
    assert_eq!(
        parse_model_plan_response(&unsupported, 2, &all_advertised_tools()),
        Err(WorkspacePlanError::UnsupportedOperation)
    );

    let over_limit = serde_json::to_vec(&json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"read_file","arguments":{"path":"one.txt"}}},
                {"function":{"name":"read_file","arguments":{"path":"two.txt"}}}
            ]
        }
    }))
    .expect("encode over-limit fixture");
    assert_eq!(
        parse_model_plan_response(&over_limit, 1, &all_advertised_tools()),
        Err(WorkspacePlanError::TooManyOperations { maximum: 1 })
    );
}

#[test]
fn ollama_plan_request_exposes_only_registered_workspace_tools() {
    let request = build_plan_request_with_project_guidance(
        "fixture-model",
        "read then create",
        &[],
        2,
        None,
        None,
    )
    .expect("build plan request");
    let request: Value = serde_json::from_slice(&request).expect("decode plan request");
    let tools = request["tools"].as_array().expect("workspace tool list");

    assert_eq!(tools.len(), 10);
    assert!(tools.iter().all(|tool| {
        tool["function"]["name"]
            .as_str()
            .is_some_and(|name| name != "complete_task")
    }));
    assert!(request["messages"][1]["content"]
        .as_str()
        .expect("plan prompt")
        .contains("at most 2 ordered workspace operations"));
    assert_eq!(request["stream"], false);
}

#[test]
fn ollama_plan_source_uses_a_local_fixture_and_keeps_control_on_the_existing_path() {
    let plan_response = json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"read_file","arguments":{"path":"input.txt"}}},
                {"function":{"name":"create_file","arguments":{
                    "path":"result.txt",
                    "contents":"fixture output"
                }}}
            ]
        }
    })
    .to_string();
    let (address, handle) = mock_server(vec![
        ok(plan_response),
        ok(tool_response(
            "complete_task",
            json!({"result":"observed result"}),
        )),
    ]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 4, address);

    let plan = source
        .create_plan("read the fixture and create a result", &[], 4)
        .expect("create fixture plan");
    let control = BoundedDecisionSource::choose_action(
        &mut source,
        "read the fixture and create a result",
        &[],
    );
    let requests = handle.join().expect("join local fixture server");

    assert_eq!(plan.operations().len(), 2);
    assert!(matches!(
        control,
        WorkspaceAction::Complete { ref result } if result == "observed result"
    ));
    assert_eq!(requests.len(), 2);
    let plan_request = request_json(&requests[0]);
    assert_eq!(
        plan_request["tools"].as_array().expect("plan tools").len(),
        10
    );
    assert!(plan_request["messages"][0]["content"]
        .as_str()
        .expect("plan instructions")
        .contains("ordered series of tool calls"));
    assert!(plan_request["messages"][1]["content"]
        .as_str()
        .expect("plan prompt")
        .contains("at most 3 ordered workspace operations"));
    assert_eq!(
        request_json(&requests[1])["tools"]
            .as_array()
            .unwrap()
            .len(),
        13
    );
}
