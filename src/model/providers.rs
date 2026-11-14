use super::{
    action_from_provider_call, build_anthropic_request, build_openai_request, build_task_prompt,
    consume_decision_budget, encode_model_request, model_agent, parse_anthropic_response,
    parse_openai_response, post_hosted, tool_definitions, ModelError, OllamaDecisionSource,
};
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};
use serde_json::{json, Value};
use ureq::Agent;

const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
pub(super) const GEMINI_GENERATE_CONTENT_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models";
const ANTHROPIC_API_VERSION: &str = "2026-10-21";
const GROQ_CHAT_COMPLETIONS_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const OPENROUTER_CHAT_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEEPSEEK_CHAT_COMPLETIONS_URL: &str = "https://api.deepseek.com/chat/completions";
const ZAI_CHAT_COMPLETIONS_URL: &str = "https://api.z.ai/api/paas/v4/chat/completions";
const MISTRAL_CHAT_COMPLETIONS_URL: &str = "https://api.mistral.ai/v1/chat/completions";

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
) -> Result<Vec<u8>, ModelError> {
    super::validate_model_name_for_provider(model, ModelProvider::OpenAiCompatible(provider))
        .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt(goal, observations, remaining_decisions)?;
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
    Anthropic,
    OpenAiCompatible(OpenAiCompatibleProvider),
}

impl ModelProvider {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "ollama" => Some(Self::Ollama),
            "gemini" => Some(Self::Gemini),
            "openai" => Some(Self::OpenAi),
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
            Self::Anthropic => "Anthropic",
            Self::OpenAiCompatible(provider) => provider.label(),
        }
    }
}

pub(crate) enum ModelDecisionSource {
    Ollama(OllamaDecisionSource),
    Gemini(GeminiDecisionSource),
    OpenAi(OpenAiDecisionSource),
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
            Self::OpenAi(source) => source.choose_action(goal, observations),
            Self::Anthropic(source) => source.choose_action(goal, observations),
            Self::OpenAiCompatible(source) => source.choose_action(goal, observations),
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
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = super::consume_decision_budget(&mut self.remaining_decisions)?;
        let request = super::build_gemini_request(&self.model, goal, observations, remaining)?;
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
        self.choose(goal, observations)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

pub(crate) struct OpenAiDecisionSource {
    model: String,
    api_key: String,
    endpoint: String,
    agent: Agent,
    remaining_decisions: usize,
}

impl OpenAiDecisionSource {
    pub(crate) fn new(model: impl Into<String>, api_key: String, action_budget: usize) -> Self {
        Self::with_url(
            model,
            api_key,
            action_budget,
            OPENAI_RESPONSES_URL.to_owned(),
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

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request = build_openai_request(&self.model, goal, observations, remaining)?;
        let response = post_hosted(
            &self.agent,
            ModelProvider::OpenAi,
            &self.endpoint,
            &self.api_key,
            None,
            &request,
        )?;
        parse_openai_response(&response)
    }
}

impl BoundedDecisionSource for OpenAiDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.choose(goal, observations)
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

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request = build_anthropic_request(&self.model, goal, observations, remaining)?;
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
        self.choose(goal, observations)
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
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = consume_decision_budget(&mut self.remaining_decisions)?;
        let request = build_compatible_chat_request(
            &self.model,
            self.provider,
            goal,
            observations,
            remaining,
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
        self.choose(goal, observations)
            .unwrap_or_else(|error| WorkspaceAction::Unsupported {
                action: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenAiCompatibleCredentials, OpenAiCompatibleProvider};

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
