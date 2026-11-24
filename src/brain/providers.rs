use super::limits::MAX_STREAMED_TOOL_ARGUMENT_BYTES;
use super::{
    action_from_call, action_from_error, action_from_provider_call,
    build_anthropic_request_with_extensions, build_gemini_request_with_extensions,
    build_responses_request_with_extensions, consume_decision_budget, enable_responses_streaming,
    encode_model_request, gemini_generate_content_endpoint, model_agent, parse_anthropic_response,
    parse_gemini_response, parse_responses_response, post_hosted, post_hosted_sse,
    post_hosted_sse_with_headers, post_hosted_with_headers, post_profile_with_bearer,
    post_responses_text_stream, tool_definitions, validate_endpoint, validate_model_name,
    validate_model_name_for_provider, AdvertisedToolNames, ModelError, OllamaDecisionSource,
    SYSTEM_INSTRUCTIONS,
};
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use ureq::Agent;

mod anthropic;
pub(crate) mod capability_evidence;
mod compatible;
mod gemini;
mod provider_catalog;
mod responses;

pub(crate) use provider_catalog::{
    all as provider_descriptors, by_provider as provider_descriptor,
    by_selector as provider_descriptor_by_selector, catalog_matches as provider_catalog_matches,
    is_credential_name as is_provider_credential_name, AuthFieldRole, ProviderAuthField,
    ProviderDescriptor, ProviderEndpoint,
};

pub use anthropic::AnthropicDecisionSource;
pub(crate) use anthropic::AnthropicWorkspaceId;
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

pub(super) const ANTHROPIC_API_VERSION: &str = "2026-10-21";

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
    pub(crate) fn adapter_id(self) -> AdapterId {
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
        provider_descriptor_by_selector(name).map(|descriptor| descriptor.provider)
    }

    pub(crate) fn capabilities(self) -> ProviderCapabilities {
        provider_catalog::capabilities(self)
    }

    pub(super) fn label(self) -> &'static str {
        provider_descriptor(self).label
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

    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Validated profile data passed from the per-user settings resolver to one
/// registered protocol adapter. It intentionally excludes the profile ID and
/// credentials so they cannot affect request routing or adapter behavior.
pub(crate) struct ProviderProfileSource {
    protocol: DecisionProtocol,
    endpoint: String,
}

impl ProviderProfileSource {
    pub(crate) fn new(protocol: DecisionProtocol, endpoint: String) -> Self {
        Self { protocol, endpoint }
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
        self.has_builtin_endpoint().then_some(self)
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
    provider: Option<ModelProvider>,
    profile: Option<ProviderProfileSource>,
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
            provider: Some(provider),
            profile: None,
            model,
            action_budget,
            ollama_endpoint,
            credentials,
        }
    }

    pub(crate) fn for_profile(
        protocol: DecisionProtocol,
        endpoint: String,
        model: String,
        action_budget: usize,
        ollama_endpoint: SocketAddr,
        credentials: ProviderSourceCredentials,
    ) -> Self {
        Self {
            provider: None,
            profile: Some(ProviderProfileSource::new(protocol, endpoint)),
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

    pub(crate) fn contains(&self, id: AdapterId) -> bool {
        self.factories.contains_key(&id)
    }

    pub(crate) fn create(
        &self,
        config: ProviderSourceConfig,
    ) -> Result<ModelDecisionSource, ProviderDecisionSourceRegistryError> {
        let adapter_id = match (&config.profile, config.provider) {
            (Some(profile), None) => profile.protocol.adapter_id(),
            (None, Some(provider)) => provider_descriptor(provider).adapter_id,
            _ => {
                return Err(ProviderDecisionSourceRegistryError::BuildFailed(
                    "provider source selection is invalid",
                ))
            }
        };
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
        profile,
        model,
        action_budget,
        ollama_endpoint,
        credentials,
    } = config;
    if let Some(profile) = profile {
        if profile.protocol != DecisionProtocol::OllamaChat {
            return Err("Ollama adapter received a different profile protocol");
        }
        if !matches!(credentials, ProviderSourceCredentials::None) {
            return Err("Ollama profile does not accept hosted credentials");
        }
        return Ok(Box::new(OllamaDecisionSource::with_profile_endpoint(
            model,
            action_budget,
            profile.endpoint,
        )));
    }
    if provider != Some(ModelProvider::Ollama) {
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
        profile,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if let Some(profile) = profile {
        if profile.protocol != DecisionProtocol::GeminiGenerateContent {
            return Err("Gemini adapter received a different profile protocol");
        }
        let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
            return Err("Gemini profile requires an API key");
        };
        return Ok(Box::new(GeminiDecisionSource::with_profile_endpoint(
            model,
            api_key,
            action_budget,
            profile.endpoint,
        )?));
    }
    if provider != Some(ModelProvider::Gemini) {
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
        profile,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if let Some(profile) = profile {
        if profile.protocol != DecisionProtocol::Responses {
            return Err("Responses adapter received a different profile protocol");
        }
        let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
            return Err("Responses profile requires an API key");
        };
        return Ok(Box::new(ResponsesDecisionSource::with_profile_endpoint(
            model,
            api_key,
            action_budget,
            profile.endpoint,
        )));
    }
    let responses_provider = match provider {
        Some(ModelProvider::OpenAi) => ResponsesProvider::OpenAi,
        Some(ModelProvider::Xai) => ResponsesProvider::Xai,
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
        profile,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if let Some(profile) = profile {
        if profile.protocol != DecisionProtocol::AnthropicMessages {
            return Err("Anthropic adapter received a different profile protocol");
        }
        let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
            return Err("Anthropic profile requires an API key");
        };
        return Ok(Box::new(AnthropicDecisionSource::with_profile_endpoint(
            model,
            api_key,
            action_budget,
            profile.endpoint,
        )));
    }
    if provider != Some(ModelProvider::Anthropic) {
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
        profile,
        model,
        action_budget,
        credentials,
        ..
    } = config;
    if let Some(profile) = profile {
        if profile.protocol != DecisionProtocol::CompatibleChat {
            return Err("compatible chat adapter received a different profile protocol");
        }
        let ProviderSourceCredentials::HostedApiKey(api_key) = credentials else {
            return Err("compatible profile requires an API key");
        };
        return Ok(Box::new(
            OpenAiCompatibleDecisionSource::with_profile_endpoint(
                model,
                api_key,
                action_budget,
                profile.endpoint,
            ),
        ));
    }
    let Some(ModelProvider::OpenAiCompatible(selected_provider)) = provider else {
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
