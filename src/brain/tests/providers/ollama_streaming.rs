//! Ollama's native NDJSON decision stream.

use super::super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use serde_json::{json, Value};

fn ndjson(records: &[Value]) -> String {
    let mut body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    body
}

fn complete_task_response(result: &str) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "function": {
                    "name": "complete_task",
                    "arguments": {"result": result}
                }
            }]
        }
    })
    .to_string()
}

fn assert_stream_fails_closed(body: String) {
    let (address, server) = mock_server(vec![ok(body)]);
    let mut source = OllamaDecisionSource::with_streaming_endpoint("test-model", 1, address);

    let action = source.choose_action("Finish the requested task", &[]);
    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));

    let requests = server.join().expect("join Ollama mock server");
    assert_eq!(requests.len(), 1, "a failed stream is never replayed");
    assert_eq!(request_json(&requests[0])["stream"], true);
}

#[test]
fn ollama_streaming_is_opt_in_and_validates_action_after_terminal_record() {
    let body = ndjson(&[
        json!({
            "message": {
                "role": "assistant",
                "thinking": "private model reasoning",
                "tool_calls": [{
                    "function": {
                        "name": "complete_task",
                        "arguments": {"result": "done"}
                    }
                }]
            },
            "done": false
        }),
        json!({
            "message": {"role": "assistant", "content": ""},
            "done_reason": "stop",
            "done": true
        }),
    ]);
    let (address, server) = mock_server(vec![ok(body)]);
    let mut source = OllamaDecisionSource::with_streaming_endpoint("test-model", 1, address);

    let action = source.choose_action("Finish the requested task", &[]);
    assert_eq!(
        action,
        WorkspaceAction::Complete {
            result: "done".to_owned()
        }
    );

    let requests = server.join().expect("join Ollama mock server");
    assert_eq!(requests.len(), 1);
    assert_eq!(request_json(&requests[0])["stream"], true);
    assert_eq!(
        request_header(&requests[0], "accept").as_deref(),
        Some("application/x-ndjson")
    );

    let (address, server) = mock_server(vec![ok(complete_task_response("done"))]);
    let mut source = OllamaDecisionSource::with_endpoint("test-model", 1, address);
    let non_stream_action = source.choose_action("Finish the requested task", &[]);
    let requests = server.join().expect("join Ollama mock server");

    assert_eq!(non_stream_action, action);
    assert_eq!(request_json(&requests[0])["stream"], false);
}

#[test]
fn ollama_streaming_rejects_a_tool_call_without_a_terminal_record() {
    let body = ndjson(&[json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "function": {
                    "name": "complete_task",
                    "arguments": {"result": "must not dispatch"}
                }
            }]
        },
        "done": false
    })]);
    let (address, server) = mock_server(vec![ok(body)]);
    let mut source = OllamaDecisionSource::with_streaming_endpoint("test-model", 1, address);

    let action = source.choose_action("Finish the requested task", &[]);
    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));

    let requests = server.join().expect("join Ollama mock server");
    assert_eq!(requests.len(), 1, "a failed stream is never replayed");
    assert_eq!(request_json(&requests[0])["stream"], true);
}

#[test]
fn ollama_streaming_opt_in_keeps_configured_profile_endpoints_non_streaming() {
    let (address, server) = mock_server(vec![ok(complete_task_response("done"))]);
    let endpoint = format!("http://{address}/api/chat");
    let mut source = OllamaDecisionSource::with_profile_endpoint("test-model", 1, endpoint);

    let action = source.choose_action("Finish the requested task", &[]);
    let requests = server.join().expect("join Ollama mock server");

    assert_eq!(
        action,
        WorkspaceAction::Complete {
            result: "done".to_owned()
        }
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(request_json(&requests[0])["stream"], false);
}

#[test]
fn ollama_streaming_rejects_malformed_records_length_and_events_after_completion() {
    assert_stream_fails_closed("not json\n".to_owned());

    let truncated_by_model = ndjson(&[
        json!({
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "function": {
                        "name": "complete_task",
                        "arguments": {"result": "incomplete"}
                    }
                }]
            },
            "done": false
        }),
        json!({
            "message": {"role": "assistant", "content": ""},
            "done_reason": "length",
            "done": true
        }),
    ]);
    assert_stream_fails_closed(truncated_by_model);

    let events_after_completion = ndjson(&[
        json!({
            "message": {"role": "assistant", "content": ""},
            "done_reason": "stop",
            "done": true
        }),
        json!({
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "function": {
                        "name": "complete_task",
                        "arguments": {"result": "late action"}
                    }
                }]
            },
            "done": false
        }),
    ]);
    assert_stream_fails_closed(events_after_completion);
}

#[test]
fn ollama_streaming_enforces_response_and_tool_argument_limits() {
    let oversized_content = "x".repeat(crate::brain::limits::MAX_MODEL_RESPONSE_BYTES);
    let oversized_response = ndjson(&[json!({
        "message": {
            "role": "assistant",
            "content": oversized_content,
            "tool_calls": [{
                "function": {
                    "name": "complete_task",
                    "arguments": {"result": "would complete without the response cap"}
                }
            }]
        },
        "done_reason": "stop",
        "done": true
    })]);
    assert!(oversized_response.len() > crate::brain::limits::MAX_MODEL_RESPONSE_BYTES);
    assert_stream_fails_closed(oversized_response);

    let oversized_result = "x".repeat(crate::brain::limits::MAX_STREAMED_TOOL_ARGUMENT_BYTES);
    let oversized_tool_call = ndjson(&[json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "function": {
                    "name": "complete_task",
                    "arguments": {"result": oversized_result}
                }
            }]
        },
        "done_reason": "stop",
        "done": true
    })]);
    assert_stream_fails_closed(oversized_tool_call);
}

#[test]
fn ollama_streaming_preserves_advertised_extension_actions() {
    let declarations = [json!({
        "type": "function",
        "function": {
            "name": "external_read",
            "description": "Read an external path",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })];
    let body = ndjson(&[
        json!({
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "function": {
                        "name": "external_read",
                        "arguments": {"path": "ollama.txt"}
                    }
                }]
            },
            "done": false
        }),
        json!({"message": {"role": "assistant", "content": ""}, "done": true}),
    ]);
    let (address, server) = mock_server(vec![ok(body)]);
    let mut source = OllamaDecisionSource::with_streaming_endpoint_and_extensions(
        "test-model",
        1,
        address,
        &declarations,
    );

    assert_eq!(
        source.choose_action("Use the advertised extension", &[]),
        WorkspaceAction::Extension {
            name: "external_read".to_owned(),
            arguments: json!({"path": "ollama.txt"})
        }
    );

    let requests = server.join().expect("join Ollama mock server");
    assert!(request_json(&requests[0])["tools"]
        .as_array()
        .expect("tool definitions")
        .iter()
        .any(|tool| tool["function"]["name"] == "external_read"));
}
