//! Windows compile-time stub. MCP stdio must fail before creating a child process.

#![allow(dead_code)]

use super::config::McpStdioConfig;
use super::lifecycle::{McpCancellation, McpLifecycleError};
use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::time::Instant;

pub(super) const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub(super) struct StdioChild {
    pub(super) child: ManagedChild,
    pub(super) stdin: ChildStdin,
    pub(super) stdout: ChildStdout,
    pub(super) stderr: ChildStderr,
    pub(super) pending: Vec<u8>,
    pub(super) stderr_used: usize,
}

pub(super) struct ManagedChild(Option<Child>);

impl ManagedChild {
    pub(super) fn has_exited(&mut self) -> io::Result<bool> {
        match self.0.as_mut() {
            Some(child) => child.try_wait().map(|status| status.is_some()),
            None => Ok(false),
        }
    }

    pub(super) fn terminate(&mut self) -> Result<(), McpLifecycleError> {
        Err(McpLifecycleError::PlatformUnavailable)
    }
}

pub(super) fn spawn(
    _config: &McpStdioConfig,
    _deadline: Instant,
    _cancel: &McpCancellation,
) -> Result<StdioChild, McpLifecycleError> {
    Err(McpLifecycleError::PlatformUnavailable)
}

pub(super) fn write_all(
    _stdin: &mut ChildStdin,
    _bytes: &[u8],
    _deadline: Instant,
    _cancel: &McpCancellation,
) -> Result<(), McpLifecycleError> {
    Err(McpLifecycleError::PlatformUnavailable)
}

pub(super) fn read_line(
    _child: &mut ManagedChild,
    _stdout: &mut ChildStdout,
    _stderr: &mut ChildStderr,
    _stderr_used: &mut usize,
    _pending: &mut Vec<u8>,
    _deadline: Instant,
    _cancel: &McpCancellation,
) -> Result<Vec<u8>, McpLifecycleError> {
    Err(McpLifecycleError::PlatformUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn stdio_is_unavailable_before_any_child_can_be_started() {
        let config = McpStdioConfig {
            executable: PathBuf::from("must-not-be-launched.exe"),
            args: Vec::new(),
            timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            spawn(
                &config,
                Instant::now() + config.timeout,
                &McpCancellation::new()
            ),
            Err(McpLifecycleError::PlatformUnavailable)
        ));
    }
}
