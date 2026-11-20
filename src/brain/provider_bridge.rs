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
    InvalidArguments,
    UnknownTool,
}

/// Both advertisement and dispatch must reject unsupported parameter schemas.
fn valid_parameters(parameters: &Value) -> bool {
    parameters.get("type").and_then(Value::as_str) == Some("object")
        && parameters.get("properties").is_some_and(Value::is_object)
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
        if !valid_parameters(&tool.parameters) {
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
    evaluate(&tool.descriptor, context, Some(tool.grant)).map_err(BridgeError::Denied)?;
    if !valid_parameters(&tool.parameters) {
        return Err(BridgeError::InvalidSchema);
    }
    Ok(())
}

/// Validate model-supplied arguments against the registered object's declared
/// properties before any host executor can receive them. This deliberately
/// supports only a narrow JSON Schema subset and rejects unsupported keywords
/// rather than silently weakening the host's declared contract.
pub(crate) fn authorize_dispatch_with_arguments(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
    arguments: &Value,
) -> Result<(), BridgeError> {
    authorize_dispatch(tools, context, name)?;
    let tool = tools
        .iter()
        .find(|tool| format!("{}_{}", tool.descriptor.namespace, tool.descriptor.name) == name)
        .ok_or(BridgeError::UnknownTool)?;
    let properties = tool.parameters["properties"]
        .as_object()
        .ok_or(BridgeError::InvalidSchema)?;
    let supplied = arguments.as_object().ok_or(BridgeError::InvalidArguments)?;
    let required = tool.parameters.get("required");
    let required = match required {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().ok_or(BridgeError::InvalidSchema))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(BridgeError::InvalidSchema),
    };
    if required.iter().any(|key| !properties.contains_key(*key)) {
        return Err(BridgeError::InvalidSchema);
    }
    if required.iter().any(|key| !supplied.contains_key(*key)) {
        return Err(BridgeError::InvalidArguments);
    }
    if supplied.keys().any(|key| !properties.contains_key(key)) {
        return Err(BridgeError::InvalidArguments);
    }
    for (key, value) in supplied {
        let schema = &properties[key];
        let kind = schema
            .get("type")
            .and_then(Value::as_str)
            .ok_or(BridgeError::InvalidSchema)?;
        let matches = match kind {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "null" => value.is_null(),
            _ => return Err(BridgeError::InvalidSchema),
        };
        if !matches {
            return Err(BridgeError::InvalidArguments);
        }
    }
    Ok(())
}

/// Convert already-authorized declarations to the provider's existing schema.
///
/// The caller must pass only the result of `authorized_declarations`.
/// Conversion does not confer execution authority.
pub(crate) fn responses_declarations(declarations: &[Value]) -> Vec<Value> {
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
pub(crate) fn anthropic_declarations(declarations: &[Value]) -> Vec<Value> {
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
pub(crate) fn gemini_declarations(declarations: &[Value]) -> Vec<Value> {
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
