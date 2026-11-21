//! Opt-in MCP client transport and lifecycle; connection alone grants no tools.
//!
//! The host supplies a validated stdio launch description and a task-scoped
//! grant, then this module performs only the `initialize` / `initialized`
//! handshake. `connect_stdio_authorized` is the only exported connection entry
//! point; the raw handshake is module-private. Tool discovery and invocation
//! remain separate host decisions outside this transport module.

mod authorized;
mod config;
mod handshake;
mod lifecycle;
mod stdio;

#[cfg(test)]
mod tests;

pub use authorized::{connect_stdio_authorized, AuthorizedConnectError};
pub use config::{McpConfigError, McpStdioConfig};
pub use lifecycle::{McpCancellation, McpLifecycleError, McpSession};
