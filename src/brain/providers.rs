use crate::harness::context::build_task_prompt_with_project_guidance;

use super::{
    action_from_provider_call, build_anthropic_request, build_responses_request,
    consume_decision_budget, encode_model_request, model_agent, parse_anthropic_response,
    parse_responses_response, post_hosted, post_responses_text_stream, tool_definitions,
    ModelError, OllamaDecisionSource,
};
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};
use serde_json::{json, Value};
use ureq::Agent;

const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const XAI_RESPONSES_URL: &str = "https://api.x.ai/v1/responses";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
pub(super) const GEMINI_GENERATE_CONTENT_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models";
pub(super) const ANTHROPIC_API_VERSION: &str = "2026-10-21";
const GROQ_CHAT_COMPLETIONS_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const OPENROUTER_CHAT_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEEPSEEK_CHAT_COMPLETIONS_URL: &str = "https://api.deepseek.com/chat/completions";
const ZAI_CHAT_COMPLETIONS_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const MISTRAL_CHAT_COMPLETIONS_URL: &str = "https://api.mistral.ai/v1/chat/completions";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponsesProvider {
    OpenAi,
    Xai,
}

impl ResponsesProvider {
    fn model_provider(self) -> ModelProvider {
        match self {
            Self::OpenAi => ModelProvider::OpenAi,
            Self::Xai => ModelProvider::Xai,
        }
    }

    pub(super) fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_RESPONSES_URL,
            Self::Xai => XAI_RESPONSES_URL,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenAiCompatibleProvider {
    Cloudflare,
    Groq,
    OpenRouter,
    DeepSeek,
    Zai,
    Mistral,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn endpoint(
        self,
        cloudflare_account_id: Option<&CloudflareAccountId>,
    ) -> Result<String, &'static str> {
        match (self, cloudflare_account_id) {
            (Self::Cloudflare, Some(account_id)) => Ok(format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/chat/completions",
                account_id.as_str()
            )),
            (Self::Cloudflare, None) => Err("CLOUDFLARE_ACCOUNT_ID is required for Cloudflare"),
            (Self::Groq, None) => Ok(GROQ_CHAT_COMPLETIONS_URL.to_owned()),
            (Self::OpenRouter, None) => Ok(OPENROUTER_CHAT_COMPLETIONS_URL.to_owned()),
            (Self::DeepSeek, None) => Ok(DEEPSEEK_CHAT_COMPLETIONS_URL.to_owned()),
            (Self::Zai, None) => Ok(ZAI_CHAT_COMPLETIONS_URL.to_owned()),
            (Self::Mistral, None) => Ok(MISTRAL_CHAT_COMPLETIONS_URL.to_owned()),
            (
                Self::Groq | Self::OpenRouter | Self::DeepSeek | Self::Zai | Self::Mistral,
                Some(_),
            ) => Err("Cloudflare account ID is only valid for Cloudflare"),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare Workers AI",
            Self::Groq => "Groq",
            Self::OpenRouter => "OpenRouter",
            Self::DeepSeek => "DeepSeek",
            Self::Zai => "Z.AI",
            Self::Mistral => "Mistral",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CloudflareAccountId(String);

impl CloudflareAccountId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for CloudflareAccountId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 128
            || value == "."
            || value == ".."
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("CLOUDFLARE_ACCOUNT_ID must be one safe path component");
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GeminiModelName(String);

impl GeminiModelName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for GeminiModelName {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 256
            || matches!(value, "." | "..")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("Gemini model name must be one safe path component");
        }
        Ok(Self(value.to_owned()))
    }
}

pub(crate) enum OpenAiCompatibleCredentials {
    Cloudflare {
        account_id: CloudflareAccountId,
        api_token: String,
    },
    Groq {
        api_key: String,
    },
    OpenRouter {
        api_key: String,
    },
    DeepSeek {
        api_key: String,
    },
    Zai {
        api_key: String,
    },
    Mistral {
        api_key: String,
    },
}

impl OpenAiCompatibleCredentials {
    fn into_parts(self) -> Result<(OpenAiCompatibleProvider, String, String), &'static str> {
        match self {
            Self::Cloudflare {
                account_id,
                api_token,
            } => Ok((
                OpenAiCompatibleProvider::Cloudflare,
                OpenAiCompatibleProvider::Cloudflare.endpoint(Some(&account_id))?,
                api_token,
            )),
            Self::Groq { api_key } => Ok((
                OpenAiCompatibleProvider::Groq,
                OpenAiCompatibleProvider::Groq.endpoint(None)?,
                api_key,
            )),
            Self::OpenRouter { api_key } => Ok((
                OpenAiCompatibleProvider::OpenRouter,
                OpenAiCompatibleProvider::OpenRouter.endpoint(None)?,
                api_key,
            )),
            Self::DeepSeek { api_key } => Ok((
                OpenAiCompatibleProvider::DeepSeek,
                OpenAiCompatibleProvider::DeepSeek.endpoint(None)?,
                api_key,
            )),
            Self::Zai { api_key } => Ok((
                OpenAiCompatibleProvider::Zai,
                OpenAiCompatibleProvider::Zai.endpoint(None)?,
                api_key,
            )),
            Self::Mistral { api_key } => Ok((
                OpenAiCompatibleProvider::Mistral,
                OpenAiCompatibleProvider::Mistral.endpoint(None)?,
                api_key,
            )),
        }
    }
}

pub(crate) fn build_compatible_chat_request(
    model: &str,
    provider: OpenAiCompatibleProvider,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    super::validate_model_name_for_provider(model, ModelProvider::OpenAiCompatible(provider))
        .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_project_guidance(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
    )?;
    let tool_choice = if matches!(
        provider,
        OpenAiCompatibleProvider::DeepSeek
            | OpenAiCompatibleProvider::Zai
            | OpenAiCompatibleProvider::Mistral
    ) {
        "auto"
    } else {
        "required"
    };
    let mut request = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": super::SYSTEM_INSTRUCTIONS },
            { "role": "user", "content": prompt }
        ],
        "tools": tool_definitions(),
        "tool_choice": tool_choice,
        "parallel_tool_calls": false,
        "n": 1,
        "stream": false
    });
    if provider == OpenAiCompatibleProvider::OpenRouter {
        request["provider"] = json!({"allow_fallbacks":false});
    }
    encode_model_request(&request)
}

pub(super) fn parse_compatible_chat_response(
    response: &[u8],
    provider: ModelProvider,
) -> Result<WorkspaceAction, ModelError> {
    let response: Value =
        serde_json::from_slice(response).map_err(|_| ModelError::ProviderInvalidJson(provider))?;
    let choices = response.get("choices").and_then(Value::as_array).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "missing chat completion choices"),
    )?;
    if choices.len() != 1 {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "expected exactly one chat completion choice",
        ));
    }
    let choice = choices.first().ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "missing chat completion choice",
    ))?;
    if choice.get("finish_reason").and_then(Value::as_str) != Some("tool_calls") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response did not finish with a tool call",
        ));
    }
    let message = choice
        .get("message")
        .ok_or(ModelError::ProviderInvalidToolCall(
            provider,
            "missing assistant message",
        ))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "tool calls must come from an assistant message",
        ));
    }
    let calls = message.get("tool_calls").and_then(Value::as_array).ok_or(
        ModelError::ProviderInvalidToolCall(provider, "missing function tool calls"),
    )?;
    if calls.len() != 1 {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "expected exactly one tool call",
        ));
    }
    let call = calls.first().ok_or(ModelError::ProviderInvalidToolCall(
        provider,
        "expected exactly one tool call",
    ))?;
    if call.get("type").and_then(Value::as_str) != Some("function") {
        return Err(ModelError::ProviderInvalidToolCall(
            provider,
            "response used an unsupported tool call type",
        ));
    }
    let function = call
        .get("function")
        .ok_or(ModelError::ProviderInvalidToolCall(
            provider,
            "missing function call details",
        ))?;
    let name =
        function
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ModelError::ProviderInvalidToolCall(
                provider,
                "tool call has no function name",
            ))?;
    let arguments = function
        .get("arguments")
        .ok_or(ModelError::ProviderInvalidToolCall(
            provider,
            "tool call arguments are not JSON text",
        ))?;
    let supports_object_arguments = matches!(
        provider,
        ModelProvider::OpenAiCompatible(
            OpenAiCompatibleProvider::DeepSeek | OpenAiCompatibleProvider::Zai
        )
    );
    let arguments = match arguments {
        Value::String(arguments) => serde_json::from_str(arguments).map_err(|_| {
            ModelError::ProviderInvalidToolCall(provider, "tool arguments are invalid JSON")
        })?,
        Value::Object(arguments) if supports_object_arguments => Value::Object(arguments.clone()),
        _ => {
            return Err(ModelError::ProviderInvalidToolCall(
                provider,
                "tool call arguments are not JSON text",
            ));
        }
    };
    action_from_provider_call(provider, name, &arguments)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelProvider {
    Ollama,
    Gemini,
    OpenAi,
    Xai,
    Anthropic,
    OpenAiCompatible(OpenAiCompatibleProvider),
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
            "groq" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Groq)),
            "openrouter" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::OpenRouter)),
            "deepseek" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::DeepSeek)),
            "zai" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Zai)),
            "mistral" => Some(Self::OpenAiCompatible(OpenAiCompatibleProvider::Mistral)),
            _ => None,
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

pub(crate) struct GeminiDecisionSource {
    model: String,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
}

impl GeminiDecisionSource {
    pub(crate) fn new(
        model: impl Into<String>,
        api_key: String,
        action_budget: usize,
    ) -> Result<Self, &'static str> {
        let model = model.into();
        let endpoint = super::gemini_generate_content_endpoint(&model)?;
        Ok(Self::with_url(model, api_key, action_budget, endpoint))
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, api_key.into(), action_budget, endpoint)
    }

    fn with_url(
        model: impl Into<String>,
        api_key: String,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            model: model.into(),
            api_key,
            endpoint,
            agent: super::model_agent(),
            remaining_decisions: action_budget,
        }
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = super::consume_decision_budget(&mut self.remaining_decisions)?;
        let request = super::build_gemini_request(
            &self.model,
            goal,
            observations,
            remaining,
            project_guidance,
        )?;
        let response = super::post_hosted(
            &self.agent,
            ModelProvider::Gemini,
            &self.endpoint,
            &self.api_key,
            None,
            &request,
        )?;
        super::parse_gemini_response(&response)
    }
}

impl BoundedDecisionSource for GeminiDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.choose(goal, observations, None)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
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
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

pub(crate) struct ResponsesDecisionSource {
    provider: ResponsesProvider,
    model: String,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
}

impl ResponsesDecisionSource {
    /// Opt-in provider SSE text delivery. No action is selected or executed here.
    /// The normal bounded decision path remains authoritative.
    #[allow(dead_code)]
    pub(crate) fn stream_text_deltas<F>(
        &self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        on_text: F,
    ) -> Result<(), ModelError>
    where
        F: FnMut(String) -> Result<(), ModelError>,
    {
        let provider = self.provider.model_provider();
        let request = build_responses_request(
            &self.model,
            provider,
            goal,
            observations,
            self.remaining_decisions,
            project_guidance,
        )?;
        post_responses_text_stream(
            &self.agent,
            provider,
            &self.endpoint,
            &self.api_key,
            &request,
            on_text,
        )
    }

    /// Opt-in native SSE decision. Provider calls are assembled before the
    /// existing schema validator selects an action; no tool is executed here.
    /// This does not replace the default bounded decision path.
    #[allow(dead_code)]
    pub(crate) fn stream_validated_action<F>(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        mut on_text: F,
    ) -> Result<WorkspaceAction, ModelError>
    where
        F: FnMut(String) -> Result<(), ModelError>,
    {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let provider = self.provider.model_provider();
        let request = build_responses_request(
            &self.model,
            provider,
            goal,
            observations,
            remaining,
            project_guidance,
        )?;
        let request = super::enable_responses_streaming(&request)?;
        let mut assembly = crate::streaming::ResponsesFinalActions::new(1, 256 * 1024);
        let mut final_calls = None;
        let mut transport_done = false;
        super::post_hosted_sse(
            &self.agent,
            provider,
            &self.endpoint,
            &self.api_key,
            &request,
            |frame| {
                // The optional transport sentinel is only valid after the
                // response has completed; it must never hide truncation.
                if frame.data.trim() == "[DONE]" {
                    if final_calls.is_none() || transport_done {
                        return Err(ModelError::ProviderInvalidToolCall(
                            provider,
                            "unexpected streaming transport sentinel",
                        ));
                    }
                    transport_done = true;
                    return Ok(());
                }
                // A transport sentinel is terminal. Reject any subsequent
                // event, including an additional sentinel or text delta.
                // Never forward text after response completion either.
                if transport_done || final_calls.is_some() {
                    return Err(ModelError::ProviderInvalidToolCall(
                        provider,
                        "event received after response completion",
                    ));
                }
                let completed_calls = assembly.consume(&frame).map_err(|_| {
                    ModelError::ProviderInvalidToolCall(provider, "invalid streaming tool call")
                })?;
                let text = crate::streaming::provider_text_delta(&frame);
                if frame.event.as_deref() == Some("response.output_text.delta") && text.is_none() {
                    return Err(ModelError::ProviderInvalidJson(provider));
                }
                if let Some(text) = text {
                    on_text(text)?;
                }
                if let Some(calls) = completed_calls {
                    final_calls = Some(calls);
                }
                Ok(())
            },
        )?;
        let calls = final_calls.ok_or(ModelError::ProviderInvalidToolCall(
            provider,
            "stream ended before response completion",
        ))?;
        if calls.len() != 1 {
            return Err(ModelError::ProviderInvalidToolCall(
                provider,
                "expected exactly one tool call",
            ));
        }
        let (_, name, arguments) =
            calls
                .into_iter()
                .next()
                .ok_or(ModelError::ProviderInvalidToolCall(
                    provider,
                    "missing tool call",
                ))?;
        action_from_provider_call(provider, &name, &arguments)
    }

    pub(crate) fn new(
        model: impl Into<String>,
        provider: ResponsesProvider,
        api_key: String,
        action_budget: usize,
    ) -> Self {
        Self::with_url(
            model,
            provider,
            api_key,
            action_budget,
            provider.endpoint().to_owned(),
        )
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(
        model: impl Into<String>,
        provider: ResponsesProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, provider, api_key, action_budget, endpoint)
    }

    fn with_url(
        model: impl Into<String>,
        provider: ResponsesProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            api_key: api_key.into(),
            endpoint,
            agent: model_agent(),
            remaining_decisions: action_budget,
        }
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        self.choose_with_streaming(
            goal,
            observations,
            project_guidance,
            std::env::var_os("KRYPPER_RESPONSES_STREAM").is_some(),
        )
    }

    fn choose_with_streaming(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        streaming: bool,
    ) -> Result<WorkspaceAction, ModelError> {
        if streaming {
            return self.stream_validated_action(goal, observations, project_guidance, |_| Ok(()));
        }
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let provider = self.provider.model_provider();
        let request = build_responses_request(
            &self.model,
            provider,
            goal,
            observations,
            remaining,
            project_guidance,
        )?;
        let response = post_hosted(
            &self.agent,
            provider,
            &self.endpoint,
            &self.api_key,
            None,
            &request,
        )?;
        parse_responses_response(&response, provider)
    }
}

impl BoundedDecisionSource for ResponsesDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.choose(goal, observations, None)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
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
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

pub(crate) struct AnthropicDecisionSource {
    model: String,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
}

impl AnthropicDecisionSource {
    pub(crate) fn new(model: impl Into<String>, api_key: String, action_budget: usize) -> Self {
        Self::with_url(
            model,
            api_key,
            action_budget,
            ANTHROPIC_MESSAGES_URL.to_owned(),
        )
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, api_key, action_budget, endpoint)
    }

    fn with_url(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            endpoint,
            agent: model_agent(),
            remaining_decisions: action_budget,
        }
    }

    fn choose_with_streaming(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        streaming: bool,
    ) -> Result<WorkspaceAction, ModelError> {
        if streaming {
            return self.stream_validated_action(goal, observations, project_guidance);
        }
        self.choose_nonstreaming(goal, observations, project_guidance)
    }

    fn stream_validated_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        self.stream_validated_action_with_text(goal, observations, project_guidance, |_| Ok(()))
    }

    /// Deliver only provider text deltas. Tool arguments stay inside the
    /// bounded action assembler and are never forwarded as display text.
    pub(crate) fn stream_validated_action_with_text<F>(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        mut on_text: F,
    ) -> Result<WorkspaceAction, ModelError>
    where
        F: FnMut(String) -> Result<(), ModelError>,
    {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request =
            build_anthropic_request(&self.model, goal, observations, remaining, project_guidance)?;
        let mut body: Value =
            serde_json::from_slice(&request).map_err(|_| ModelError::RequestEncoding)?;
        body["stream"] = Value::Bool(true);
        let request = encode_model_request(&body)?;
        let mut blocks = std::collections::BTreeMap::<usize, (String, String)>::new();
        let mut completed_blocks = std::collections::BTreeSet::<usize>::new();
        let mut completed = false;
        let mut tool_use_stop = false;
        super::post_hosted_sse(
            &self.agent,
            ModelProvider::Anthropic,
            &self.endpoint,
            &self.api_key,
            &request,
            |frame| {
                let provider = ModelProvider::Anthropic;
                let invalid = || {
                    ModelError::ProviderInvalidToolCall(
                        provider,
                        "invalid Anthropic streaming tool call",
                    )
                };
                // Never expose text from an invalid or post-completion frame.
                if completed {
                    return Err(invalid());
                }
                let event: Value = serde_json::from_str(&frame.data)
                    .map_err(|_| ModelError::ProviderInvalidJson(provider))?;
                // Anthropic repeats the event kind in the JSON payload. Reject
                // contradictory framing before consuming arguments or text.
                if frame.event.as_deref() != event["type"].as_str() {
                    return Err(invalid());
                }
                // Once the provider has declared tool_use as the stop reason,
                // no more content blocks or deltas may change the final action.
                if tool_use_stop
                    && matches!(
                        frame.event.as_deref(),
                        Some("content_block_start" | "content_block_delta" | "content_block_stop")
                    )
                {
                    return Err(invalid());
                }
                match frame.event.as_deref() {
                    Some("content_block_start") => {
                        let index = usize::try_from(event["index"].as_u64().ok_or_else(invalid)?)
                            .map_err(|_| invalid())?;
                        let block = &event["content_block"];
                        if block["type"] == "tool_use" {
                            let name = block["name"].as_str().ok_or_else(invalid)?;
                            // A decision selects exactly one action. Reject a
                            // second tool before buffering its arguments.
                            if !blocks.is_empty() || completed_blocks.contains(&index) {
                                return Err(invalid());
                            }
                            blocks.insert(index, (name.to_owned(), String::new()));
                        }
                    }
                    Some("content_block_delta") => {
                        if event["delta"]["type"] == "input_json_delta" {
                            let index =
                                usize::try_from(event["index"].as_u64().ok_or_else(invalid)?)
                                    .map_err(|_| invalid())?;
                            let chunk = event["delta"]["partial_json"]
                                .as_str()
                                .ok_or_else(invalid)?;
                            let (_, arguments) = blocks.get_mut(&index).ok_or_else(invalid)?;
                            if completed_blocks.contains(&index)
                                || arguments.len().saturating_add(chunk.len()) > 256 * 1024
                            {
                                return Err(invalid());
                            }
                            arguments.push_str(chunk);
                        }
                    }
                    Some("content_block_stop") => {
                        let index = usize::try_from(event["index"].as_u64().ok_or_else(invalid)?)
                            .map_err(|_| invalid())?;
                        if blocks.contains_key(&index) && !completed_blocks.insert(index) {
                            return Err(invalid());
                        }
                    }
                    Some("message_delta") => {
                        if tool_use_stop
                            || event["delta"]["stop_reason"] != "tool_use"
                            || blocks.len() != 1
                            || completed_blocks.len() != 1
                        {
                            return Err(invalid());
                        }
                        tool_use_stop = true;
                    }
                    Some("message_stop") => {
                        if !tool_use_stop || blocks.len() != 1 || completed_blocks.len() != 1 {
                            return Err(invalid());
                        }
                        completed = true;
                    }
                    Some("error") => return Err(invalid()),
                    _ => {}
                }
                // Display text is never sourced from tool arguments.
                let text = crate::streaming::provider_text_delta(&frame);
                if frame.event.as_deref() == Some("content_block_delta")
                    && event["delta"]["type"] == "text_delta"
                    && text.is_none()
                {
                    return Err(ModelError::ProviderInvalidJson(provider));
                }
                if let Some(text) = text {
                    on_text(text)?;
                }
                Ok(())
            },
        )?;
        if !completed || !tool_use_stop || blocks.len() != 1 || completed_blocks.len() != 1 {
            return Err(ModelError::ProviderInvalidToolCall(
                ModelProvider::Anthropic,
                "stream ended without one complete tool call",
            ));
        }
        let (_, (name, arguments)) =
            blocks
                .into_iter()
                .next()
                .ok_or(ModelError::ProviderInvalidToolCall(
                    ModelProvider::Anthropic,
                    "missing tool call",
                ))?;
        let args: Value = serde_json::from_str(&arguments)
            .map_err(|_| ModelError::ProviderInvalidJson(ModelProvider::Anthropic))?;
        action_from_provider_call(ModelProvider::Anthropic, &name, &args)
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        self.choose_with_streaming(
            goal,
            observations,
            project_guidance,
            std::env::var_os("KRYPPER_ANTHROPIC_STREAM").is_some(),
        )
    }

    fn choose_nonstreaming(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request =
            build_anthropic_request(&self.model, goal, observations, remaining, project_guidance)?;
        let response = post_hosted(
            &self.agent,
            ModelProvider::Anthropic,
            &self.endpoint,
            &self.api_key,
            Some(ANTHROPIC_API_VERSION),
            &request,
        )?;
        parse_anthropic_response(&response)
    }
}

impl BoundedDecisionSource for AnthropicDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.choose(goal, observations, None)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
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
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

pub(crate) struct OpenAiCompatibleDecisionSource {
    model: String,
    provider: OpenAiCompatibleProvider,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
}

impl OpenAiCompatibleDecisionSource {
    pub(crate) fn new(
        model: impl Into<String>,
        credentials: OpenAiCompatibleCredentials,
        action_budget: usize,
    ) -> Result<Self, &'static str> {
        let (provider, endpoint, api_key) = credentials.into_parts()?;
        Ok(Self::with_url(
            model,
            provider,
            api_key,
            action_budget,
            endpoint,
        ))
    }

    #[cfg(test)]
    pub(super) fn with_endpoint(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, provider, api_key, action_budget, endpoint)
    }

    fn with_url(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            model: model.into(),
            provider,
            api_key: api_key.into(),
            endpoint,
            agent: model_agent(),
            remaining_decisions: action_budget,
        }
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request = build_compatible_chat_request(
            &self.model,
            self.provider,
            goal,
            observations,
            remaining,
            project_guidance,
        )?;
        let provider = ModelProvider::OpenAiCompatible(self.provider);
        let response = post_hosted(
            &self.agent,
            provider,
            &self.endpoint,
            &self.api_key,
            None,
            &request,
        )?;
        parse_compatible_chat_response(&response, provider)
    }
}

impl BoundedDecisionSource for OpenAiCompatibleDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.choose(goal, observations, None)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
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
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiCompatibleCredentials, OpenAiCompatibleProvider};

    /// Exercise the real HTTP/SSE transport, assembly and action validator
    /// without contacting a provider or executing the returned workspace action.
    fn assert_responses_stream_validates_action(provider: super::ResponsesProvider) {
        use super::ResponsesDecisionSource;
        use crate::agent_loop::WorkspaceAction;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let mut request = [0_u8; 16384];
            let count = socket.read(&mut request).expect("read request");
            let received = String::from_utf8_lossy(&request[..count]);
            assert!(received.contains("POST "));
            let frames = [
                (
                    "response.output_item.added",
                    serde_json::json!({
                        "output_index": 0,
                        "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
                    }),
                ),
                (
                    "response.function_call_arguments.delta",
                    serde_json::json!({
                        "output_index": 0, "delta": r#"{"path":"README.md"}"#
                    }),
                ),
                (
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "output_index": 0
                    }),
                ),
                (
                    "response.output_item.done",
                    serde_json::json!({
                        "output_index": 0,
                        "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
                    }),
                ),
                (
                    "response.completed",
                    serde_json::json!({
                        "response": {"status": "completed"}
                    }),
                ),
            ];
            let mut body = String::new();
            for (event, data) in frames {
                body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).expect("send SSE");
        });

        let mut source = ResponsesDecisionSource::with_endpoint(
            "test-model",
            provider,
            "local-test-key",
            2,
            endpoint,
        );
        let action = source
            .choose_with_streaming("read README", &[], None, true)
            .expect("completed stream produces validated action");
        assert!(matches!(action, WorkspaceAction::ReadFile { .. }));
        server.join().expect("server completed");
    }

    #[test]
    fn anthropic_text_delta_excludes_tool_argument_chunks() {
        use crate::streaming::{provider_text_delta, SseFrame};
        let text = SseFrame {
            event: Some("content_block_delta".into()),
            data: serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hello"}
            })
            .to_string(),
        };
        let tool = SseFrame {
            event: Some("content_block_delta".into()),
            data: serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "secret"}
            })
            .to_string(),
        };
        assert_eq!(provider_text_delta(&text).as_deref(), Some("hello"));
        assert_eq!(provider_text_delta(&tool), None);
    }

    #[test]
    fn openai_responses_stream_validates_completed_action() {
        assert_responses_stream_validates_action(super::ResponsesProvider::OpenAi);
    }

    #[test]
    fn xai_responses_stream_validates_completed_action() {
        assert_responses_stream_validates_action(super::ResponsesProvider::Xai);
    }

    /// A transport that closes before response.completed must never yield an action.
    fn assert_responses_stream_rejects_truncated_tool_call(provider: super::ResponsesProvider) {
        use super::ResponsesDecisionSource;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept request");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set timeout");
            let mut request = [0_u8; 16384];
            socket.read(&mut request).expect("read request");
            let body = format!(
                "event: response.output_item.added\ndata: {}\n\n",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
                })
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .expect("send partial SSE");
        });
        let mut source = ResponsesDecisionSource::with_endpoint(
            "test-model",
            provider,
            "local-test-key",
            2,
            endpoint,
        );
        assert!(source
            .choose_with_streaming("read README", &[], None, true)
            .is_err());
        server.join().expect("server completed");
    }

    #[test]
    fn openai_responses_stream_rejects_truncated_tool_call() {
        assert_responses_stream_rejects_truncated_tool_call(super::ResponsesProvider::OpenAi);
    }

    #[test]
    fn xai_responses_stream_rejects_truncated_tool_call() {
        assert_responses_stream_rejects_truncated_tool_call(super::ResponsesProvider::Xai);
    }

    fn assert_anthropic_stream_decision(
        truncated: bool,
        duplicate_stop: bool,
        mismatched_type: bool,
    ) {
        use super::AnthropicDecisionSource;
        use crate::agent_loop::WorkspaceAction;
        use serde_json::json;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut request = [0_u8; 16384];
            let count = socket.read(&mut request).expect("request");
            let received = String::from_utf8_lossy(&request[..count]);
            assert!(received.contains("x-api-key:") || received.contains("X-Api-Key:"));
            let frames = [
                (
                    "content_block_start",
                    json!({"index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"read_file","input":{}}}),
                ),
                (
                    "content_block_delta",
                    json!({"index":0,"delta":{"type":"input_json_delta","partial_json":r#"{"path":"#}}),
                ),
                (
                    "content_block_delta",
                    json!({"index":0,"delta":{"type":"input_json_delta","partial_json":r#""README.md"}"#}}),
                ),
                ("content_block_stop", json!({"index":0})),
                ("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
                ("message_stop", json!({})),
            ];
            let mut body = String::new();
            for (kind, data) in frames {
                if truncated && kind == "message_stop" {
                    break;
                }
                let mut data = data;
                data["type"] = serde_json::Value::String(
                    if mismatched_type && kind == "content_block_delta" {
                        "message_delta"
                    } else {
                        kind
                    }
                    .to_owned(),
                );
                body.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
                if duplicate_stop && kind == "message_delta" {
                    body.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).expect("response");
        });
        let mut source =
            AnthropicDecisionSource::with_endpoint("test-model", "local-test-key", 2, endpoint);
        let result = source.choose_with_streaming("read README", &[], None, true);
        if truncated || duplicate_stop || mismatched_type {
            assert!(result.is_err(), "invalid stream must not yield an action");
        } else {
            let streamed = result.expect("valid Anthropic SSE decision");
            assert!(matches!(streamed, WorkspaceAction::ReadFile { .. }));
            let response = json!({
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use",
                    "id": "tool_1",
                    "name": "read_file",
                    "input": {"path": "README.md"}
                }]
            });
            let nonstreamed = super::parse_anthropic_response(response.to_string().as_bytes())
                .expect("equivalent non-streaming decision");
            assert_eq!(format!("{streamed:?}"), format!("{nonstreamed:?}"));
        }
        server.join().expect("server");
    }

    #[test]
    fn anthropic_stream_assembles_and_validates_completed_tool_use() {
        assert_anthropic_stream_decision(false, false, false);
    }

    #[test]
    fn anthropic_truncated_stream_never_yields_an_action() {
        assert_anthropic_stream_decision(true, false, false);
    }

    #[test]
    fn anthropic_duplicate_stop_reason_never_yields_an_action() {
        assert_anthropic_stream_decision(false, true, false);
    }

    #[test]
    fn anthropic_mismatched_event_type_never_yields_an_action() {
        assert_anthropic_stream_decision(false, false, true);
    }

    #[test]
    fn mistral_credentials_bind_to_the_fixed_endpoint() {
        let key = "local-mistral-test-key";
        let (provider, endpoint, api_key) = OpenAiCompatibleCredentials::Mistral {
            api_key: key.to_owned(),
        }
        .into_parts()
        .expect("Mistral credentials use a fixed endpoint");

        assert_eq!(provider, OpenAiCompatibleProvider::Mistral);
        assert_eq!(endpoint, "https://api.mistral.ai/v1/chat/completions");
        assert_eq!(api_key, key);
    }
}
