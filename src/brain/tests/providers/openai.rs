//! OpenAI Responses provider contract.

use super::super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use serde_json::{json, Value};
use std::path::PathBuf;
pub(super) fn openai_function_call(name: &str, arguments: Value) -> Value {
    json!({
        "type": "function_call",
        "name": name,
        "arguments": arguments.to_string()
    })
}

#[test]
fn openai_responses_request_uses_fixed_contract_and_normalizes_one_action() {
    let (address, server) = mock_server(vec![ok(json!({
        "status":"completed",
        "output":[openai_function_call(
            "create_file",
            json!({"path":"summary.txt", "contents":"from OpenAI"})
        )]
    })
    .to_string())]);
    let api_key = "local-openai-test-key";
    let endpoint = format!("http://{address}/v1/responses");
    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model:cloud",
        ResponsesProvider::OpenAi,
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
    let request = server.join().expect("join OpenAI mock server");
    let raw_request = &request[0];
    let first_line = String::from_utf8_lossy(raw_request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    let body = request_body(raw_request);

    assert!(first_line.starts_with("POST /v1/responses "));
    assert_eq!(
        request_header(raw_request, "authorization").as_deref(),
        Some("Bearer local-openai-test-key")
    );
    assert_eq!(body["model"], "test-model:cloud");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], false);
    assert!(body["instructions"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["input"].as_str().is_some_and(|text| {
        text.contains("Summarize the note") && text.contains("local-only fixture text")
    }));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["path"]["type"],
        "string"
    );
    assert!(!body.to_string().contains(api_key));
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: "from OpenAI".to_owned(),
        }
    );
}

#[test]
fn openai_responses_parser_rejects_malformed_unsupported_and_multiple_calls() {
    let one_call = openai_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"status":"completed", "output":[]})
            .to_string()
            .into_bytes(),
        json!({"status":"incomplete", "output":[one_call.clone()]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), one_call]})
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
    ];

    for response in invalid_responses {
        assert!(parse_responses_response(&response, ModelProvider::OpenAi).is_err());
    }
}
