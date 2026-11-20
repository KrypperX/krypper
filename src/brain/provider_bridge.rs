//! Provider-neutral, fail-closed extension declaration bridge.
//!
//! Only host-authorized descriptors may enter provider schemas. This module
//! does not register executors or bypass the existing Harness dispatch.

use crate::tools::capability_policy::{
    evaluate, CapabilityContext, CapabilityDenial, CapabilityGrant,
};
use crate::tools::extension_contract::{
    validate_descriptors, ExtensionContractError, ExtensionDescriptor,
};
use serde_json::{json, Value};

/// A validated declaration and the host-issued grant for one task.
pub(crate) struct ExtensionTool<'a> {
    pub descriptor: ExtensionDescriptor<'a>,
    pub description: &'a str,
    pub parameters: Value,
    pub grant: &'a CapabilityGrant,
}

/// Errors that prevent an extension from being advertised to a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BridgeError {
    InvalidContract(ExtensionContractError),
    Denied(CapabilityDenial),
    InvalidSchema,
    UnknownTool,
}

/// Build declarations in the existing OpenAI-function shape used by Brain.
///
/// An advertised tool is not executable until Harness separately validates
/// its grant and routes it through the existing authorized dispatch boundary.
pub(crate) fn authorized_declarations(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
) -> Result<Vec<Value>, BridgeError> {
    let descriptors = tools
        .iter()
        .map(|tool| tool.descriptor.clone())
        .collect::<Vec<_>>();
    validate_descriptors(&descriptors).map_err(BridgeError::InvalidContract)?;
    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        evaluate(&tool.descriptor, context, Some(tool.grant)).map_err(BridgeError::Denied)?;
        if tool.parameters.get("type").and_then(Value::as_str) != Some("object")
            || !tool
                .parameters
                .get("properties")
                .is_some_and(Value::is_object)
        {
            return Err(BridgeError::InvalidSchema);
        }
        declarations.push(json!({
            "type": "function",
            "function": {
                "name": format!("{}_{}", tool.descriptor.namespace, tool.descriptor.name),
                "description": tool.description,
                "parameters": tool.parameters
            }
        }));
    }
    Ok(declarations)
}

/// Revalidate an extension call at dispatch time, not just at advertisement.
///
/// This gate must be called by the existing Harness dispatch path before any
/// extension executor runs. Model-supplied names never create grants.
pub(crate) fn authorize_dispatch(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
) -> Result<(), BridgeError> {
    let descriptors = tools
        .iter()
        .map(|tool| tool.descriptor.clone())
        .collect::<Vec<_>>();
    validate_descriptors(&descriptors).map_err(BridgeError::InvalidContract)?;
    let tool = tools
        .iter()
        .find(|tool| format!("{}_{}", tool.descriptor.namespace, tool.descriptor.name) == name)
        .ok_or(BridgeError::UnknownTool)?;
    evaluate(&tool.descriptor, context, Some(tool.grant)).map_err(BridgeError::Denied)
}

#[cfg(test)]
#[path = "provider_bridge/tests.rs"]
mod tests;
