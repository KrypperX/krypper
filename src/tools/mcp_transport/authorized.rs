//! Host-authorized MCP connection boundary.
//!
//! An MCP handshake is a process launch, not a capability grant. Reuse the
//! existing task-scoped extension policy before crossing that boundary.

use super::lifecycle::connect_stdio;
use super::{McpCancellation, McpLifecycleError, McpSession, McpStdioConfig};
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
/// has been validated. This is the only exported connection path; the raw
/// handshake is module-private so callers cannot skip the grant check. This
/// does not grant access to discovered MCP tools: each tool invocation must
/// still pass its own host-side policy checks.
///
/// On supported Unix hosts, the grant authorizes Krypper to spawn the configured
/// process. The stdio transport clears the inherited environment except for a
/// minimal `PATH` and `LANG`, and groups the child for bounded cleanup, but it
/// adds no filesystem or network confinement. The server runs as the Krypper
/// process's OS user, subject to external OS policy. Host grants and discovered
/// tool effects gate Krypper's calls; they do not constrain other behavior of
/// the server process. Windows currently returns `PlatformUnavailable` before
/// spawning a child.
///
/// The host owns `config`, `context` and `grant` as one authorized launch
/// decision; never deserialize them from model output or untrusted MCP server
/// metadata.
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
    capability_policy::evaluate(&launch, context, grant).map_err(AuthorizedConnectError::Denied)?;
    connect_stdio(config, cancel).map_err(AuthorizedConnectError::Transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use uuid::Uuid;

    fn config() -> McpStdioConfig {
        McpStdioConfig {
            executable: "/definitely/not/a/real/mcp-server".into(),
            args: vec![],
            timeout: Duration::from_secs(1),
        }
    }

    fn script_config(script: String) -> McpStdioConfig {
        McpStdioConfig {
            executable: "/bin/sh".into(),
            args: vec!["-c".into(), script],
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
            Err(AuthorizedConnectError::Denied(
                CapabilityDenial::MissingGrant
            ))
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
            Err(AuthorizedConnectError::Denied(
                CapabilityDenial::HostApprovalRequired
            ))
        ));
    }

    #[test]
    fn denies_unauthorized_task_wrong_operation_and_effect() {
        let task_id = Uuid::new_v4();
        let authorized = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        let unauthorized = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: false,
        };
        let cancel = McpCancellation::new();
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, unauthorized, Some(&grant(task_id))),
            Err(AuthorizedConnectError::Denied(
                CapabilityDenial::TaskUnauthorized
            ))
        ));
        let mut wrong_operation = grant(task_id);
        wrong_operation.operation = "other".into();
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, authorized, Some(&wrong_operation)),
            Err(AuthorizedConnectError::Denied(
                CapabilityDenial::WrongOperation
            ))
        ));
        let mut wrong_effect = grant(task_id);
        wrong_effect.effect = ExtensionEffect::ReadWorkspace;
        assert!(matches!(
            connect_stdio_authorized(&config(), &cancel, authorized, Some(&wrong_effect)),
            Err(AuthorizedConnectError::Denied(
                CapabilityDenial::WrongEffect
            ))
        ));
    }

    #[test]
    fn denied_grant_never_launches_the_server() {
        let task_id = Uuid::new_v4();
        let context = CapabilityContext {
            task_id: Uuid::new_v4(),
            generation: 3,
            task_authorized: true,
        };
        let marker_directory = TempDir::with_prefix("krypper-mcp-authorized-");
        let marker = marker_directory.path().join(task_id.to_string());
        let script = format!("printf spawned > '{}'\n", marker.display());
        let result = connect_stdio_authorized(
            &script_config(script),
            &McpCancellation::new(),
            context,
            Some(&grant(task_id)),
        );
        assert!(matches!(
            result,
            Err(AuthorizedConnectError::Denied(CapabilityDenial::WrongTask))
        ));
        thread::sleep(Duration::from_millis(50));
        assert!(!marker.exists());
    }

    #[test]
    fn failure_injection_reports_transport_failure_after_authorization() {
        let task_id = Uuid::new_v4();
        let context = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        assert!(matches!(
            connect_stdio_authorized(
                &config(),
                &McpCancellation::new(),
                context,
                Some(&grant(task_id))
            ),
            Err(AuthorizedConnectError::Transport(
                McpLifecycleError::SpawnFailed
            ))
        ));
    }
}
