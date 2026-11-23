use super::*;

/// Provider families that use the OpenAI Responses protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesProvider {
    /// OpenAI Responses.
    OpenAi,
    /// xAI Responses.
    Xai,
}

impl ResponsesProvider {
    pub(super) fn model_provider(self) -> ModelProvider {
        match self {
            Self::OpenAi => ModelProvider::OpenAi,
            Self::Xai => ModelProvider::Xai,
        }
    }

    pub(in crate::brain) fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_RESPONSES_URL,
            Self::Xai => XAI_RESPONSES_URL,
        }
    }
}

/// Bounded OpenAI-compatible Responses decision source with extensions.
pub struct ResponsesDecisionSource {
    provider: ResponsesProvider,
    pub(super) core: HostedSourceCore,
}

impl ResponsesDecisionSource {
    /// Opt-in provider SSE text delivery. No action is selected or executed here.
    /// The normal bounded decision path remains authoritative.
    #[allow(dead_code)] // reserved for an opt-in text-only SSE caller; no CLI surface yet
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
        let request = build_responses_request_with_extensions(
            &self.core.model,
            provider,
            goal,
            observations,
            self.core.remaining_decisions,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        post_responses_text_stream(
            &self.core.agent,
            provider,
            &self.core.endpoint,
            &self.core.api_key,
            &request,
            on_text,
        )
    }

    /// Opt-in native SSE decision. Provider calls are assembled before the
    /// existing schema validator selects an action; no tool is executed here.
    /// This does not replace the default bounded decision path.
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
        let remaining = self.core.consume_decision_budget()?;
        let provider = self.provider.model_provider();
        let request = build_responses_request_with_extensions(
            &self.core.model,
            provider,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        let request = super::enable_responses_streaming(&request)?;
        let mut assembly =
            crate::streaming::ResponsesFinalActions::new(1, MAX_STREAMED_TOOL_ARGUMENT_BYTES);
        let mut final_calls = None;
        let mut transport_done = false;
        super::post_hosted_sse(
            &self.core.agent,
            provider,
            &self.core.endpoint,
            &self.core.api_key,
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
            &[],
        )
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        provider: ResponsesProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, provider, api_key, action_budget, endpoint, &[])
    }

    /// Create a source that advertises host-authorized extension declarations.
    pub fn with_endpoint_and_extensions(
        model: impl Into<String>,
        provider: ResponsesProvider,
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
        provider: ResponsesProvider,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
        extension_declarations: &[Value],
    ) -> Self {
        Self {
            provider,
            core: HostedSourceCore::new(
                model,
                api_key,
                action_budget,
                endpoint,
                extension_declarations,
            ),
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
            self.provider
                .model_provider()
                .capabilities()
                .native_sse_decisions
                && stream_flag_enabled(std::env::var_os("KRYPPER_RESPONSES_STREAM").as_deref()),
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
        let remaining = self.core.consume_decision_budget()?;
        let provider = self.provider.model_provider();
        let request = build_responses_request_with_extensions(
            &self.core.model,
            provider,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        let response = post_hosted(
            &self.core.agent,
            provider,
            &self.core.endpoint,
            &self.core.api_key,
            None,
            &request,
        )?;
        parse_responses_response(&response, provider)
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
}

super::impl_hosted_bounded_decision_source!(ResponsesDecisionSource);
