//! Anthropic Messages provider contract.

use super::super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use serde_json::{json, Value};
use std::path::PathBuf;
pub(super) fn assert_anthropic_schema_supported(schema: &Value) {
    match schema {
        Value::Object(fields) => {
            for unsupported in ["minimum", "maximum", "minLength", "maxLength"] {
                assert!(
                    !fields.contains_key(unsupported),
                    "Anthropic schema contains unsupported `{unsupported}`"
                );
            }
            if fields.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    fields.get("additionalProperties"),
                    Some(&Value::Bool(false))
                );
            }
            fields.values().for_each(assert_anthropic_schema_supported);
        }
        Value::Array(values) => values.iter().for_each(assert_anthropic_schema_supported),
        _ => {}
    }
}

pub(super) fn anthropic_tool_use(name: &str, input: Value) -> Value {
    json!({"type":"tool_use", "id":"toolu_test", "name":name, "input":input})
}

#[test]
fn anthropic_messages_request_uses_fixed_contract_and_normalizes_one_action() {
    let (address, server) = mock_server(vec![ok(json!({
        "stop_reason":"tool_use",
        "content":[anthropic_tool_use("read_file", json!({"path":"plan.md"}))]
    })
    .to_string())]);
    let api_key = "local-anthropic-test-key";
    let endpoint = format!("http://{address}/v1/messages");
    let mut source =
        AnthropicDecisionSource::with_endpoint("test-model:cloud", api_key, 2, endpoint);
    let action = source.choose_action("Inspect the plan", &[]);
    let request = server.join().expect("join Anthropic mock server");
    let raw_request = &request[0];
    let first_line = String::from_utf8_lossy(raw_request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    let body = request_body(raw_request);

    assert!(first_line.starts_with("POST /v1/messages "));
    assert_eq!(
        request_header(raw_request, "authorization").as_deref(),
        Some("Bearer local-anthropic-test-key")
    );
    assert_eq!(
        request_header(raw_request, "anthropic-version").as_deref(),
        Some("2026-10-21")
    );
    assert_eq!(body["model"], "test-model:cloud");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], false);
    assert!(body["system"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("Inspect the plan")));
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
    for tool in body["tools"].as_array().expect("Anthropic tools array") {
        assert_eq!(tool["strict"], true);
        assert_anthropic_schema_supported(&tool["input_schema"]);
    }
    assert!(!body.to_string().contains(api_key));
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn anthropic_sse_request_preserves_auth_version_and_workspace_headers() {
    let (address, server) = mock_server(vec![ok("event: ping\ndata: {}\n\n".to_owned())]);
    let endpoint = format!("http://{address}/v1/messages");
    let result = crate::brain::http::post_hosted_sse_with_headers(
        &model_agent(),
        ModelProvider::Anthropic,
        &endpoint,
        "local-anthropic-stream-key",
        &[("anthropic-workspace-id", "wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ")],
        br#"{}"#,
        |_| Ok(()),
    );
    let requests = server.join().expect("join Anthropic SSE mock server");
    assert!(result.is_ok(), "SSE transport failed: {result:?}");
    let request = &requests[0];
    assert_eq!(
        request_header(request, "x-api-key").as_deref(),
        Some("local-anthropic-stream-key")
    );
    assert_eq!(request_header(request, "authorization"), None);
    assert_eq!(
        request_header(request, "anthropic-version").as_deref(),
        Some("2026-10-21")
    );
    assert_eq!(
        request_header(request, "anthropic-workspace-id").as_deref(),
        Some("wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ")
    );
    assert_eq!(
        request_header(request, "accept").as_deref(),
        Some("text/event-stream")
    );
    assert!(!request_body(request)
        .to_string()
        .contains("local-anthropic-stream-key"));
}

#[test]
fn anthropic_workspace_header_is_sent_only_when_configured() {
    for workspace_id in [None, Some("wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ")] {
        let (address, server) = mock_server(vec![ok(json!({
            "stop_reason":"tool_use",
            "content":[anthropic_tool_use("read_file", json!({"path":"plan.md"}))]
        })
        .to_string())]);
        let endpoint = format!("http://{address}/v1/messages");
        let mut source = AnthropicDecisionSource::with_endpoint(
            "test-model:cloud",
            "local-anthropic-test-key",
            1,
            endpoint,
        );
        if let Some(workspace_id) = workspace_id {
            source = source
                .with_workspace_id(workspace_id)
                .expect("valid workspace id");
        }
        let action = source.choose_action("Inspect the plan", &[]);
        let request = server.join().expect("join Anthropic mock server");

        assert_eq!(
            request_header(&request[0], "anthropic-workspace-id").as_deref(),
            workspace_id
        );
        // Authentication and version headers are unchanged by the option.
        assert_eq!(
            request_header(&request[0], "authorization").as_deref(),
            Some("Bearer local-anthropic-test-key")
        );
        assert_eq!(
            request_header(&request[0], "anthropic-version").as_deref(),
            Some("2026-10-21")
        );
        assert!(matches!(action, WorkspaceAction::ReadFile { .. }));
    }
}

#[test]
fn anthropic_workspace_id_rejects_empty_oversized_and_unsafe_values() {
    let source = || {
        AnthropicDecisionSource::with_endpoint(
            "test-model:cloud",
            "local-anthropic-test-key",
            1,
            "http://127.0.0.1:9/v1/messages".to_owned(),
        )
    };
    let longest_valid = "x".repeat(128);
    for valid in [
        "wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ",
        "a",
        "A-b_c.d",
        longest_valid.as_str(),
    ] {
        assert!(source().with_workspace_id(valid).is_ok(), "{valid:?}");
    }
    let too_long = "x".repeat(129);
    for invalid in [
        "",
        " ",
        "a b",
        " abc",
        "abc ",
        "a\r\nX-Injected: 1",
        "a\0b",
        "wrk/space",
        "ไทย",
        too_long.as_str(),
    ] {
        assert_eq!(
            source().with_workspace_id(invalid).err(),
            Some("ANTHROPIC_WORKSPACE_ID must be 1-128 ASCII letters, digits, '-', '_' or '.'"),
            "{invalid:?}"
        );
    }
}

#[test]
fn anthropic_messages_parser_rejects_malformed_unsupported_and_multiple_calls() {
    let one_call = anthropic_tool_use("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"stop_reason":"end_turn", "content":[]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[one_call.clone(), one_call]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[anthropic_tool_use("run_shell", json!({}))]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[{
            "type":"tool_use", "name":"complete_task", "input":"not an object"
        }]})
        .to_string()
        .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(parse_anthropic_response(&response).is_err());
    }
}

#[test]
fn anthropic_schema_drops_provider_unsupported_bounds_but_runtime_validation_remains() {
    let tools = anthropic_tool_definitions();
    let tools = tools.as_array().expect("Anthropic tools array");
    let read_range = tools
        .iter()
        .find(|tool| tool["name"] == "read_range")
        .expect("read_range tool");
    assert_eq!(
        read_range["input_schema"]["properties"]["start_line"],
        json!({"type":"integer"})
    );
    assert_eq!(
        read_range["input_schema"]["properties"]["max_lines"],
        json!({"type":"integer"})
    );

    let invalid_calls = [
        (
            "read_range",
            json!({"path":"notes.txt", "start_line":0, "max_lines":1}),
        ),
        (
            "read_range",
            json!({"path":"notes.txt", "start_line":1, "max_lines":201}),
        ),
        ("search_text", json!({"directory":".", "query":""})),
        (
            "search_text",
            json!({"directory":".", "query":"x".repeat(4097)}),
        ),
        (
            "replace_text",
            json!({"path":"notes.txt", "match_text":"", "replacement_text":"next"}),
        ),
        ("apply_patch", json!({"path":"notes.txt", "patch":""})),
        (
            "apply_patch",
            json!({"path":"notes.txt", "patch":"--- a/notes.txt\n+++ b/notes.txt\n", "shell":"unsafe"}),
        ),
    ];

    for (name, input) in invalid_calls {
        let response = json!({
            "stop_reason":"tool_use",
            "content":[anthropic_tool_use(name, input)]
        });
        assert!(parse_anthropic_response(response.to_string().as_bytes()).is_err());
    }
}

#[test]
fn hosted_provider_errors_never_echo_keys_or_response_bodies() {
    let openai_key = "local-openai-error-key";
    let anthropic_key = "local-anthropic-error-key";
    for (key, provider, path) in [
        (openai_key, "OpenAI", "/v1/responses"),
        (anthropic_key, "Anthropic", "/v1/messages"),
    ] {
        let (address, server) = mock_server(vec![MockResponse {
            status: 401,
            body: json!({"error":{"message":key}}).to_string(),
        }]);
        let endpoint = format!("http://{address}{path}");
        let mut source = if provider == "OpenAI" {
            ModelDecisionSource::Responses(ResponsesDecisionSource::with_endpoint(
                "test-model",
                ResponsesProvider::OpenAi,
                key,
                1,
                endpoint,
            ))
        } else {
            ModelDecisionSource::Anthropic(AnthropicDecisionSource::with_endpoint(
                "test-model",
                key,
                1,
                endpoint,
            ))
        };
        let action = source.choose_action("Test a failed request", &[]);
        let requests = server.join().expect("join provider error mock server");
        let recorded = String::from_utf8_lossy(&requests[0]);
        assert!(recorded.contains(key));
        assert!(!request_body(&requests[0]).to_string().contains(key));
        let WorkspaceAction::Unsupported { action } = action else {
            panic!("provider error should fail as an unsupported bounded action");
        };
        assert!(action.contains("401"));
        assert!(!action.contains(key));
        assert!(!action.contains("error\":{\"message"));
    }
}
