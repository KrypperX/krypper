//! Tool definitions and provider-specific schema adapters.

use crate::tools::registry::{workspace_operations, OperationDefinition};
use serde_json::{json, Value};

pub(crate) fn gemini_function_declarations() -> Vec<Value> {
    tool_definitions()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name":function.get("name")?,
                "description":function.get("description")?,
                "parameters":gemini_supported_schema(function.get("parameters")?)
            }))
        })
        .collect()
}

pub(crate) fn gemini_supported_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    adapt_gemini_schema(&mut schema);
    schema
}

pub(crate) fn adapt_gemini_schema(schema: &mut Value) {
    match schema {
        Value::Object(fields) => {
            if let Some(schema_type) = fields
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase)
            {
                fields.insert("type".to_owned(), Value::String(schema_type));
            }
            for unsupported in [
                "additionalProperties",
                "minimum",
                "maximum",
                "minLength",
                "maxLength",
            ] {
                fields.remove(unsupported);
            }
            fields.values_mut().for_each(adapt_gemini_schema);
        }
        Value::Array(values) => values.iter_mut().for_each(adapt_gemini_schema),
        _ => {}
    }
}

pub(crate) fn openai_tool_definitions() -> Value {
    let definitions = tool_definitions();
    let tools = definitions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "type":"function",
                "name":function.get("name")?,
                "description":function.get("description")?,
                "parameters":function.get("parameters")?,
                "strict":false
            }))
        })
        .collect::<Vec<_>>();
    Value::Array(tools)
}

pub(crate) fn anthropic_tool_definitions() -> Value {
    let definitions = tool_definitions();
    let tools = definitions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name":function.get("name")?,
                "description":function.get("description")?,
                "strict":true,
                "input_schema":anthropic_strict_schema(function.get("parameters")?)
            }))
        })
        .collect::<Vec<_>>();
    Value::Array(tools)
}

pub(crate) fn anthropic_strict_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    adapt_anthropic_strict_schema(&mut schema);
    schema
}

pub(crate) fn adapt_anthropic_strict_schema(schema: &mut Value) {
    match schema {
        Value::Object(fields) => {
            for unsupported in ["minimum", "maximum", "minLength", "maxLength"] {
                fields.remove(unsupported);
            }
            if fields.get("type").and_then(Value::as_str) == Some("object") {
                fields.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            fields.values_mut().for_each(adapt_anthropic_strict_schema);
        }
        Value::Array(values) => values.iter_mut().for_each(adapt_anthropic_strict_schema),
        _ => {}
    }
}

pub(crate) fn workspace_tool_definitions() -> Vec<Value> {
    workspace_operations()
        .iter()
        .map(operation_tool_definition)
        .collect::<Vec<_>>()
}

/// Plan and correction requests may contain only operations accepted by `WorkspacePlan`.
pub(crate) fn workspace_plan_tool_definitions() -> Vec<Value> {
    workspace_operations()
        .iter()
        .filter(|operation| operation.name() != "offline_read_only_exec")
        .map(operation_tool_definition)
        .collect()
}

fn operation_tool_definition(operation: &OperationDefinition) -> Value {
    let description = operation.provider_description();
    function_tool_with_parameters(operation.name(), &description, operation.parameters())
}

pub(crate) fn tool_definitions() -> Value {
    let mut tools = workspace_tool_definitions();
    tools.push(function_tool(
        "complete_task",
        "Finish the task with a concise result after its requested work is complete.",
        json!({"result":{"type":"string","minLength":1}}),
    ));
    Value::Array(tools)
}

pub(crate) fn function_tool(name: &str, description: &str, properties: Value) -> Value {
    let properties = properties.as_object().cloned().unwrap_or_default();
    let required = properties.keys().collect::<Vec<_>>();
    function_tool_with_parameters(
        name,
        description,
        json!({
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        }),
    )
}

pub(crate) fn function_tool_with_parameters(
    name: &str,
    description: &str,
    parameters: Value,
) -> Value {
    json!({
        "type":"function",
        "function": {
            "name":name,
            "description":description,
            "parameters":parameters
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{workspace_plan_tool_definitions, workspace_tool_definitions};

    fn has_direct_exec_tool(definitions: &[serde_json::Value]) -> bool {
        definitions.iter().any(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                == Some("offline_read_only_exec")
        })
    }

    #[test]
    fn direct_action_is_advertised_for_turns_but_not_for_unhandled_plans() {
        assert!(has_direct_exec_tool(&workspace_tool_definitions()));
        assert!(!has_direct_exec_tool(&workspace_plan_tool_definitions()));
    }
}
