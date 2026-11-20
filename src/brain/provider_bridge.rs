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
    NameCollision,
}

/// Reject unsupported JSON Schema constructs before advertising a tool.
/// The dispatch validator must enforce every accepted constraint.
fn valid_parameters(parameters: &Value) -> bool {
    valid_schema(parameters) && parameters.get("type").and_then(Value::as_str) == Some("object")
}

fn valid_schema(schema: &Value) -> bool {
    let Some(fields) = schema.as_object() else {
        return false;
    };
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type" | "properties" | "required" | "additionalProperties" | "items"
        )
    }) {
        return false;
    }
    let Some(kind) = fields.get("type").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        kind,
        "object" | "array" | "string" | "boolean" | "integer" | "number" | "null"
    ) {
        return false;
    }
    match kind {
        "object" => {
            if fields.contains_key("items") {
                return false;
            }
            if fields
                .get("additionalProperties")
                .is_some_and(|v| v != &Value::Bool(false))
            {
                return false;
            }
            let Some(properties) = fields.get("properties").and_then(Value::as_object) else {
                return false;
            };
            if !properties.values().all(valid_schema) {
                return false;
            }
            match fields.get("required") {
                None => true,
                Some(Value::Array(required)) => required
                    .iter()
                    .all(|v| v.as_str().is_some_and(|k| properties.contains_key(k))),
                _ => false,
            }
        }
        "array" => {
            !fields.contains_key("properties")
                && !fields.contains_key("required")
                && !fields.contains_key("additionalProperties")
                && fields.get("items").is_some_and(valid_schema)
        }
        _ => fields.len() == 1,
    }
}

fn validate_value(schema: &Value, value: &Value) -> bool {
    match schema["type"].as_str() {
        Some("object") => {
            let (Some(values), Some(properties)) =
                (value.as_object(), schema["properties"].as_object())
            else {
                return false;
            };
            let required = schema.get("required").and_then(Value::as_array);
            if required.is_some_and(|items| {
                items
                    .iter()
                    .any(|k| !values.contains_key(k.as_str().unwrap_or("")))
            }) {
                return false;
            }
            values.iter().all(|(key, value)| {
                properties
                    .get(key)
                    .is_some_and(|sub| validate_value(sub, value))
            })
        }
        Some("array") => value
            .as_array()
            .is_some_and(|items| items.iter().all(|v| validate_value(&schema["items"], v))),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("number") => value.is_number(),
        Some("null") => value.is_null(),
        _ => false,
    }
}

/// Reject an extension name that could shadow a built-in tool or another extension.
fn validate_extension_names(tools: &[ExtensionTool<'_>]) -> Result<(), BridgeError> {
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        let name = format!("{}_{}", tool.descriptor.namespace, tool.descriptor.name);
        if name == "complete_task"
            || crate::tools::registry::workspace_operations()
                .iter()
                .any(|operation| operation.name() == name)
            || !seen.insert(name)
        {
            return Err(BridgeError::NameCollision);
        }
    }
    Ok(())
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
    validate_extension_names(tools)?;
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
    validate_extension_names(tools)?;
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
    if !valid_parameters(&tool.parameters) {
        return Err(BridgeError::InvalidSchema);
    }
    if !validate_value(&tool.parameters, arguments) {
        return Err(BridgeError::InvalidArguments);
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
