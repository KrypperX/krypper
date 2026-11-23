use super::*;

/// Identifier sent in the `anthropic-workspace-id` request header.
///
/// The value is restricted to a conservative ASCII set so it can never
/// contain whitespace, control characters or header-injection sequences.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnthropicWorkspaceId(String);

impl AnthropicWorkspaceId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for AnthropicWorkspaceId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(
                "ANTHROPIC_WORKSPACE_ID must be 1-128 ASCII letters, digits, '-', '_' or '.'",
            );
        }
        Ok(Self(value.to_owned()))
    }
}

/// Bounded Anthropic decision source with optional extension declarations.
pub struct AnthropicDecisionSource {
    pub(super) core: HostedSourceCore,
    workspace_id: Option<AnthropicWorkspaceId>,
}

fn map_anthropic_assembly_error(error: crate::streaming::AnthropicAssemblyError) -> ModelError {
    match error {
        crate::streaming::AnthropicAssemblyError::InvalidJson => {
            ModelError::ProviderInvalidJson(ModelProvider::Anthropic)
        }
        crate::streaming::AnthropicAssemblyError::InvalidToolCall(message) => {
            ModelError::ProviderInvalidToolCall(ModelProvider::Anthropic, message)
        }
    }
}

impl AnthropicDecisionSource {
    pub(crate) fn new(model: impl Into<String>, api_key: String, action_budget: usize) -> Self {
        Self::with_url(
            model,
            api_key,
            action_budget,
            ANTHROPIC_MESSAGES_URL.to_owned(),
            &[],
        )
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, api_key, action_budget, endpoint, &[])
    }

    /// Create a source that advertises host-authorized extension declarations.
    pub fn with_endpoint_and_extensions(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self::with_url(
            model,
            api_key,
            action_budget,
            endpoint,
            extension_declarations,
        )
    }

    fn with_url(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self {
            core: HostedSourceCore::new(
                model,
                api_key,
                action_budget,
                endpoint,
                extension_declarations,
            ),
            workspace_id: None,
        }
    }

    /// Send the `anthropic-workspace-id` header with every request.
    ///
    /// Anthropic requires this header for API keys that are not scoped to a
    /// workspace; it is optional for workspace-scoped keys. Without this call
    /// no such header is sent. Returns an error for an empty, over-long or
    /// non-identifier value.
    pub fn with_workspace_id(mut self, workspace_id: &str) -> Result<Self, &'static str> {
        self.workspace_id = Some(AnthropicWorkspaceId::try_from(workspace_id)?);
        Ok(self)
    }

    fn workspace_headers(&self) -> Vec<(&'static str, &str)> {
        self.workspace_id
            .iter()
            .map(|id| (ANTHROPIC_WORKSPACE_HEADER, id.as_str()))
            .collect()
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

    #[cfg(test)]
    pub(crate) fn choose_with_streaming_for_test(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
        streaming: bool,
    ) -> Result<WorkspaceAction, ModelError> {
        self.choose_with_streaming(goal, observations, project_guidance, streaming)
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
        let remaining = self.core.consume_decision_budget()?;
        let request = build_anthropic_request_with_extensions(
            &self.core.model,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        let mut body: Value =
            serde_json::from_slice(&request).map_err(|_| ModelError::RequestEncoding)?;
        body["stream"] = Value::Bool(true);
        let request = encode_model_request(&body)?;
        let mut assembly =
            crate::streaming::AnthropicFinalActions::new(MAX_STREAMED_TOOL_ARGUMENT_BYTES);
        let workspace_headers = self.workspace_headers();
        super::post_hosted_sse_with_headers(
            &self.core.agent,
            ModelProvider::Anthropic,
            &self.core.endpoint,
            &self.core.api_key,
            &workspace_headers,
            &request,
            |frame| {
                let text = assembly
                    .push(&frame)
                    .map_err(map_anthropic_assembly_error)?;
                if let Some(text) = text {
                    on_text(text)?;
                }
                Ok(())
            },
        )?;
        let (name, arguments) = assembly.finish().map_err(map_anthropic_assembly_error)?;
        action_from_provider_call(ModelProvider::Anthropic, &name, &arguments)
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
            ModelProvider::Anthropic.capabilities().native_sse_decisions
                && stream_flag_enabled(std::env::var_os("KRYPPER_ANTHROPIC_STREAM").as_deref()),
        )
    }

    fn choose_nonstreaming(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = self.core.consume_decision_budget()?;
        let request = build_anthropic_request_with_extensions(
            &self.core.model,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        let workspace_headers = self.workspace_headers();
        let response = post_hosted_with_headers(
            &self.core.agent,
            ModelProvider::Anthropic,
            &self.core.endpoint,
            &self.core.api_key,
            Some(ANTHROPIC_API_VERSION),
            &workspace_headers,
            &request,
        )?;
        parse_anthropic_response(&response)
    }
}

super::impl_hosted_bounded_decision_source!(AnthropicDecisionSource);
