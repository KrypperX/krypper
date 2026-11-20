//! Parsing of native, plan and OpenAI Responses payloads.

use crate::agent_loop::WorkspaceAction;
use serde_json::Value;

use super::action::*;
use super::dto::*;
use super::errors::*;
use super::plan::*;
use super::providers::*;

pub(crate) fn parse_model_response(response: &[u8]) -> Result<WorkspaceAction, ModelError> {
    let response: OllamaChatResponse =
        serde_json::from_slice(response).map_err(|_| ModelError::InvalidJson)?;
    let message = response
        .message
        .ok_or(ModelError::InvalidToolCall("missing assistant message"))?;
    if message.role != OllamaMessageRole::Assistant {
        return Err(ModelError::InvalidToolCall(
            "tool calls must come from an assistant message",
        ));
    }
    let calls = message.tool_calls;
    if calls.len() != 1 {
        return Err(ModelError::InvalidToolCall(
            "expected exactly one tool call",
        ));
    }
    let call = calls.into_iter().next().ok_or(ModelError::InvalidToolCall(
        "expected exactly one tool call",
    ))?;
    action_from_call(&call.function.name, &call.function.arguments)
}

pub(crate) fn parse_model_plan_response(
    response: &[u8],
    maximum_operations: usize,
) -> Result<WorkspacePlan, WorkspacePlanError> {
    let response: OllamaChatResponse =
        serde_json::from_slice(response).map_err(|_| WorkspacePlanError::InvalidResponse)?;
    let message = response
        .message
        .ok_or(WorkspacePlanError::InvalidResponse)?;
    if message.role != OllamaMessageRole::Assistant {
        return Err(WorkspacePlanError::InvalidResponse);
    }
    if message.tool_calls.is_empty() {
        return Err(WorkspacePlanError::EmptyPlan);
    }
    if message.tool_calls.len() > maximum_operations {
        return Err(WorkspacePlanError::TooManyOperations {
            maximum: maximum_operations,
        });
    }

    let actions = message
        .tool_calls
        .into_iter()
        .map(|call| {
            action_from_call(&call.function.name, &call.function.arguments).map_err(|error| {
                match error {
                    ModelError::UnsupportedTool { .. } => WorkspacePlanError::UnsupportedOperation,
                    ModelError::InvalidArguments(_) => WorkspacePlanError::InvalidOperation,
                    _ => WorkspacePlanError::InvalidResponse,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    WorkspacePlan::try_from_actions(actions, maximum_operations)
}

pub(crate) fn parse_responses_response(
    response: &[u8],
    provider: ModelProvider,
) -> Result<WorkspaceAction, ModelError> {
    let response: Value =
        serde_json::from_slice(response).map_err(|_| ModelError::ProviderInvalidJson(provider))?;
    if response.get("choices").is_some() {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "Responses output must not contain chat completion choices",
        ));
    }
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response did not complete",
        ));
    }
    let output = response.get("output").and_then(Value::as_array).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "missing response output"),
    )?;
    if output.iter().any(|item| {
        item.get("type").and_then(Value::as_str).is_none_or(|kind| {
            !matches!(kind, "message" | "reasoning" | "function_call") && !kind.ends_with("_call")
        })
    }) {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response contains a malformed or unsupported output item",
        ));
    }
    let calls = output
        .iter()
        .filter(|item| {
            item.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with("_call"))
        })
        .collect::<Vec<_>>();
    if calls.len() != 1 {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "expected exactly one tool call",
        ));
    }
    let call = calls.first().ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "expected exactly one tool call",
    ))?;
    if call.get("type").and_then(Value::as_str) != Some("function_call") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response used an unsupported tool call type",
        ));
    }
    let name =
        call.get("name")
            .and_then(Value::as_str)
            .ok_or(ModelError::ProviderInvalidToolCall(
                provider,
                "tool call has no function name",
            ))?;
    let arguments = call.get("arguments").and_then(Value::as_str).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "tool call arguments are not JSON text"),
    )?;
    let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
        ModelError::ProviderInvalidToolCall(provider, "tool arguments are invalid JSON")
    })?;
    action_from_provider_call(provider, name, &arguments)
}
