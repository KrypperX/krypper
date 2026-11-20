//! Opt-in MCP client transport configuration; no process is spawned by validation.
//!
//! This module intentionally does not discover tools, create grants, or execute
//! provider-supplied commands. Transport activation is a separate host decision.
use std::time::Duration;

/// Upper bound for a single MCP transport operation.
pub const MAX_MCP_TIMEOUT: Duration = Duration::from_secs(60);

/// A host-supplied local MCP server launch description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpStdioConfig {
    /// Absolute executable path selected by the host, never a shell command.
    pub executable: std::path::PathBuf,
    /// Literal argv elements; no shell interpolation is performed.
    pub args: Vec<String>,
    /// Timeout for connection and handshake.
    pub timeout: Duration,
}

/// Configuration errors detected before attempting any connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpConfigError {
    MissingExecutable,
    RelativeExecutable,
    InvalidExecutable,
    InvalidArgument,
    TooManyArguments,
    InvalidTimeout,
}

impl McpStdioConfig {
    /// Validate the host's opt-in configuration without launching a subprocess.
    ///
    /// No network permission, task grant, or tool execution follows from success.
    pub fn validate(&self) -> Result<(), McpConfigError> {
        if self.executable.as_os_str().is_empty() {
            return Err(McpConfigError::MissingExecutable);
        }
        if !self.executable.is_absolute() {
            return Err(McpConfigError::RelativeExecutable);
        }
        // Reject embedded NUL before a future stdio launcher reaches the OS API.
        if self.executable.to_string_lossy().contains('\0') {
            return Err(McpConfigError::InvalidExecutable);
        }
        if self.args.len() > 128 {
            return Err(McpConfigError::TooManyArguments);
        }
        if self.args.iter().any(|arg| arg.contains('\0') || arg.len() > 16 * 1024) {
            return Err(McpConfigError::InvalidArgument);
        }
        if self.timeout.is_zero() || self.timeout > MAX_MCP_TIMEOUT {
            return Err(McpConfigError::InvalidTimeout);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> McpStdioConfig {
        McpStdioConfig {
            executable: std::path::PathBuf::from("/usr/bin/mcp-server"),
            args: vec!["--stdio".into()],
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn accepts_bounded_explicit_stdio_configuration() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn rejects_missing_and_relative_executables() {
        let mut config = valid();
        config.executable = "".into();
        assert_eq!(config.validate(), Err(McpConfigError::MissingExecutable));
        config.executable = "mcp-server".into();
        assert_eq!(config.validate(), Err(McpConfigError::RelativeExecutable));
    }

    #[test]
    fn rejects_executable_with_embedded_nul() {
        let mut config = valid();
        config.executable = "/usr/bin/mcp\0server".into();
        assert_eq!(config.validate(), Err(McpConfigError::InvalidExecutable));
    }

    #[test]
    fn rejects_invalid_timeouts_and_arguments() {
        let mut config = valid();
        config.timeout = Duration::ZERO;
        assert_eq!(config.validate(), Err(McpConfigError::InvalidTimeout));
        config.timeout = MAX_MCP_TIMEOUT + Duration::from_nanos(1);
        assert_eq!(config.validate(), Err(McpConfigError::InvalidTimeout));
        config = valid();
        config.args = vec!["bad\0arg".into()];
        assert_eq!(config.validate(), Err(McpConfigError::InvalidArgument));
        config.args = vec![String::new(); 129];
        assert_eq!(config.validate(), Err(McpConfigError::TooManyArguments));
    }
}
