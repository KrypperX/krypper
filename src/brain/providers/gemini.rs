use super::*;

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

/// Bounded Gemini decision source with optional extension declarations.
pub struct GeminiDecisionSource {
    pub(super) core: HostedSourceCore,
}

impl GeminiDecisionSource {
    pub(crate) fn new(
        model: impl Into<String>,
        api_key: String,
        action_budget: usize,
    ) -> Result<Self, &'static str> {
        let model = model.into();
        let endpoint = super::gemini_generate_content_endpoint(&model)?;
        Ok(Self::with_url(model, api_key, action_budget, endpoint, &[]))
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        api_key: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self::with_url(model, api_key.into(), action_budget, endpoint, &[])
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
            api_key.into(),
            action_budget,
            endpoint,
            extension_declarations,
        )
    }

    fn with_url(
        model: impl Into<String>,
        api_key: String,
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
        }
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        let remaining = self.core.consume_decision_budget()?;
        let request = super::build_gemini_request_with_extensions(
            &self.core.model,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.core.extension_declarations,
        )?;
        let response = super::post_hosted(
            &self.core.agent,
            ModelProvider::Gemini,
            &self.core.endpoint,
            &self.core.api_key,
            None,
            &request,
        )?;
        super::parse_gemini_response(&response)
    }
}

super::impl_hosted_bounded_decision_source!(GeminiDecisionSource);
