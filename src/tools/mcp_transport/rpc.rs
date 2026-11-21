//! Bounded JSON-RPC exchange for MCP tool operations.

use super::lifecycle::{McpCancellation, McpLifecycleError};
use super::stdio::{self, StdioChild};
use serde_json::Value;
use std::time::Instant;

const MAX_NOTIFICATIONS: usize = 16;

pub(super) fn exchange(
    io: &mut StdioChild,
    id: u64,
    request: Value,
    deadline: Instant,
    cancel: &McpCancellation,
) -> Result<Value, McpLifecycleError> {
    let mut line = request.to_string();
    line.push('\n');
    stdio::write_all(&mut io.stdin, line.as_bytes(), deadline, cancel)?;

    let mut notifications = 0;
    loop {
        let response = stdio::read_line(
            &mut io.child,
            &mut io.stdout,
            &mut io.stderr,
            &mut io.stderr_used,
            &mut io.pending,
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
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(McpLifecycleError::ProtocolViolation);
        }
        if value.get("error").is_some() {
            return Err(McpLifecycleError::ServerError);
        }
        let result = value
            .get("result")
            .and_then(Value::as_object)
            .ok_or(McpLifecycleError::ProtocolViolation)?;
        return Ok(Value::Object(result.clone()));
    }
}
