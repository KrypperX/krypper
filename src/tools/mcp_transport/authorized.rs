//! Host-authorized MCP connection boundary.
//!
//! An MCP handshake is a process launch, not a capability grant. Reuse the
//! existing task-scoped extension policy before crossing that boundary.

use super::{connect_stdio, McpCancellation, McpLifecycleError, McpSession, McpStdioConfig};
use crate::tools::capability_policy::{self, CapabilityContext, CapabilityDenial, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};

/// A denied launch never attempts to spawn a child process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizedConnectError {
    Denied(CapabilityDenial),
    Transport(McpLifecycleError),
}

/// Connect to an MCP server only after the host's task-scoped process grant
/// has been validated. This does not grant access to discovered MCP tools:
/// each tool invocation must still pass its own policy and sandbox checks.
///
/// The host owns `context` and `grant`; never deserialize them from model
/// output or untrusted MCP server metadata.
pub fn connect_stdio_authorized(
    config: &McpStdioConfig,
    cancel: &McpCancellation,
    context: CapabilityContext,
    grant: Option<&CapabilityGrant>,
) -> Result<McpSession, AuthorizedConnectError> {
    let launch = ExtensionDescriptor {
        version: EXTENSION_CONTRACT_VERSION,
        namespace: "mcp",
        name: "connect_stdio",
        effect: ExtensionEffect::ExecuteProcess,
        authority: ExtensionAuthority::TaskAndHostGrant,
    };
    capability_policy::evaluate(&launch, context, grant)
        .map_err(AuthorizedConnectError::Denied)?;
    connect_stdio(config, cancel).map_err(AuthorizedConnectError::Transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    fn config() -> McpStdioConfig {
        McpStdioConfig {
            executable: "/definitely/not/a/real/mcp-server".into(),
            args: vec![],
            timeout: Duration::from_secs(1),
        }
    }

    fn grant(task_id: Uuid) -> CapabilityGrant {
        CapabilityGrant {
            task_id,
            namespace: "mcp".into(),
            operation: "connect_stdio".into(),
            effect: ExtensionEffect::ExecuteProcess,
            generation: 3,
            host_approved: true,
        }
    }

    #[test]
    fn denies_missing_and_cross_task_grants_before_spawn() {
        let task_id = Uuid::new_v4();
        let context = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        let cancel = McpCancellation::new();
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, context, None),
            Err(AuthorizedConnectError::Denied(CapabilityDenial::MissingGrant))
        ));
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, context, Some(&grant(Uuid::new_v4()))),
            Err(AuthorizedConnectError::Denied(CapabilityDenial::WrongTask))
        ));
    }

    #[test]
    fn denies_revoked_and_unapproved_process_grants() {
        let task_id = Uuid::new_v4();
        let context = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        let cancel = McpCancellation::new();
        let mut stale = grant(task_id);
        stale.generation = 2;
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, context, Some(&stale)),
            Err(AuthorizedConnectError::Denied(CapabilityDenial::StaleGrant))
        ));
        let mut unapproved = grant(task_id);
        unapproved.host_approved = false;
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, context, Some(&unapproved)),
            Err(AuthorizedConnectError::Denied(CapabilityDenial::HostApprovalRequired))
        ));
    }

    #[test]
    fn approved_grant_reaches_transport_without_granting_tools() {
        let task_id = Uuid::new_v4();
        let context = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        assert!(matches!(
            connect_stdio_authorized(&config(), &McpCancellation::new(), context, Some(&grant(task_id))),
            Err(AuthorizedConnectError::Transport(McpLifecycleError::SpawnFailed))
        ));
    }
}
