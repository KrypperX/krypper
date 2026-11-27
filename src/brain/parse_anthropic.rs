//! Parsing of Anthropic Messages payloads.

use crate::contracts::goal_turn::WorkspaceAction;
use serde_json::Value;

use super::action::*;
use super::errors::*;
use super::providers::*;
use super::tool_set::AdvertisedToolNames;

pub(crate) fn parse_anthropic_response(
    response: &[u8],
    advertised_tools: &AdvertisedToolNames,
) -> Result<WorkspaceAction, ModelError> {
    let provider = ModelProvider::Anthropic;
    let response: Value =
        serde_json::from_slice(response).map_err(|_| ModelError::ProviderInvalidJson(provider))?;
    if response.get("stop_reason").and_then(Value::as_str) != Some("tool_use") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response did not request a tool use",
        ));
    }
    let content = response.get("content").and_then(Value::as_array).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "missing response content"),
    )?;
    let calls = content
        .iter()
        .filter(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with("tool_use"))
        })
        .collect::<Vec<_>>();
    if calls.len() != 1 {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "expected exactly one tool use",
        ));
    }
    let call = calls.first().ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "expected exactly one tool use",
    ))?;
    if call.get("type").and_then(Value::as_str) != Some("tool_use") {
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
                "tool use has no function name",
            ))?;
    let arguments = call
        .get("input")
        .ok_or(ModelError::ProviderInvalidToolCall(
            provider,
            "tool use has no input",
        ))?;
    action_from_provider_call(provider, name, arguments, advertised_tools)
}
