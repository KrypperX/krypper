//! Model error taxonomy and its formatting.

use crate::harness::context::ContextError;
use serde_json::Value;
use std::fmt;

use super::limits::*;
use super::providers::*;

/// Errors produced while building, sending or interpreting a model request.
#[derive(Debug)]
pub enum ModelError {
    /// The bounded decision budget is exhausted.
    DecisionBudgetExhausted,
    /// The model name failed validation; the payload explains why.
    InvalidModelName(&'static str),
    /// The model request could not be encoded.
    RequestEncoding,
    /// The model request exceeded the allowed size.
    RequestTooLarge,
    /// Prior observations exceeded the allowed context size.
    ObservationsTooLarge,
    /// The local model service is unavailable.
    ServiceUnavailable,
    /// The local model request failed in transit.
    Transport(String),
    /// The local model response exceeded the allowed size.
    ResponseTooLarge,
    /// The local model service returned an error status.
    HttpStatus(u16),
    /// The local model response was not valid JSON.
    InvalidJson,
    /// The local model produced an invalid tool call.
    InvalidToolCall(&'static str),
    /// A local model tool name is not a built-in workspace action.
    UnsupportedTool {
        /// Requested tool name.
        name: String,
        /// Model-supplied JSON arguments.
        arguments: Value,
    },
    /// The local model supplied invalid arguments.
    InvalidArguments(&'static str),
    /// A hosted model request failed in transit.
    ProviderTransport(ModelProvider),
    /// A hosted model returned an error status.
    ProviderHttpStatus(ModelProvider, u16),
    /// A hosted model response exceeded the allowed size.
    ProviderResponseTooLarge(ModelProvider),
    /// A hosted model response was not valid JSON.
    ProviderInvalidJson(ModelProvider),
    /// A hosted model produced an invalid tool call.
    ProviderInvalidToolCall(ModelProvider, &'static str),
    /// A hosted model tool name is not a built-in workspace action.
    ProviderUnsupportedTool {
        /// Provider that produced the tool call.
        provider: ModelProvider,
        /// Requested tool name.
        name: String,
        /// Model-supplied JSON arguments.
        arguments: Value,
    },
    /// A hosted model supplied invalid arguments.
    ProviderInvalidArguments(ModelProvider, &'static str),
}

impl From<ContextError> for ModelError {
    fn from(error: ContextError) -> Self {
        match error {
            ContextError::RequestEncoding => Self::RequestEncoding,
            ContextError::ObservationsTooLarge => Self::ObservationsTooLarge,
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecisionBudgetExhausted => formatter.write_str("model decision budget exhausted"),
            Self::InvalidModelName(message) => formatter.write_str(message),
            Self::RequestEncoding => formatter.write_str("could not encode the model request"),
            Self::RequestTooLarge => formatter.write_str("model request exceeded 4 MiB"),
            Self::ObservationsTooLarge => {
                formatter.write_str("task observations exceeded the 2 MiB model context limit")
            }
            Self::ServiceUnavailable => write!(
                formatter,
                "local Ollama service at {OLLAMA_ADDRESS} is unavailable"
            ),
            Self::Transport(error) => write!(formatter, "Ollama request failed: {error}"),
            Self::ResponseTooLarge => formatter.write_str("Ollama response exceeded 2 MiB"),
            Self::HttpStatus(status) => write!(formatter, "Ollama returned HTTP {status}"),
            Self::InvalidJson => formatter.write_str("Ollama response was not valid JSON"),
            Self::InvalidToolCall(message) => {
                write!(formatter, "invalid Ollama tool call: {message}")
            }
            Self::UnsupportedTool { .. } => {
                formatter.write_str("Ollama requested an unsupported task action")
            }
            Self::InvalidArguments(message) => {
                write!(
                    formatter,
                    "Ollama tool call has invalid arguments: {message}"
                )
            }
            Self::ProviderTransport(provider) => {
                write!(formatter, "{} request failed", provider.label())
            }
            Self::ProviderHttpStatus(provider, status) => {
                write!(formatter, "{} returned HTTP {status}", provider.label())
            }
            Self::ProviderResponseTooLarge(provider) => {
                write!(formatter, "{} response exceeded 2 MiB", provider.label())
            }
            Self::ProviderInvalidJson(provider) => {
                write!(
                    formatter,
                    "{} response was not valid JSON",
                    provider.label()
                )
            }
            Self::ProviderInvalidToolCall(provider, message) => {
                write!(
                    formatter,
                    "invalid {} tool call: {message}",
                    provider.label()
                )
            }
            Self::ProviderUnsupportedTool { provider, .. } => {
                write!(
                    formatter,
                    "{} requested an unsupported task action",
                    provider.label()
                )
            }
            Self::ProviderInvalidArguments(provider, message) => write!(
                formatter,
                "{} tool call has invalid arguments: {message}",
                provider.label()
            ),
        }
    }
}
