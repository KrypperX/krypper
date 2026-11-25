//! Provider-specific conversion of host-authorized extension declarations.
//!
//! Descriptor, grant, schema and argument authorization is owned by Tools. This
//! module adapts already-validated declarations to each model provider format.

use crate::contracts::extensions::CapabilityContext;
use crate::tools::extension_authorization::{
    authorize_extension_dispatch, authorize_extension_dispatch_with_arguments,
    validate_authorized_tools, validate_extension_declarations,
};
use serde_json::{json, Value};

pub use crate::contracts::extensions::{BridgeError, ExtensionTool};

#[cfg(test)]
use crate::contracts::extensions::{CapabilityDenial, CapabilityGrant, ExtensionDescriptor};

/// Build declarations in the common function shape consumed by provider adapters.
///
/// An advertised tool is not executable until Harness separately validates its
/// grant and routes it through the existing authorized dispatch boundary.
pub fn authorized_declarations(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
) -> Result<Vec<Value>, BridgeError> {
    validate_authorized_tools(tools, context)?;
    Ok(tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.descriptor.qualified_name(),
                    "description": tool.description,
                    "parameters": tool.parameters
                }
            })
        })
        .collect())
}

/// Revalidate an extension call at dispatch time, not just at advertisement.
///
/// This gate must be called by the existing Harness dispatch path before any
/// extension executor runs.
pub fn authorize_dispatch(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
) -> Result<(), BridgeError> {
    authorize_extension_dispatch(tools, context, name)
}

/// Validate model-supplied arguments before any host executor can receive them.
pub(crate) fn authorize_dispatch_with_arguments(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
    arguments: &Value,
) -> Result<(), BridgeError> {
    authorize_extension_dispatch_with_arguments(tools, context, name, arguments)
}

/// Validate declaration envelopes at a provider request-builder boundary.
pub(crate) fn validate_declarations(declarations: &[Value]) -> Result<(), BridgeError> {
    validate_extension_declarations(declarations)
}

/// Convert already-authorized declarations to the provider's existing schema.
///
/// The caller must pass only the result of `authorized_declarations`.
/// Conversion does not confer execution authority.
pub fn responses_declarations(declarations: &[Value]) -> Vec<Value> {
    declarations
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "type": "function",
                "name": function.get("name")?,
                "description": function.get("description")?,
                "parameters": function.get("parameters")?,
                "strict": false
            }))
        })
        .collect()
}

/// Translate authorized declarations to Anthropic's existing strict schema.
pub fn anthropic_declarations(declarations: &[Value]) -> Vec<Value> {
    declarations
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name": function.get("name")?,
                "description": function.get("description")?,
                "strict": true,
                "input_schema": super::tools_schema::anthropic_strict_schema(
                    function.get("parameters")?
                )
            }))
        })
        .collect()
}

/// Translate authorized declarations to Gemini's supported schema subset.
pub fn gemini_declarations(declarations: &[Value]) -> Vec<Value> {
    declarations
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name": function.get("name")?,
                "description": function.get("description")?,
                "parameters": super::tools_schema::gemini_supported_schema(
                    function.get("parameters")?
                )
            }))
        })
        .collect()
}

#[cfg(test)]
#[path = "provider_bridge/tests.rs"]
mod tests;
