use super::*;
use crate::harness::context::build_task_prompt_with_repository_context_json;

/// Token counts returned by a provider response. Missing fields stay unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelTokenUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

pub(crate) struct CompatibleRequestContext<'a> {
    pub project_guidance: Option<&'a str>,
    pub extension_declarations: &'a [Value],
    pub repository_context: Option<&'a Value>,
}

/// A provider that uses Krypper's OpenAI-compatible chat protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiCompatibleProvider {
    /// Cloudflare Workers AI.
    Cloudflare,
    /// Cloudflare AI Gateway, including Workers AI and third-party catalog models.
    CloudflareAiGateway,
    /// Groq.
    Groq,
    /// OpenRouter.
    OpenRouter,
    /// DeepSeek.
    DeepSeek,
    /// Z.AI.
    Zai,
    /// Mistral.
    Mistral,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn endpoint(
        self,
        cloudflare_account_id: Option<&CloudflareAccountId>,
    ) -> Result<String, &'static str> {
        let descriptor = provider_descriptor(ModelProvider::OpenAiCompatible(self));
        match (descriptor.endpoint, cloudflare_account_id) {
            (ProviderEndpoint::CloudflareAccountPath { prefix, suffix, .. }, Some(account_id)) => {
                Ok(format!("{prefix}{}{suffix}", account_id.as_str()))
            }
            (ProviderEndpoint::CloudflareAccountPath { .. }, None) => {
                Err("CLOUDFLARE_ACCOUNT_ID is required for Cloudflare")
            }
            (ProviderEndpoint::Fixed(url), None) => Ok(url.to_owned()),
            (ProviderEndpoint::Fixed(_), Some(_)) => {
                Err("Cloudflare account ID is only valid for Cloudflare")
            }
            _ => Err("provider endpoint metadata is invalid"),
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
pub(crate) struct CloudflareGatewayId(String);

impl CloudflareGatewayId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for CloudflareGatewayId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(
                "CLOUDFLARE_AI_GATEWAY_ID must be 1-64 visible ASCII characters without spaces",
            );
        }
        Ok(Self(value.to_owned()))
    }
}

pub(crate) enum OpenAiCompatibleCredentials {
    Cloudflare {
        account_id: CloudflareAccountId,
        api_token: String,
    },
    CloudflareAiGateway {
        account_id: CloudflareAccountId,
        api_token: String,
        gateway_id: CloudflareGatewayId,
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

pub(crate) struct OpenAiCompatibleParts {
    pub(super) provider: OpenAiCompatibleProvider,
    pub(super) endpoint: String,
    pub(super) api_key: String,
    pub(super) extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatibleCredentials {
    pub(super) fn provider(&self) -> OpenAiCompatibleProvider {
        match self {
            Self::Cloudflare { .. } => OpenAiCompatibleProvider::Cloudflare,
            Self::CloudflareAiGateway { .. } => OpenAiCompatibleProvider::CloudflareAiGateway,
            Self::Groq { .. } => OpenAiCompatibleProvider::Groq,
            Self::OpenRouter { .. } => OpenAiCompatibleProvider::OpenRouter,
            Self::DeepSeek { .. } => OpenAiCompatibleProvider::DeepSeek,
            Self::Zai { .. } => OpenAiCompatibleProvider::Zai,
            Self::Mistral { .. } => OpenAiCompatibleProvider::Mistral,
        }
    }

    pub(super) fn into_parts(self) -> Result<OpenAiCompatibleParts, &'static str> {
        match self {
            Self::Cloudflare {
                account_id,
                api_token,
            } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::Cloudflare,
                endpoint: OpenAiCompatibleProvider::Cloudflare.endpoint(Some(&account_id))?,
                api_key: api_token,
                extra_headers: Vec::new(),
            }),
            Self::CloudflareAiGateway {
                account_id,
                api_token,
                gateway_id,
            } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::CloudflareAiGateway,
                endpoint: OpenAiCompatibleProvider::CloudflareAiGateway
                    .endpoint(Some(&account_id))?,
                api_key: api_token,
                extra_headers: vec![(
                    AuthFieldRole::CloudflareGatewayId
                        .request_header()
                        .expect("Cloudflare gateway ID has a request header")
                        .to_owned(),
                    gateway_id.as_str().to_owned(),
                )],
            }),
            Self::Groq { api_key } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::Groq,
                endpoint: OpenAiCompatibleProvider::Groq.endpoint(None)?,
                api_key,
                extra_headers: Vec::new(),
            }),
            Self::OpenRouter { api_key } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::OpenRouter,
                endpoint: OpenAiCompatibleProvider::OpenRouter.endpoint(None)?,
                api_key,
                extra_headers: Vec::new(),
            }),
            Self::DeepSeek { api_key } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::DeepSeek,
                endpoint: OpenAiCompatibleProvider::DeepSeek.endpoint(None)?,
                api_key,
                extra_headers: Vec::new(),
            }),
            Self::Zai { api_key } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::Zai,
                endpoint: OpenAiCompatibleProvider::Zai.endpoint(None)?,
                api_key,
                extra_headers: Vec::new(),
            }),
            Self::Mistral { api_key } => Ok(OpenAiCompatibleParts {
                provider: OpenAiCompatibleProvider::Mistral,
                endpoint: OpenAiCompatibleProvider::Mistral.endpoint(None)?,
                api_key,
                extra_headers: Vec::new(),
            }),
        }
    }
}

#[cfg(test)]
pub(crate) fn build_compatible_chat_request(
    model: &str,
    provider: OpenAiCompatibleProvider,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    build_compatible_chat_request_with_extensions(
        model,
        provider,
        goal,
        observations,
        remaining_decisions,
        project_guidance,
        &[],
    )
}

/// Build an OpenAI-compatible request with authorized extension declarations.
///
/// The declarations must come from `authorized_declarations`; conversion here
/// does not grant execution authority.
pub fn build_compatible_chat_request_with_extensions(
    model: &str,
    provider: OpenAiCompatibleProvider,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
    extension_declarations: &[Value],
) -> Result<Vec<u8>, ModelError> {
    build_compatible_chat_request_with_retrieval_context(
        model,
        provider,
        goal,
        observations,
        remaining_decisions,
        CompatibleRequestContext {
            project_guidance,
            extension_declarations,
            repository_context: None,
        },
    )
}

pub(crate) fn build_compatible_chat_request_with_retrieval_context(
    model: &str,
    provider: OpenAiCompatibleProvider,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    context: CompatibleRequestContext<'_>,
) -> Result<Vec<u8>, ModelError> {
    build_compatible_chat_request_with_retrieval_context_for(
        model,
        Some(provider),
        goal,
        observations,
        remaining_decisions,
        context,
    )
}

pub(crate) fn build_profile_compatible_chat_request(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    context: CompatibleRequestContext<'_>,
) -> Result<Vec<u8>, ModelError> {
    build_compatible_chat_request_with_retrieval_context_for(
        model,
        None,
        goal,
        observations,
        remaining_decisions,
        context,
    )
}

fn build_compatible_chat_request_with_retrieval_context_for(
    model: &str,
    provider: Option<OpenAiCompatibleProvider>,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    context: CompatibleRequestContext<'_>,
) -> Result<Vec<u8>, ModelError> {
    let CompatibleRequestContext {
        project_guidance,
        extension_declarations,
        repository_context,
    } = context;
    crate::brain::provider_bridge::validate_declarations(extension_declarations)
        .map_err(|_| ModelError::InvalidToolCall("invalid extension declarations"))?;
    match provider {
        Some(provider) => super::validate_model_name_for_provider(
            model,
            ModelProvider::OpenAiCompatible(provider),
        ),
        None => super::validate_model_name(model),
    }
    .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_repository_context_json(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
        repository_context,
    )?;
    let tool_choice = if matches!(
        provider,
        Some(
            OpenAiCompatibleProvider::DeepSeek
                | OpenAiCompatibleProvider::Zai
                | OpenAiCompatibleProvider::Mistral
        )
    ) {
        "auto"
    } else {
        "required"
    };
    let mut tools = tool_definitions();
    if let Value::Array(items) = &mut tools {
        items.extend(extension_declarations.iter().cloned());
    }
    let mut request = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": super::SYSTEM_INSTRUCTIONS },
            { "role": "user", "content": prompt }
        ],
        "tools": tools,
        "tool_choice": tool_choice,
        "parallel_tool_calls": false,
        "n": 1,
        "stream": false
    });
    if provider == Some(OpenAiCompatibleProvider::OpenRouter) {
        request["provider"] = json!({"allow_fallbacks":false});
    }
    encode_model_request(&request)
}

pub(crate) fn parse_compatible_chat_response(
    response: &[u8],
    provider: ModelProvider,
) -> Result<WorkspaceAction, ModelError> {
    parse_compatible_chat_response_for(response, Some(provider))
}

pub(crate) fn parse_profile_compatible_chat_response(
    response: &[u8],
) -> Result<WorkspaceAction, ModelError> {
    parse_compatible_chat_response_for(response, None)
}

fn compatible_invalid_json(provider: Option<ModelProvider>) -> ModelError {
    provider.map_or(ModelError::InvalidJson, ModelError::ProviderInvalidJson)
}

fn compatible_invalid_tool_call(
    provider: Option<ModelProvider>,
    message: &'static str,
) -> ModelError {
    provider.map_or(ModelError::InvalidToolCall(message), |provider| {
        ModelError::ProviderInvalidToolCall(provider, message)
    })
}

fn parse_compatible_chat_response_for(
    response: &[u8],
    provider: Option<ModelProvider>,
) -> Result<WorkspaceAction, ModelError> {
    let response: Value =
        serde_json::from_slice(response).map_err(|_| compatible_invalid_json(provider))?;
    let choices = response
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| compatible_invalid_tool_call(provider, "missing chat completion choices"))?;
    if choices.len() != 1 {
        return Err(compatible_invalid_tool_call(
            provider,
            "expected exactly one chat completion choice",
        ));
    }
    let choice = choices
        .first()
        .ok_or_else(|| compatible_invalid_tool_call(provider, "missing chat completion choice"))?;
    if choice.get("finish_reason").and_then(Value::as_str) != Some("tool_calls") {
        return Err(compatible_invalid_tool_call(
            provider,
            "response did not finish with a tool call",
        ));
    }
    let message = choice
        .get("message")
        .ok_or_else(|| compatible_invalid_tool_call(provider, "missing assistant message"))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(compatible_invalid_tool_call(
            provider,
            "tool calls must come from an assistant message",
        ));
    }
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .ok_or_else(|| compatible_invalid_tool_call(provider, "missing function tool calls"))?;
    if calls.len() != 1 {
        return Err(compatible_invalid_tool_call(
            provider,
            "expected exactly one tool call",
        ));
    }
    let call = calls
        .first()
        .ok_or_else(|| compatible_invalid_tool_call(provider, "expected exactly one tool call"))?;
    if call.get("type").and_then(Value::as_str) != Some("function") {
        return Err(compatible_invalid_tool_call(
            provider,
            "response used an unsupported tool call type",
        ));
    }
    let function = call
        .get("function")
        .ok_or_else(|| compatible_invalid_tool_call(provider, "missing function call details"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| compatible_invalid_tool_call(provider, "tool call has no function name"))?;
    let arguments = function.get("arguments").ok_or_else(|| {
        compatible_invalid_tool_call(provider, "tool call arguments are not JSON text")
    })?;
    let supports_object_arguments = matches!(
        provider,
        Some(ModelProvider::OpenAiCompatible(
            OpenAiCompatibleProvider::DeepSeek | OpenAiCompatibleProvider::Zai
        ))
    );
    let arguments = match arguments {
        Value::String(arguments) => serde_json::from_str(arguments).map_err(|_| {
            compatible_invalid_tool_call(provider, "tool arguments are invalid JSON")
        })?,
        Value::Object(arguments) if supports_object_arguments => Value::Object(arguments.clone()),
        _ => {
            return Err(compatible_invalid_tool_call(
                provider,
                "tool call arguments are not JSON text",
            ));
        }
    };
    match provider {
        Some(provider) => action_from_provider_call(provider, name, &arguments),
        None => super::action_from_call(name, &arguments),
    }
}

fn parse_model_token_usage(response: &[u8]) -> Option<ModelTokenUsage> {
    let response: Value = serde_json::from_slice(response).ok()?;
    let value = response.get("usage")?;
    let usage = ModelTokenUsage {
        prompt_tokens: value.get("prompt_tokens").and_then(Value::as_u64),
        completion_tokens: value.get("completion_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
    };
    (usage.prompt_tokens.is_some()
        || usage.completion_tokens.is_some()
        || usage.total_tokens.is_some())
    .then_some(usage)
}

/// Bounded OpenAI-compatible chat decision source with extensions.
pub struct OpenAiCompatibleDecisionSource {
    pub(super) core: HostedSourceCore,
    provider: Option<OpenAiCompatibleProvider>,
    extra_headers: Vec<(String, String)>,
    last_token_usage: Option<ModelTokenUsage>,
    repository_context: Option<Value>,
}

impl OpenAiCompatibleDecisionSource {
    pub(crate) fn new(
        model: impl Into<String>,
        credentials: OpenAiCompatibleCredentials,
        action_budget: usize,
    ) -> Result<Self, &'static str> {
        let OpenAiCompatibleParts {
            provider,
            endpoint,
            api_key,
            extra_headers,
        } = credentials.into_parts()?;
        Ok(Self::with_url_and_headers(
            model,
            provider,
            api_key,
            action_budget,
            endpoint,
            &[],
            &extra_headers,
        ))
    }

    pub(crate) fn with_profile_endpoint(
        model: impl Into<String>,
        api_key: String,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            core: HostedSourceCore::new(model, api_key, action_budget, endpoint, &[]),
            provider: None,
            extra_headers: Vec::new(),
            last_token_usage: None,
            repository_context: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, provider, api_key, action_budget, endpoint, &[])
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint_and_headers(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extra_headers: &[(String, String)],
    ) -> Self {
        Self::with_url_and_headers(
            model,
            provider,
            api_key,
            action_budget,
            endpoint,
            &[],
            extra_headers,
        )
    }

    /// Create a source that advertises host-authorized extension declarations.
    pub fn with_endpoint_and_extensions(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self::with_url(
            model,
            provider,
            api_key,
            action_budget,
            endpoint,
            extension_declarations,
        )
    }

    fn with_url(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self::with_url_and_headers(
            model,
            provider,
            api_key,
            action_budget,
            endpoint,
            extension_declarations,
            &[],
        )
    }

    fn with_url_and_headers(
        model: impl Into<String>,
        provider: OpenAiCompatibleProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
        extra_headers: &[(String, String)],
    ) -> Self {
        Self {
            core: HostedSourceCore::new(
                model,
                api_key,
                action_budget,
                endpoint,
                extension_declarations,
            ),
            provider: Some(provider),
            extra_headers: extra_headers.to_vec(),
            last_token_usage: None,
            repository_context: None,
        }
    }

    pub(crate) fn set_repository_context(&mut self, context: Value) {
        self.repository_context = Some(context);
    }

    pub(crate) fn take_token_usage(&mut self) -> Option<ModelTokenUsage> {
        self.last_token_usage.take()
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        self.last_token_usage = None;
        let remaining = self.core.consume_decision_budget()?;
        let request_context = CompatibleRequestContext {
            project_guidance,
            extension_declarations: &self.core.extension_declarations,
            repository_context: self.repository_context.as_ref(),
        };
        let request = match self.provider {
            Some(provider) => build_compatible_chat_request_with_retrieval_context(
                &self.core.model,
                provider,
                goal,
                observations,
                remaining,
                request_context,
            )?,
            None => build_profile_compatible_chat_request(
                &self.core.model,
                goal,
                observations,
                remaining,
                request_context,
            )?,
        };
        let response = match self.provider {
            Some(provider) => {
                let extra_headers = self
                    .extra_headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect::<Vec<_>>();
                post_hosted_with_headers(
                    &self.core.agent,
                    ModelProvider::OpenAiCompatible(provider),
                    &self.core.endpoint,
                    &self.core.api_key,
                    None,
                    &extra_headers,
                    &request,
                )?
            }
            None => post_profile_with_bearer(
                &self.core.agent,
                &self.core.endpoint,
                &self.core.api_key,
                &request,
            )?,
        };
        self.last_token_usage = parse_model_token_usage(&response);
        match self.provider {
            Some(provider) => {
                parse_compatible_chat_response(&response, ModelProvider::OpenAiCompatible(provider))
            }
            None => parse_profile_compatible_chat_response(&response),
        }
    }
}

super::impl_hosted_bounded_decision_source!(OpenAiCompatibleDecisionSource);
