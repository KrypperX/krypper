//! Opt-in MCP client transport and lifecycle; connection alone grants no tools.
//!
//! The host supplies a validated stdio launch description, then this module
//! performs only the `initialize` / `initialized` handshake. Tool discovery and
//! invocation remain separate host decisions outside this transport module.

mod config;
mod handshake;
mod lifecycle;
mod stdio;

#[cfg(test)]
mod tests;

pub use config::{McpConfigError, McpStdioConfig};
pub use lifecycle::{connect_stdio, McpCancellation, McpLifecycleError, McpSession};
