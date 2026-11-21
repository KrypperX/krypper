//! Untrusted MCP tool advertisements mapped to the existing extension contract.

use super::lifecycle::McpCancellation;
use super::rpc;
use super::stdio::StdioChild;
use super::{untrusted_text, McpToolError, DESCRIPTION_PREFIX, MAX_DESCRIPTION_BYTES};
use crate::brain::provider_bridge::{
    authorized_declarations, validate_declarations, ExtensionTool,
};
use crate::tools::capability_policy::{evaluate, CapabilityContext, CapabilityGrant};
use crate::tools::extension_contract::{
    validate_descriptors, ExtensionAuthority, ExtensionDescriptor, ExtensionEffect,
    EXTENSION_CONTRACT_VERSION,
};
use serde_json::{json, Value};
use std::time::Instant;
use uuid::Uuid;

const MAX_DISCOVERED_TOOLS: usize = 64;

/// One advertised MCP tool that passed contract, schema and grant checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpTool {
    pub(super) namespace: String,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) schema: Value,
    pub(super) grant: CapabilityGrant,
    pub(super) session_id: Uuid,
}

impl McpTool {
    /// Stable provider-facing name for this tool.
    pub fn qualified_name(&self) -> String {
        format!("{}_{}", self.namespace, self.name)
    }

    /// Provider-facing description; its text is untrusted server data.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Validated JSON Schema object for model arguments.
    pub fn parameters(&self) -> &Value {
        &self.schema
    }

    /// View as the existing extension declaration type for provider advertising.
    pub fn extension_tool(&self) -> ExtensionTool<'_> {
        self.extension_tool_with_grant(&self.grant)
    }

    pub(super) fn extension_tool_with_grant<'a>(
        &'a self,
        grant: &'a CapabilityGrant,
    ) -> ExtensionTool<'a> {
        ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: &self.namespace,
                name: &self.name,
                effect: self.grant.effect,
                authority: ExtensionAuthority::Task,
            },
            description: &self.description,
            parameters: self.schema.clone(),
            grant,
        }
    }
}

struct Candidate {
    namespace: String,
    name: String,
    description: String,
    schema: Value,
    grant: Option<CapabilityGrant>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn list_tools(
    io: &mut StdioChild,
    id: u64,
    deadline: Instant,
    cancel: &McpCancellation,
    scope: &str,
    context: CapabilityContext,
    discovery_grant: Option<&CapabilityGrant>,
    tool_grants: &[CapabilityGrant],
    session_id: Uuid,
) -> Result<Vec<McpTool>, McpToolError> {
    let launch = ExtensionDescriptor {
        version: EXTENSION_CONTRACT_VERSION,
        namespace: "mcp",
        name: "discover_tools",
        effect: ExtensionEffect::ExecuteProcess,
        authority: ExtensionAuthority::TaskAndHostGrant,
    };
    evaluate(&launch, context, discovery_grant)?;

    let namespace = format!("mcp_{scope}");
    if scope.is_empty()
        || validate_descriptors(&[ExtensionDescriptor {
            version: EXTENSION_CONTRACT_VERSION,
            namespace: &namespace,
            name: "probe",
            effect: ExtensionEffect::ReadWorkspace,
            authority: ExtensionAuthority::Task,
        }])
        .is_err()
    {
        return Err(McpToolError::InvalidScope);
    }

    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {}
    });
    let result = rpc::exchange(io, id, request, deadline, cancel)?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(McpToolError::InvalidToolList)?;
    if tools.len() > MAX_DISCOVERED_TOOLS {
        return Err(McpToolError::InvalidToolList);
    }

    let mut candidates = Vec::with_capacity(tools.len());
    for value in tools {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or(McpToolError::InvalidToolList)?;
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let schema = value
            .get("inputSchema")
            .cloned()
            .ok_or(McpToolError::InvalidToolList)?;
        let grant = tool_grants
            .iter()
            .find(|grant| {
                grant.namespace == namespace
                    && grant.operation == name
                    && matches!(
                        grant.effect,
                        ExtensionEffect::ReadWorkspace | ExtensionEffect::MutateWorkspace
                    )
            })
            .cloned();
        candidates.push(Candidate {
            namespace: namespace.clone(),
            name: name.to_owned(),
            description: format!(
                "{DESCRIPTION_PREFIX}{}",
                untrusted_text(description, MAX_DESCRIPTION_BYTES)
            ),
            schema,
            grant,
        });
    }

    let descriptors = candidates
        .iter()
        .map(|candidate| ExtensionDescriptor {
            version: EXTENSION_CONTRACT_VERSION,
            namespace: &candidate.namespace,
            name: &candidate.name,
            effect: ExtensionEffect::ReadWorkspace,
            authority: ExtensionAuthority::Task,
        })
        .collect::<Vec<_>>();
    validate_descriptors(&descriptors)?;

    let declarations = candidates
        .iter()
        .map(|candidate| {
            json!({
                "type": "function",
                "function": {
                    "name": format!("{}_{}", candidate.namespace, candidate.name),
                    "description": candidate.description,
                    "parameters": candidate.schema
                }
            })
        })
        .collect::<Vec<_>>();
    validate_declarations(&declarations)?;

    let granted = candidates
        .into_iter()
        .filter_map(|candidate| {
            candidate.grant.map(|grant| McpTool {
                namespace: candidate.namespace,
                name: candidate.name,
                description: candidate.description,
                schema: candidate.schema,
                grant,
                session_id,
            })
        })
        .collect::<Vec<_>>();
    let tools = granted
        .iter()
        .map(McpTool::extension_tool)
        .collect::<Vec<_>>();
    authorized_declarations(&tools, context)?;
    Ok(granted)
}
