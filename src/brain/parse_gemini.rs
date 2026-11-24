//! Parsing of Gemini generateContent payloads.

use crate::agent_loop::WorkspaceAction;
use serde_json::Value;

use super::action::*;
use super::errors::*;
use super::providers::*;
use super::tool_set::AdvertisedToolNames;

pub(crate) fn parse_gemini_response(
    response: &[u8],
    advertised_tools: &AdvertisedToolNames,
) -> Result<WorkspaceAction, ModelError> {
    let provider = ModelProvider::Gemini;
    let response: Value =
        serde_json::from_slice(response).map_err(|_| ModelError::ProviderInvalidJson(provider))?;
    let candidates = response.get("candidates").and_then(Value::as_array).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "missing response candidates"),
    )?;
    let mut calls = Vec::new();
    for candidate in candidates {
        if let Some(finish_reason) = candidate.get("finishReason") {
            match finish_reason.as_str() {
                Some("STOP") => {}
                Some("MALFORMED_FUNCTION_CALL") => {
                    return Err(ModelError::ProviderInvalidToolCall(
                        provider,
                        "provider reported a malformed function call",
                    ));
                }
                Some(_) => {
                    return Err(ModelError::ProviderInvalidToolCall(
                        provider,
                        "provider did not finish the response candidate normally",
                    ));
                }
                None => {
                    return Err(ModelError::ProviderInvalidToolCall(
                        provider,
                        "response candidate has a malformed finish reason",
                    ));
                }
            }
        }
        let content = candidate.get("content").and_then(Value::as_object).ok_or(
            ModelError::ProviderInvalidToolCall(
                provider,
                "response candidate has malformed content",
            ),
        )?;
        if let Some(role) = content.get("role") {
            if role.as_str() != Some("model") {
                return Err(ModelError::ProviderInvalidToolCall(
                    provider,
                    "response candidate has an unsupported content role",
                ));
            }
        }
        let parts = content.get("parts").and_then(Value::as_array).ok_or(
            ModelError::ProviderInvalidToolCall(
                provider,
                "response candidate has malformed content parts",
            ),
        )?;
        for part in parts {
            if !part.is_object() {
                return Err(ModelError::ProviderInvalidToolCall(
                    provider,
                    "response candidate has a malformed content part",
                ));
            }
            if let Some(call) = part.get("functionCall") {
                if !call.is_object() {
                    return Err(ModelError::ProviderInvalidToolCall(
                        provider,
                        "function call is malformed",
                    ));
                }
                calls.push(call);
            }
        }
    }
    if calls.len() != 1 {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "expected exactly one function call",
        ));
    }
    let call = calls.first().ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "expected exactly one function call",
    ))?;
    let name =
        call.get("name")
            .and_then(Value::as_str)
            .ok_or(ModelError::ProviderInvalidToolCall(
                provider,
                "function call has no name",
            ))?;
    let arguments = call.get("args").ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "function call has no arguments",
    ))?;
    action_from_provider_call(provider, name, arguments, advertised_tools)
}
