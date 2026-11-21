//! MCP initialize/initialized handshake and protocol-version negotiation.

use super::lifecycle::McpLifecycleError;
use super::stdio::{self, StdioChild};
use serde_json::{json, Value};
use std::time::Instant;

/// Protocol versions understood by this client, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2024-11-05"];
const MAX_NOTIFICATIONS: usize = 16;

pub(super) fn initialize(
    stdio: &mut StdioChild,
    deadline: Instant,
    cancel: &super::lifecycle::McpCancellation,
) -> Result<String, McpLifecycleError> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0],
            "capabilities": {},
            "clientInfo": {
                "name": "krypper",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });
    let mut line = request.to_string();
    line.push('\n');
    stdio::write_all(&mut stdio.stdin, line.as_bytes(), deadline, cancel)?;

    let mut notifications = 0;
    let result = loop {
        let response = stdio::read_line(
            &mut stdio.child,
            &mut stdio.stdout,
            &mut stdio.stderr,
            &mut stdio.stderr_used,
            &mut stdio.pending,
            deadline,
            cancel,
        )?;
        let text =
            std::str::from_utf8(&response).map_err(|_| McpLifecycleError::ProtocolViolation)?;
        let value: Value =
            serde_json::from_str(text).map_err(|_| McpLifecycleError::ProtocolViolation)?;
        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpLifecycleError::ProtocolViolation);
        }
        if value.get("id").is_none() && value.get("method").is_some() {
            notifications += 1;
            if notifications > MAX_NOTIFICATIONS {
                return Err(McpLifecycleError::ProtocolViolation);
            }
            continue;
        }
        break parse_response(value)?;
    };
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or(McpLifecycleError::ProtocolViolation)?;
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return Err(McpLifecycleError::UnsupportedProtocolVersion);
    }

    stdio::write_all(
        &mut stdio.stdin,
        b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        deadline,
        cancel,
    )?;
    if matches!(stdio.child.try_wait(), Ok(Some(_))) {
        return Err(McpLifecycleError::Disconnected);
    }
    Ok(version.to_owned())
}

fn parse_response(value: Value) -> Result<Value, McpLifecycleError> {
    if value.get("id").and_then(Value::as_u64) != Some(1) {
        return Err(McpLifecycleError::ProtocolViolation);
    }
    if value.get("error").is_some() {
        return Err(McpLifecycleError::ServerError);
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpLifecycleError::ProtocolViolation)?;
    if result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .is_none()
        || !result.get("capabilities").is_some_and(Value::is_object)
        || !result
            .get("serverInfo")
            .and_then(Value::as_object)
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .is_some()
        || !result
            .get("serverInfo")
            .and_then(Value::as_object)
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .is_some()
    {
        return Err(McpLifecycleError::ProtocolViolation);
    }
    Ok(Value::Object(result.clone()))
}
