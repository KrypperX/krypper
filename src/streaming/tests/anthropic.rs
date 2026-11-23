//! Anthropic stream assembler and provider integration regressions.

fn assert_anthropic_stream_decision(truncated: bool, duplicate_stop: bool, mismatched_type: bool) {
    use crate::agent_loop::WorkspaceAction;
    use crate::brain::AnthropicDecisionSource;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut request = [0_u8; 16384];
        let count = socket.read(&mut request).expect("request");
        let received = String::from_utf8_lossy(&request[..count]);
        assert!(received
            .to_ascii_lowercase()
            .contains("authorization: bearer local-test-key"));
        let frames = [
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"read_file","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":r#"{"path":"#}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":r#""README.md"}"#}}),
            ),
            ("content_block_stop", json!({"index":0})),
            ("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
            ("message_stop", json!({})),
        ];
        let mut body = String::new();
        for (kind, data) in frames {
            if truncated && kind == "message_stop" {
                break;
            }
            let mut data = data;
            data["type"] = serde_json::Value::String(
                if mismatched_type && kind == "content_block_delta" {
                    "message_delta"
                } else {
                    kind
                }
                .to_owned(),
            );
            body.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
            if duplicate_stop && kind == "message_delta" {
                body.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
            }
        }
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
        socket.write_all(response.as_bytes()).expect("response");
    });
    let mut source = AnthropicDecisionSource::with_endpoint_and_extensions(
        "test-model",
        "local-test-key",
        2,
        endpoint,
        &[],
    );
    let result = source.choose_with_streaming_for_test("read README", &[], None, true);
    if truncated || duplicate_stop || mismatched_type {
        assert!(result.is_err(), "invalid stream must not yield an action");
    } else {
        let streamed = result.expect("valid Anthropic SSE decision");
        assert!(matches!(streamed, WorkspaceAction::ReadFile { .. }));
        let response = json!({
            "stop_reason": "tool_use",
            "content": [{
                "type": "tool_use",
                "id": "tool_1",
                "name": "read_file",
                "input": {"path": "README.md"}
            }]
        });
        let nonstreamed = crate::brain::parse_anthropic_response(response.to_string().as_bytes())
            .expect("equivalent non-streaming decision");
        assert_eq!(format!("{streamed:?}"), format!("{nonstreamed:?}"));
    }
    server.join().expect("server");
}

#[test]
fn anthropic_stream_assembles_and_validates_completed_tool_use() {
    assert_anthropic_stream_decision(false, false, false);
}

#[test]
fn anthropic_truncated_stream_never_yields_an_action() {
    assert_anthropic_stream_decision(true, false, false);
}

#[test]
fn anthropic_duplicate_stop_reason_never_yields_an_action() {
    assert_anthropic_stream_decision(false, true, false);
}

#[test]
fn anthropic_mismatched_event_type_never_yields_an_action() {
    assert_anthropic_stream_decision(false, false, true);
}

#[test]
fn anthropic_stream_sends_workspace_header_only_when_configured() {
    use crate::brain::AnthropicDecisionSource;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    for workspace_id in [None, Some("wrkspc_stream_test")] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut received = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).expect("request");
                if count == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..count]);
            }
            socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("response");
            String::from_utf8_lossy(&received).to_ascii_lowercase()
        });
        let mut source = AnthropicDecisionSource::with_endpoint_and_extensions(
            "test-model",
            "local-test-key",
            1,
            endpoint,
            &[],
        );
        if let Some(workspace_id) = workspace_id {
            source = source
                .with_workspace_id(workspace_id)
                .expect("valid workspace id");
        }
        let result = source.choose_with_streaming_for_test("read README", &[], None, true);
        assert!(result.is_err(), "an empty stream never yields an action");
        let received = server.join().expect("server completed");

        assert!(received.contains("authorization: bearer local-test-key"));
        assert!(!received.contains("x-api-key: local-test-key"));
        assert!(received.contains("anthropic-version:"));
        assert_eq!(
            received.contains("anthropic-workspace-id:"),
            workspace_id.is_some()
        );
        if let Some(workspace_id) = workspace_id {
            assert!(received.contains(&format!("anthropic-workspace-id: {workspace_id}")));
        }
    }
}
