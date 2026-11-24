use super::limits::MAX_STREAMED_TOOL_ARGUMENT_BYTES;
use super::{
    action_from_error, action_from_provider_call, build_anthropic_request_with_extensions,
    build_gemini_request_with_extensions, build_responses_request_with_extensions,
    consume_decision_budget, enable_responses_streaming, encode_model_request,
    gemini_generate_content_endpoint, model_agent, parse_anthropic_response, parse_gemini_response,
    parse_responses_response, post_hosted, post_hosted_sse, post_hosted_sse_with_headers,
    post_hosted_with_headers, post_responses_text_stream, tool_definitions,
    validate_model_name_for_provider, ModelError, OllamaDecisionSource, SYSTEM_INSTRUCTIONS,
};
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};
use serde_json::{json, Value};
use ureq::Agent;

mod anthropic;
mod compatible;
mod gemini;
mod responses;

pub use anthropic::AnthropicDecisionSource;
#[cfg(test)]
pub(crate) use compatible::parse_compatible_chat_response;
#[cfg(test)]
pub(crate) use compatible::{build_compatible_chat_request, OpenAiCompatibleParts};
pub use compatible::{
    build_compatible_chat_request_with_extensions, ModelTokenUsage, OpenAiCompatibleDecisionSource,
    OpenAiCompatibleProvider,
};
#[cfg(test)]
pub(crate) use compatible::{
    build_compatible_chat_request_with_retrieval_context, CompatibleRequestContext,
};
pub(crate) use compatible::{
    CloudflareAccountId, CloudflareGatewayId, OpenAiCompatibleCredentials,
};
pub use gemini::GeminiDecisionSource;
pub(crate) use gemini::GeminiModelName;
pub use responses::{ResponsesDecisionSource, ResponsesProvider};

/// Only explicit opt-in values enable experimental native streaming.
fn stream_flag_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value.and_then(|value| value.to_str()), Some(value) if value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod stream_flag_tests {
    use super::stream_flag_enabled;
    use std::ffi::OsStr;

    #[test]
    fn streaming_requires_explicit_opt_in() {
        for value in ["", "0", "false", "FALSE", "yes", "on", "2", " true "] {
            assert!(!stream_flag_enabled(Some(OsStr::new(value))), "{value:?}");
        }
        assert!(!stream_flag_enabled(None));
        for value in ["1", "true", "TRUE", "TrUe"] {
            assert!(stream_flag_enabled(Some(OsStr::new(value))), "{value:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_flag_is_disabled() {
        use std::os::unix::ffi::OsStrExt;
        assert!(!stream_flag_enabled(Some(OsStr::from_bytes(b"true\xff"))));
    }
}

const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const XAI_RESPONSES_URL: &str = "https://api.x.ai/v1/responses";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
pub(super) const GEMINI_GENERATE_CONTENT_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models";
pub(super) const ANTHROPIC_API_VERSION: &str = "2026-10-21";
const ANTHROPIC_WORKSPACE_HEADER: &str = "anthropic-workspace-id";
const GROQ_CHAT_COMPLETIONS_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const OPENROUTER_CHAT_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEEPSEEK_CHAT_COMPLETIONS_URL: &str = "https://api.deepseek.com/chat/completions";
const ZAI_CHAT_COMPLETIONS_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const MISTRAL_CHAT_COMPLETIONS_URL: &str = "https://api.mistral.ai/v1/chat/completions";

/// The provider protocol selected for a model request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelProvider {
    /// Local Ollama chat protocol.
    Ollama,
    /// Gemini generate-content protocol.
    Gemini,
    /// OpenAI Responses protocol.
    OpenAi,
    /// xAI Responses protocol.
    Xai,
    /// Anthropic Messages protocol.
    Anthropic,
    /// An OpenAI-compatible chat-completions provider.
    OpenAiCompatible(OpenAiCompatibleProvider),
}

/// Provider protocol and feature support are independent of model names.
/// A provider can remain usable through its non-streaming decision path even
/// when no validated native SSE assembler is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecisionProtocol {
    OllamaChat,
    GeminiGenerateContent,
    Responses,
    AnthropicMessages,
    CompatibleChat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderCapabilities {
    pub(crate) protocol: DecisionProtocol,
    pub(crate) native_sse_decisions: bool,
}

impl ModelProvider {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "ollama" => Some(Self::Ollama),
            "gemini" => Some(Self::Gemini),
            "openai" => Some(Self::OpenAi),
            "xai" => Some(Self::Xai),
            "anthropic" => Some(Self::Anthropic),
            "cloudflare" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Cloudflare)),
            "cloudflare-ai-gateway" => Some(Self::OpenAiCompatible(
                OpenAiCompatibleProvider::CloudflareAiGateway,
            )),
            "groq" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Groq)),
            "openrouter" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::OpenRouter)),
            "deepseek" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::DeepSeek)),
            "zai" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Zai)),
            "mistral" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Mistral)),
            _ => None,
        }
    }

    pub(crate) fn capabilities(self) -> ProviderCapabilities {
        let protocol = match self {
            Self::Ollama => DecisionProtocol::OllamaChat,
            Self::Gemini => DecisionProtocol::GeminiGenerateContent,
            Self::OpenAi | Self::Xai => DecisionProtocol::Responses,
            Self::Anthropic => DecisionProtocol::AnthropicMessages,
            Self::OpenAiCompatible(_) => DecisionProtocol::CompatibleChat,
        };
        ProviderCapabilities {
            protocol,
            native_sse_decisions: matches!(
                protocol,
                DecisionProtocol::Responses | DecisionProtocol::AnthropicMessages
            ),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::Gemini => "Gemini",
            Self::OpenAi => "OpenAI",
            Self::Xai => "xAI",
            Self::Anthropic => "Anthropic",
            Self::OpenAiCompatible(provider) => provider.label(),
        }
    }
}

pub(crate) enum ModelDecisionSource {
    Ollama(OllamaDecisionSource),
    Gemini(GeminiDecisionSource),
    Responses(ResponsesDecisionSource),
    Anthropic(AnthropicDecisionSource),
    OpenAiCompatible(OpenAiCompatibleDecisionSource),
}

impl ModelDecisionSource {
    pub(crate) fn set_repository_context(&mut self, context: Value) -> Result<(), &'static str> {
        match self {
            Self::OpenAiCompatible(source) => {
                source.set_repository_context(context);
                Ok(())
            }
            _ => Err("--repository-context-json is supported by OpenAI-compatible providers"),
        }
    }

    pub(crate) fn supports_token_usage(&self) -> bool {
        matches!(self, Self::OpenAiCompatible(_))
    }

    pub(crate) fn take_token_usage(&mut self) -> Option<ModelTokenUsage> {
        match self {
            Self::OpenAiCompatible(source) => source.take_token_usage(),
            _ => None,
        }
    }
}

impl BoundedDecisionSource for ModelDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self {
            Self::Ollama(source) => source.choose_action(goal, observations),
            Self::Gemini(source) => source.choose_action(goal, observations),
            Self::Responses(source) => source.choose_action(goal, observations),
            Self::Anthropic(source) => source.choose_action(goal, observations),
            Self::OpenAiCompatible(source) => source.choose_action(goal, observations),
        }
    }

    fn needs_root_project_guidance(&self) -> bool {
        true
    }

    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        guidance: Option<&str>,
    ) -> WorkspaceAction {
        match self {
            Self::Ollama(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
            Self::Gemini(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
            Self::Responses(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
            Self::Anthropic(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
            Self::OpenAiCompatible(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
        }
    }
}

/// State and common budget handling shared by hosted decision sources.
struct HostedSourceCore {
    model: String,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
    extension_declarations: Vec<Value>,
}

impl HostedSourceCore {
    fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            endpoint,
            agent: model_agent(),
            remaining_decisions: action_budget,
            extension_declarations: extension_declarations.to_vec(),
        }
    }

    fn consume_decision_budget(&mut self) -> Result<usize, ModelError> {
        consume_decision_budget(&mut self.remaining_decisions)
    }
}

macro_rules! impl_hosted_bounded_decision_source {
    ($($source:ty),+ $(,)?) => {
        $(
        impl BoundedDecisionSource for $source {
            fn choose_action(
                &mut self,
                goal: &str,
                observations: &[WorkspaceObservation],
            ) -> WorkspaceAction {
                self.choose(goal, observations, None)
                    .unwrap_or_else(action_from_error)
            }

            fn needs_root_project_guidance(&self) -> bool {
                true
            }

            fn choose_action_with_root_project_guidance(
                &mut self,
                goal: &str,
                observations: &[WorkspaceObservation],
                guidance: Option<&str>,
            ) -> WorkspaceAction {
                self.choose(goal, observations, guidance)
                    .unwrap_or_else(action_from_error)
            }
        }
        )+
    };
}

pub(in crate::brain::providers) use impl_hosted_bounded_decision_source;

#[cfg(test)]
mod tests;
