//! Model error taxonomy and its formatting.

use crate::harness::context::ContextError;
use std::fmt;

use super::limits::*;
use super::providers::*;

#[derive(Debug)]
pub(crate) enum ModelError {
    DecisionBudgetExhausted,
    InvalidModelName(&'static str),
    RequestEncoding,
    RequestTooLarge,
    ObservationsTooLarge,
    ServiceUnavailable,
    Transport(String),
    ResponseTooLarge,
    HttpStatus(u16),
    InvalidJson,
    InvalidToolCall(&'static str),
    UnsupportedTool,
    InvalidArguments(&'static str),
    ProviderTransport(ModelProvider),
    ProviderHttpStatus(ModelProvider, u16),
    ProviderResponseTooLarge(ModelProvider),
    ProviderInvalidJson(ModelProvider),
    ProviderInvalidToolCall(ModelProvider, &'static str),
    ProviderUnsupportedTool(ModelProvider),
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
            Self::UnsupportedTool => {
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
            Self::ProviderUnsupportedTool(provider) => {
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
