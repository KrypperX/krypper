//! Opt-in MCP client lifecycle and error mapping.

use super::config::McpConfigError;
use super::stdio::{self, StdioChild};
use std::io;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
    child: stdio::ManagedChild,
    _stdio: (ChildStdin, ChildStdout, ChildStderr),
    protocol_version: String,
}

impl McpSession {
    /// Return the negotiated MCP protocol version.
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Terminate the server process and confirm cleanup.
    pub fn shutdown(mut self) -> Result<(), McpLifecycleError> {
        self.child.terminate()
    }
}

/// Validate host configuration, launch the stdio server and complete the MCP handshake.
pub fn connect_stdio(
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
    let mut stdio = stdio::spawn(config, deadline)?;
    let protocol_version = match super::handshake::initialize(&mut stdio, deadline, cancel) {
        Ok(version) => version,
        Err(error) => return Err(cleanup_on_error(stdio, error)),
    };
    Ok(McpSession {
        child: stdio.child,
        _stdio: (stdio.stdin, stdio.stdout, stdio.stderr),
        protocol_version,
    })
}

fn cleanup_on_error(mut stdio: StdioChild, error: McpLifecycleError) -> McpLifecycleError {
    match stdio.child.terminate() {
        Ok(()) => error,
        Err(_) => McpLifecycleError::CleanupFailed,
    }
}
