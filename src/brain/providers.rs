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
use std::collections::HashMap;
use std::net::SocketAddr;
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

impl DecisionProtocol {
    pub(crate) const fn adapter_id(self) -> AdapterId {
        match self {
            Self::OllamaChat => AdapterId::OLLAMA,
            Self::GeminiGenerateContent => AdapterId::GEMINI,
            Self::Responses => AdapterId::RESPONSES,
            Self::AnthropicMessages => AdapterId::ANTHROPIC,
            Self::CompatibleChat => AdapterId::OPENAI_COMPATIBLE,
        }
    }
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

/// Stable identifier for one protocol adapter factory.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AdapterId(&'static str);

impl AdapterId {
    pub(crate) const OLLAMA: Self = Self::new("ollama-chat");
    pub(crate) const GEMINI: Self = Self::new("gemini-generate-content");
    pub(crate) const RESPONSES: Self = Self::new("openai-responses");
    pub(crate) const ANTHROPIC: Self = Self::new("anthropic-messages");
    pub(crate) const OPENAI_COMPATIBLE: Self = Self::new("openai-compatible-chat");

    pub(crate) const fn new(value: &'static str) -> Self {
        Self(value)
    }
}

/// Narrow capability hooks the CLI needs beyond the shared bounded decision contract.
pub(crate) trait ProviderDecisionSource: BoundedDecisionSource {
    fn set_repository_context(&mut self, _context: Value) -> Result<(), &'static str> {
        Err("--repository-context-json is supported by OpenAI-compatible providers")
    }

    fn supports_token_usage(&self) -> bool {
        false
    }

    fn take_token_usage(&mut self) -> Option<ModelTokenUsage> {
        None
    }

    fn ollama_source_mut(&mut self) -> Option<&mut OllamaDecisionSource> {
        None
    }

    fn set_anthropic_workspace_id(&mut self, _workspace_id: &str) -> Result<(), &'static str> {
        Err("ANTHROPIC_WORKSPACE_ID is supported by Anthropic")
    }
}

impl ProviderDecisionSource for OllamaDecisionSource {
    fn ollama_source_mut(&mut self) -> Option<&mut OllamaDecisionSource> {
        Some(self)
    }
}

impl ProviderDecisionSource for GeminiDecisionSource {}
impl ProviderDecisionSource for ResponsesDecisionSource {}

impl ProviderDecisionSource for AnthropicDecisionSource {
    fn set_anthropic_workspace_id(&mut self, workspace_id: &str) -> Result<(), &'static str> {
        self.set_workspace_id(workspace_id)
    }
}

impl ProviderDecisionSource for OpenAiCompatibleDecisionSource {
    fn set_repository_context(&mut self, context: Value) -> Result<(), &'static str> {
        OpenAiCompatibleDecisionSource::set_repository_context(self, context);
        Ok(())
    }

    fn supports_token_usage(&self) -> bool {
        true
    }

    fn take_token_usage(&mut self) -> Option<ModelTokenUsage> {
        OpenAiCompatibleDecisionSource::take_token_usage(self)
    }
}

/// Provider-specific secrets and account data resolved by the CLI.
///
/// This deliberately has no `Debug` implementation so credentials cannot be logged accidentally.
pub(crate) enum ProviderSourceCredentials {
    None,
    HostedApiKey(String),
    OpenAiCompatible(OpenAiCompatibleCredentials),
}

/// Inputs required to construct one bounded source without changing its wire protocol.
///
/// This deliberately has no `Debug` implementation because it may hold credentials.
pub(crate) struct ProviderSourceConfig {
    provider: ModelProvider,
    model: String,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
    credentials: ProviderSourceCredentials,
}

impl ProviderSourceConfig {
    pub(crate) fn new(
        provider: ModelProvider,
        model: String,
        action_budget: usize,
        ollama_endpoint: SocketAddr,
        credentials: ProviderSourceCredentials,
    ) -> Self {
        Self {
            provider,
            model,
            action_budget,
            ollama_endpoint,
            credentials,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderDecisionSourceRegistryError {
    DuplicateAdapter(AdapterId),
    MissingAdapter(AdapterId),
    BuildFailed(&'static str),
}

impl ProviderDecisionSourceRegistryError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::DuplicateAdapter(_) => "provider adapter is already registered",
            Self::MissingAdapter(_) => "provider adapter is not registered",
            Self::BuildFailed(message) => message,
        }
    }
}

pub(crate) type ProviderSourceFactory =
    fn(ProviderSourceConfig) -> Result<Box<dyn ProviderDecisionSource>, &'static str>;

/// Factory registry for the existing protocol-specific decision sources.
pub(crate) struct ProviderDecisionSourceRegistry {
    factories: HashMap<AdapterId, ProviderSourceFactory>,
}

impl ProviderDecisionSourceRegistry {
    pub(crate) fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub(crate) fn with_builtins() -> Self {
        let mut registry = Self::new();
        for (id, factory) in [
            (
                AdapterId::OLLAMA,
                ollama_source_factory as ProviderSourceFactory,
            ),
            (AdapterId::GEMINI, gemini_source_factory),
            (AdapterId::RESPONSES, responses_source_factory),
            (AdapterId::ANTHROPIC, anthropic_source_factory),
            (AdapterId::OPENAI_COMPATIBLE, compatible_source_factory),
        ] {
            registry
                .register(id, factory)
                .expect("built-in provider adapter IDs must be unique");
        }
        registry
    }

    pub(crate) fn register(
        &mut self,
        id: AdapterId,
        factory: ProviderSourceFactory,
    ) -> Result<(), ProviderDecisionSourceRegistryError> {
        if self.factories.contains_key(&id) {
            return Err(ProviderDecisionSourceRegistryError::DuplicateAdapter(id));
        }
        self.factories.insert(id, factory);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: AdapterId) -> bool {
        self.factories.contains_key(&id)
    }

    pub(crate) fn create(
        &self,
        config: ProviderSourceConfig,
    ) -> Result<ModelDecisionSource, ProviderDecisionSourceRegistryError> {
        let adapter_id = config.provider.capabilities().protocol.adapter_id();
        self.create_with_adapter(adapter_id, config)
    }

    pub(crate) fn create_with_adapter(
        &self,
        adapter_id: AdapterId,
        config: ProviderSourceConfig,
    ) -> Result<ModelDecisionSource, ProviderDecisionSourceRegistryError> {
        let factory = self.factories.get(&adapter_id).ok_or(
            ProviderDecisionSourceRegistryError::MissingAdapter(adapter_id),
        )?;
        let source = factory(config).map_err(ProviderDecisionSourceRegistryError::BuildFailed)?;
        Ok(ModelDecisionSource::from_adapter(adapter_id, source))
    }
}

fn ollama_source_factory(
    config: ProviderSourceConfig,
) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
    let ProviderSourceConfig {
        provider,
        model,
        action_budget,
        ollama_endpoint,
        credentials,
    } = config;
    if provider != ModelProvider::Ollama {
        return Err("Ollama adapter requires the Ollama provider");
    }
    if !matches!(credentials, ProviderSourceCredentials::None) {
        return Err("Ollama adapter does not accept hosted credentials");
    }
    Ok(Box::new(OllamaDecisionSource::with_endpoint(
        model,
        action_budget,
        ollama_endpoint,
    )))
}

fn gemini_source_factory(
    config: ProviderSourceConfig,
) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
    let ProviderSourceConfig {
        provider,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if provider != ModelProvider::Gemini {
        return Err("Gemini adapter requires the Gemini provider");
    }
    let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
        return Err("Gemini adapter requires hosted API key credentials");
    };
    Ok(Box::new(GeminiDecisionSource::new(
        model,
        api_key,
        action_budget,
    )?))
}

fn responses_source_factory(
    config: ProviderSourceConfig,
) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
    let ProviderSourceConfig {
        provider,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    let responses_provider = match provider {
        ModelProvider::OpenAi => ResponsesProvider::OpenAi,
        ModelProvider::Xai => ResponsesProvider::Xai,
        _ => return Err("Responses adapter requires OpenAI or xAI"),
    };
    let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
        return Err("Responses adapter requires hosted API key credentials");
    };
    Ok(Box::new(ResponsesDecisionSource::new(
        model,
        responses_provider,
        api_key,
        action_budget,
    )))
}

fn anthropic_source_factory(
    config: ProviderSourceConfig,
) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
    let ProviderSourceConfig {
        provider,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if provider != ModelProvider::Anthropic {
        return Err("Anthropic adapter requires the Anthropic provider");
    }
    let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
        return Err("Anthropic adapter requires hosted API key credentials");
    };
    Ok(Box::new(AnthropicDecisionSource::new(
        model,
        api_key,
        action_budget,
    )))
}

fn compatible_source_factory(
    config: ProviderSourceConfig,
) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
    let ProviderSourceConfig {
        provider,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    let ModelProvider::OpenAiCompatible(selected_provider) = provider else {
        return Err("compatible chat adapter requires an OpenAI-compatible provider");
    };
    let ProviderSourceCredentials::OpenAiCompatible(credentials) = credentials else {
        return Err("compatible chat adapter requires provider credentials");
    };
    if credentials.provider() != selected_provider {
        return Err("compatible provider credentials do not match the selected provider");
    }
    Ok(Box::new(OpenAiCompatibleDecisionSource::new(
        model,
        credentials,
        action_budget,
    )?))
}

/// Runtime facade retained for existing CLI and Harness call sites.
pub(crate) struct ModelDecisionSource {
    adapter_id: AdapterId,
    source: Box<dyn ProviderDecisionSource>,
}

impl ModelDecisionSource {
    fn from_adapter(adapter_id: AdapterId, source: Box<dyn ProviderDecisionSource>) -> Self {
        Self { adapter_id, source }
    }

    #[cfg(test)]
    pub(crate) fn from_source_for_test<S>(adapter_id: AdapterId, source: S) -> Self
    where
        S: ProviderDecisionSource + 'static,
    {
        Self::from_adapter(adapter_id, Box::new(source))
    }

    pub(crate) fn adapter_id(&self) -> AdapterId {
        self.adapter_id
    }

    pub(crate) fn set_repository_context(&mut self, context: Value) -> Result<(), &'static str> {
        self.source.set_repository_context(context)
    }

    pub(crate) fn supports_token_usage(&self) -> bool {
        self.source.supports_token_usage()
    }

    pub(crate) fn take_token_usage(&mut self) -> Option<ModelTokenUsage> {
        self.source.take_token_usage()
    }

    pub(crate) fn ollama_source_mut(&mut self) -> Option<&mut OllamaDecisionSource> {
        self.source.ollama_source_mut()
    }

    pub(crate) fn set_anthropic_workspace_id(
        &mut self,
        workspace_id: &str,
    ) -> Result<(), &'static str> {
        self.source.set_anthropic_workspace_id(workspace_id)
    }
}

impl BoundedDecisionSource for ModelDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.source.choose_action(goal, observations)
    }

    fn needs_root_project_guidance(&self) -> bool {
        self.source.needs_root_project_guidance()
    }

    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        guidance: Option<&str>,
    ) -> WorkspaceAction {
        self.source
            .choose_action_with_root_project_guidance(goal, observations, guidance)
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
