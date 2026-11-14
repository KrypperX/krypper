use super::{
    build_anthropic_request, build_openai_request, consume_decision_budget, model_agent,
    parse_anthropic_response, parse_openai_response, post_hosted, ModelError, OllamaDecisionSource,
};
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};
use ureq::Agent;

const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2026-10-21";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelProvider {
    Ollama,
    OpenAi,
    Anthropic,
}

impl ModelProvider {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "ollama" => Some(Self::Ollama),
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
        }
    }
}

pub(crate) enum ModelDecisionSource {
    Ollama(OllamaDecisionSource),
    OpenAi(OpenAiDecisionSource),
    Anthropic(AnthropicDecisionSource),
}

impl BoundedDecisionSource for ModelDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self {
            Self::Ollama(source) => source.choose_action(goal, observations),
            Self::OpenAi(source) => source.choose_action(goal, observations),
            Self::Anthropic(source) => source.choose_action(goal, observations),
        }
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
