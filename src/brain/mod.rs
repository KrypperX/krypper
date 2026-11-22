//! Model adapters: request building, transport, parsing and error mapping.
//!
//! Each submodule owns a single concern; this file is the facade that keeps the
//! existing `crate::brain::*` surface working.

mod action;
mod decision;
mod dto;
mod errors;
mod http;
mod limits;
mod parse;
mod parse_anthropic;
mod parse_gemini;
mod plan;
pub mod provider_bridge;
mod providers;
mod reflection;
mod request;
mod tools_schema;

#[cfg(test)]
mod tests;

pub use decision::OllamaDecisionSource;
pub use plan::{WorkspacePlan, WorkspacePlanError};
pub use reflection::BrainReflectionError;

pub(crate) use action::{action_from_error, action_from_provider_call};
pub(crate) use decision::consume_decision_budget;
pub(crate) use dto::{OllamaChatResponse, OllamaMessageRole};
pub use errors::ModelError;
pub(crate) use http::{
    model_agent, post_chat, post_hosted, post_hosted_sse, post_hosted_sse_with_headers,
    post_hosted_with_headers, post_responses_text_stream, validate_endpoint,
};
pub(crate) use limits::{OLLAMA_ADDRESS, SYSTEM_INSTRUCTIONS};
pub(crate) use parse::{parse_model_plan_response, parse_responses_response};
pub(crate) use parse_anthropic::parse_anthropic_response;
pub(crate) use parse_gemini::parse_gemini_response;
pub(crate) use plan::MAX_PLAN_OPERATIONS;
pub use providers::{
    build_compatible_chat_request_with_extensions, AnthropicDecisionSource, GeminiDecisionSource,
    ModelProvider, OpenAiCompatibleDecisionSource, OpenAiCompatibleProvider,
    ResponsesDecisionSource, ResponsesProvider,
};
pub(crate) use providers::{CloudflareAccountId, ModelDecisionSource, OpenAiCompatibleCredentials};
pub(crate) use request::{
    build_anthropic_request, build_gemini_request, build_responses_request,
    enable_responses_streaming, encode_model_request, gemini_generate_content_endpoint,
    validate_model_name_for_provider,
};
pub use request::{
    build_anthropic_request_with_extensions, build_chat_request_with_extensions,
    build_gemini_request_with_extensions, build_responses_request_with_extensions,
};
pub(crate) use tools_schema::{tool_definitions, workspace_tool_definitions};

#[cfg(test)]
pub(crate) use action::action_from_call;
#[cfg(test)]
pub(crate) use limits::MAX_MODEL_REQUEST_BYTES;
#[cfg(test)]
pub(crate) use parse::parse_model_response;
#[cfg(test)]
use providers::parse_compatible_chat_response;
#[cfg(test)]
pub(crate) use request::{build_chat_request, build_plan_request_with_project_guidance};
#[cfg(test)]
pub(crate) use tools_schema::{
    anthropic_strict_schema, anthropic_tool_definitions, gemini_function_declarations,
    gemini_supported_schema, openai_tool_definitions,
};
