//! Shared extension schema and capability checks owned by Tools.

use crate::contracts::extensions::{
    validate_descriptors, BridgeError, CapabilityContext, ExtensionEffect, ExtensionTool,
};
use crate::tools::capability_policy::evaluate;
use serde_json::Value;
use std::collections::HashSet;

/// Validate every descriptor, grant and parameter schema before advertising tools.
pub(crate) fn validate_authorized_tools(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
) -> Result<(), BridgeError> {
    validate_contract_and_effects(tools)?;
    for tool in tools {
        evaluate(&tool.descriptor, context, Some(tool.grant)).map_err(BridgeError::Denied)?;
        if !valid_parameters(&tool.parameters) {
            return Err(BridgeError::InvalidSchema);
        }
    }
    Ok(())
}

/// Revalidate one registered extension at dispatch time.
pub(crate) fn authorize_extension_dispatch(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
) -> Result<(), BridgeError> {
    authorize_tool(tools, context, name).map(|_| ())
}

/// Validate arguments before any host executor receives model-supplied values.
pub(crate) fn authorize_extension_dispatch_with_arguments(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
    arguments: &Value,
) -> Result<(), BridgeError> {
    let tool = authorize_tool(tools, context, name)?;
    if !validate_value(&tool.parameters, arguments) {
        return Err(BridgeError::InvalidArguments);
    }
    Ok(())
}

/// Validate untrusted declarations before filtering the tools that have grants.
pub(crate) fn validate_extension_declarations(declarations: &[Value]) -> Result<(), BridgeError> {
    let mut seen = HashSet::new();
    for declaration in declarations {
        let Some(function) = declaration.get("function") else {
            return Err(BridgeError::InvalidSchema);
        };
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            return Err(BridgeError::InvalidSchema);
        };
        if function
            .get("description")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(BridgeError::InvalidSchema);
        }
        let Some(parameters) = function.get("parameters") else {
            return Err(BridgeError::InvalidSchema);
        };
        if !valid_parameters(parameters) {
            return Err(BridgeError::InvalidSchema);
        }
        validate_name(name, &mut seen)?;
    }
    Ok(())
}

fn authorize_tool<'a, 'b>(
    tools: &'a [ExtensionTool<'b>],
    context: CapabilityContext,
    name: &str,
) -> Result<&'a ExtensionTool<'b>, BridgeError> {
    validate_contract_and_effects(tools)?;
    let tool = tools
        .iter()
        .find(|tool| tool.descriptor.qualified_name() == name)
        .ok_or(BridgeError::UnknownTool)?;
    evaluate(&tool.descriptor, context, Some(tool.grant)).map_err(BridgeError::Denied)?;
    if !valid_parameters(&tool.parameters) {
        return Err(BridgeError::InvalidSchema);
    }
    Ok(tool)
}

fn validate_contract_and_effects(tools: &[ExtensionTool<'_>]) -> Result<(), BridgeError> {
    let descriptors = tools
        .iter()
        .map(|tool| tool.descriptor.clone())
        .collect::<Vec<_>>();
    validate_descriptors(&descriptors).map_err(BridgeError::InvalidContract)?;
    validate_names(tools.iter().map(|tool| tool.descriptor.qualified_name()))?;
    if !tools.iter().all(supported_effect) {
        return Err(BridgeError::UnsupportedEffect);
    }
    Ok(())
}

fn validate_names(names: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), BridgeError> {
    let mut seen = HashSet::new();
    for name in names {
        validate_name(name.as_ref(), &mut seen)?;
    }
    Ok(())
}

fn validate_name(name: &str, seen: &mut HashSet<String>) -> Result<(), BridgeError> {
    if name == "complete_task"
        || crate::tools::registry::workspace_operations()
            .iter()
            .any(|operation| operation.name() == name)
        || !seen.insert(name.to_owned())
    {
        return Err(BridgeError::NameCollision);
    }
    Ok(())
}

/// Check whether a parameter schema is in the subset enforced by dispatch.
pub(crate) fn valid_parameters(parameters: &Value) -> bool {
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
                .is_some_and(|value| value != &Value::Bool(false))
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
                Some(Value::Array(required)) => required.iter().all(|value| {
                    value
                        .as_str()
                        .is_some_and(|key| properties.contains_key(key))
                }),
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
                    .any(|key| !values.contains_key(key.as_str().unwrap_or("")))
            }) {
                return false;
            }
            values.iter().all(|(key, value)| {
                properties
                    .get(key)
                    .is_some_and(|subschema| validate_value(subschema, value))
            })
        }
        Some("array") => value.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| validate_value(&schema["items"], item))
        }),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("number") => value.is_number(),
        Some("null") => value.is_null(),
        _ => false,
    }
}

fn supported_effect(tool: &ExtensionTool<'_>) -> bool {
    matches!(
        tool.descriptor.effect,
        ExtensionEffect::ReadWorkspace | ExtensionEffect::MutateWorkspace
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::extensions::{
        CapabilityGrant, ExtensionAuthority, ExtensionDescriptor, EXTENSION_CONTRACT_VERSION,
    };
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn validation_requires_a_matching_grant_and_supported_schema() {
        let task_id = Uuid::new_v4();
        let grant = CapabilityGrant {
            task_id,
            namespace: "external".into(),
            operation: "read".into(),
            effect: ExtensionEffect::ReadWorkspace,
            generation: 1,
            host_approved: false,
        };
        let tool = ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read a path",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            grant: &grant,
        };
        assert_eq!(
            validate_authorized_tools(
                std::slice::from_ref(&tool),
                CapabilityContext {
                    task_id,
                    generation: 1,
                    task_authorized: true,
                }
            ),
            Ok(())
        );
        assert_eq!(
            authorize_extension_dispatch_with_arguments(
                std::slice::from_ref(&tool),
                CapabilityContext {
                    task_id,
                    generation: 1,
                    task_authorized: true,
                },
                "external_read",
                &json!({"path":"note.txt","extra":true})
            ),
            Err(BridgeError::InvalidArguments)
        );
    }
}
