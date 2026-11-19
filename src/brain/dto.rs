//! Deserialization types for Ollama chat responses.

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub(crate) struct OllamaChatResponse {
    pub(super) message: Option<OllamaMessage>,
}

#[derive(Deserialize)]
pub(crate) struct OllamaMessage {
    pub(super) role: OllamaMessageRole,
    #[serde(default)]
    pub(super) content: Option<String>,
    #[serde(default)]
    pub(super) tool_calls: Vec<OllamaToolCall>,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OllamaMessageRole {
    Assistant,
    User,
    System,
    Tool,
}

#[derive(Deserialize)]
pub(crate) struct OllamaToolCall {
    pub(super) function: OllamaFunctionCall,
}

#[derive(Deserialize)]
pub(crate) struct OllamaFunctionCall {
    pub(super) name: String,
    pub(super) arguments: Value,
}
