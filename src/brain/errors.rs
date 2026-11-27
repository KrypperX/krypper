//! Model error taxonomy and its formatting.

use crate::contracts::goal_turn::DecisionFailureCode;
use serde_json::Value;
use std::fmt;

use super::context::ContextError;
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
    /// The provider endpoint is not a safe `https` (or loopback `http`) origin.
    InvalidEndpoint,
    /// Prior observations exceeded the allowed context size.
    ObservationsTooLarge,
    /// Retrieved repository context exceeded its bounded prompt size.
    RepositoryContextTooLarge,
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

impl ModelError {
    /// Classifies a decision error without returning provider text or transport details.
    ///
    /// Unsupported tool calls are not failures here: Brain preserves them for the host-owned
    /// extension dispatch path.
    pub fn decision_failure_code(&self) -> Option<DecisionFailureCode> {
        Some(match self {
            Self::DecisionBudgetExhausted => DecisionFailureCode::DecisionBudgetExhausted,
            Self::InvalidModelName(_) => DecisionFailureCode::InvalidModelName,
            Self::RequestEncoding => DecisionFailureCode::RequestEncodingFailed,
            Self::RequestTooLarge => DecisionFailureCode::RequestTooLarge,
            Self::InvalidEndpoint => DecisionFailureCode::InvalidEndpoint,
            Self::ObservationsTooLarge | Self::RepositoryContextTooLarge => {
                DecisionFailureCode::ContextTooLarge
            }
            Self::ServiceUnavailable | Self::Transport(_) | Self::ProviderTransport(_) => {
                DecisionFailureCode::ProviderUnavailable
            }
            Self::ResponseTooLarge | Self::ProviderResponseTooLarge(_) => {
                DecisionFailureCode::ResponseTooLarge
            }
            Self::HttpStatus(status) | Self::ProviderHttpStatus(_, status) => match status {
                429 => DecisionFailureCode::ProviderRateLimited,
                408 | 425 | 500..=599 => DecisionFailureCode::ProviderUnavailable,
                _ => DecisionFailureCode::ProviderRejected,
            },
            Self::InvalidJson | Self::ProviderInvalidJson(_) => {
                DecisionFailureCode::InvalidProviderResponse
            }
            Self::InvalidToolCall(_) | Self::ProviderInvalidToolCall(_, _) => {
                DecisionFailureCode::InvalidModelOutput
            }
            Self::InvalidArguments(_) | Self::ProviderInvalidArguments(_, _) => {
                DecisionFailureCode::InvalidActionArguments
            }
            Self::UnsupportedTool { .. } | Self::ProviderUnsupportedTool { .. } => return None,
        })
    }
}

impl From<ContextError> for ModelError {
    fn from(error: ContextError) -> Self {
        match error {
            ContextError::RequestEncoding => Self::RequestEncoding,
            ContextError::ObservationsTooLarge => Self::ObservationsTooLarge,
            ContextError::RepositoryContextTooLarge => Self::RepositoryContextTooLarge,
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
            Self::InvalidEndpoint => formatter.write_str(
                "model endpoint must use https, or http on a loopback host such as 127.0.0.1 or localhost",
            ),
            Self::ObservationsTooLarge => {
                formatter.write_str("task observations exceeded the 2 MiB model context limit")
            }
            Self::RepositoryContextTooLarge => formatter.write_str(
                "retrieved repository context exceeded the 8 KiB model context limit",
            ),
            Self::ServiceUnavailable => write!(
                formatter,
                "local Ollama service at {OLLAMA_ADDRESS} is unavailable"
            ),
            Self::Transport(_) => formatter.write_str("Ollama request failed"),
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
