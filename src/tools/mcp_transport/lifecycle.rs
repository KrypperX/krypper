//! Opt-in MCP client lifecycle and error mapping.

use super::config::McpConfigError;
use super::discovery::McpTool;
use super::stdio::{self, StdioChild};
use super::McpToolError;
use crate::tools::capability_policy::{CapabilityContext, CapabilityGrant};
use serde_json::Value;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Fail-closed outcomes for a connection or handshake attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpLifecycleError {
    InvalidConfig(McpConfigError),
    SpawnFailed,
    Io(io::ErrorKind),
    Timeout,
    Cancelled,
    Disconnected,
    ProtocolViolation,
    UnsupportedProtocolVersion,
    ServerError,
    OutputLimitExceeded,
    CleanupFailed,
}

/// A cloneable cancellation signal checked throughout the handshake.
#[derive(Clone, Debug, Default)]
pub struct McpCancellation(Arc<AtomicBool>);

impl McpCancellation {
    /// Create an uncancelled signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation of an in-flight connection.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A connected MCP stdio client after a completed handshake.
///
/// Connecting does not discover or invoke tools and creates no capability grant.
pub struct McpSession {
    io: StdioChild,
    protocol_version: String,
    timeout: Duration,
    next_id: u64,
}

impl McpSession {
    /// Return the negotiated MCP protocol version.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Discover advertised tools after a separate host-issued discovery grant.
    ///
    /// Only tools with a matching host grant are returned; ungranted tools stay
    /// invisible to the provider. Every name, description and schema is treated
    /// as untrusted server data and validated through the existing extension
    /// contract before it can be advertised.
    #[allow(clippy::too_many_arguments)]
    pub fn discover_tools_authorized(
        &mut self,
        scope: &str,
        context: CapabilityContext,
        discovery_grant: Option<&CapabilityGrant>,
        tool_grants: &[CapabilityGrant],
        cancel: &McpCancellation,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let id = self.next_request_id();
        let deadline = Instant::now() + self.timeout;
        super::discovery::list_tools(
            &mut self.io,
            id,
            deadline,
            cancel,
            scope,
            context,
            discovery_grant,
            tool_grants,
        )
    }

    /// Invoke one discovered tool after rechecking the current host grant.
    ///
    /// The call goes through the existing extension argument validator and the
    /// same stdio transport; no separate executor or permission model is used.
    pub fn invoke_tool_authorized(
        &mut self,
        tool: &McpTool,
        context: CapabilityContext,
        grant: Option<&CapabilityGrant>,
        arguments: &Value,
        cancel: &McpCancellation,
    ) -> Result<String, McpToolError> {
        let id = self.next_request_id();
        let deadline = Instant::now() + self.timeout;
        super::invocation::call_tool(
            &mut self.io,
            id,
            deadline,
            cancel,
            tool,
            context,
            grant,
            arguments,
        )
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Terminate the server process and confirm cleanup.
    pub fn shutdown(mut self) -> Result<(), McpLifecycleError> {
        self.io.child.terminate()
    }
}

/// Validate host configuration, launch the stdio server and complete the MCP handshake.
pub(super) fn connect_stdio(
    config: &super::config::McpStdioConfig,
    cancel: &McpCancellation,
) -> Result<McpSession, McpLifecycleError> {
    config
        .validate()
        .map_err(McpLifecycleError::InvalidConfig)?;
    if cancel.is_cancelled() {
        return Err(McpLifecycleError::Cancelled);
    }
    let deadline = Instant::now() + config.timeout;
    let mut stdio = stdio::spawn(config, deadline, cancel)?;
    let protocol_version = match super::handshake::initialize(&mut stdio, deadline, cancel) {
        Ok(version) => version,
        Err(error) => return Err(cleanup_on_error(error, || stdio.child.terminate())),
    };
    Ok(McpSession {
        io: stdio,
        protocol_version,
        timeout: config.timeout,
        next_id: 2,
    })
}

pub(super) fn cleanup_on_error(
    error: McpLifecycleError,
    cleanup: impl FnOnce() -> Result<(), McpLifecycleError>,
) -> McpLifecycleError {
    match cleanup() {
        Ok(()) => error,
        Err(_) => McpLifecycleError::CleanupFailed,
    }
}
