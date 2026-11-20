//! Request builders for every supported provider.

use crate::agent_loop::WorkspaceObservation;
use crate::harness::context::{
    build_plan_prompt_with_project_guidance, build_task_prompt_with_project_guidance,
};
use serde_json::{json, Value};

use super::errors::*;
use super::limits::*;
use super::providers;
use super::providers::*;
use super::tools_schema::*;

pub(crate) fn validate_model_name_for_provider(
    name: &str,
    provider: ModelProvider,
) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err("model name must not be empty");
    }
    if name.trim() != name {
        return Err("model name must not have leading or trailing whitespace");
    }
    if name.len() > MAX_MODEL_NAME_BYTES {
        return Err("model name must be at most 256 bytes");
    }
    if name.chars().any(char::is_control) {
        return Err("model name must not contain control characters");
    }
    if provider == ModelProvider::Gemini {
        GeminiModelName::try_from(name)?;
    }
    let lower_name = name.to_ascii_lowercase();
    if provider == ModelProvider::Ollama
        && (lower_name.ends_with(":cloud") || lower_name.ends_with("-cloud"))
    {
        return Err("cloud models are not supported; choose a locally installed model");
    }
    Ok(())
}

pub(crate) fn build_chat_request(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    validate_model_name_for_provider(model, ModelProvider::OpenAi)
        .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_project_guidance(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
    )?;
    let request = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": SYSTEM_INSTRUCTIONS },
            { "role": "user", "content": prompt }
        ],
        "tools": tool_definitions(),
        "stream": false
    });
    encode_model_request(&request)
}

pub(crate) fn build_plan_request_with_project_guidance(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    maximum_operations: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    build_plan_request_with_project_guidance_and_targets(
        model,
        goal,
        observations,
        maximum_operations,
        project_guidance,
        None,
    )
}

fn build_plan_request_with_project_guidance_and_targets(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    maximum_operations: usize,
    project_guidance: Option<&str>,
    proposed_targets: Option<&[PathBuf]>,
) -> Result<Vec<u8>, ModelError> {
    validate_model_name_for_provider(model, ModelProvider::Ollama)
        .map_err(ModelError::InvalidModelName)?;
    let prompt = match proposed_targets {
        Some(targets) => build_plan_prompt_with_project_guidance_and_targets(
            goal,
            observations,
            maximum_operations,
            project_guidance,
            targets,
        )?,
        None => build_plan_prompt_with_project_guidance(
            goal,
            observations,
            maximum_operations,
            project_guidance,
        )?,
    };
    let request = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": PLAN_SYSTEM_INSTRUCTIONS },
            { "role": "user", "content": prompt }
        ],
        "tools": workspace_tool_definitions(),
        "stream": false
    });
    encode_model_request(&request)
}

pub(crate) fn build_responses_request(
    model: &str,
    provider: ModelProvider,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        let plan = self.propose_plan_with_project_guidance(
            goal,
            observations,
            action_budget,
            project_guidance,
            None,
        )?;
        self.accept_plan_with_project_guidance(
            plan,
            action_budget,
            project_guidance.map(str::to_owned),
        )
    }

    pub(crate) fn propose_plan_with_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        action_budget: usize,
        project_guidance: Option<&str>,
        proposed_targets: Option<&[PathBuf]>,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    validate_model_name_for_provider(model, provider).map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_project_guidance(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
    )?;
    let request = json!({
        "model": model,
        "instructions": SYSTEM_INSTRUCTIONS,
        "input": prompt,
        "tools": openai_tool_definitions(),
        "tool_choice": "required",
        "parallel_tool_calls": false,
        "store": false,
        "stream": false
    });
    encode_model_request(&request)
}

pub(crate) fn build_anthropic_request(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    validate_model_name_for_provider(model, ModelProvider::Anthropic)
        .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_project_guidance(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
    )?;
    let request = json!({
        "model": model,
        "max_tokens": ANTHROPIC_MAX_OUTPUT_TOKENS,
        "system": SYSTEM_INSTRUCTIONS,
        "messages": [{ "role": "user", "content": prompt }],
        "tools": anthropic_tool_definitions(),
        "tool_choice": {"type":"auto", "disable_parallel_tool_use":true},
        "stream": false
    });
    encode_model_request(&request)
}

pub(crate) fn build_gemini_request(
    model: &str,
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
) -> Result<Vec<u8>, ModelError> {
    validate_model_name_for_provider(model, ModelProvider::Gemini)
        .map_err(ModelError::InvalidModelName)?;
    let prompt = build_task_prompt_with_project_guidance(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
    )?;
    let declarations = gemini_function_declarations();
    let allowed_function_names = declarations
        .iter()
        .filter_map(|declaration| declaration.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let request = json!({
        "systemInstruction": {"parts":[{"text":SYSTEM_INSTRUCTIONS}]},
        "contents": [{"role":"user","parts":[{"text":prompt}] }],
        "tools": [{"functionDeclarations":declarations}],
        "toolConfig": {
            "functionCallingConfig": {
                "mode":"ANY",
                "allowedFunctionNames":allowed_function_names
            }
        },
        "generationConfig": {"candidateCount":1}
    });
    encode_model_request(&request)
}

pub(crate) fn gemini_generate_content_endpoint(model: &str) -> Result<String, &'static str> {
    let model = GeminiModelName::try_from(model)?;
    Ok(format!(
        "{}/{}:generateContent",
        providers::GEMINI_GENERATE_CONTENT_URL,
        model.as_str()
    ))
}

pub(crate) fn encode_model_request(request: &Value) -> Result<Vec<u8>, ModelError> {
    let encoded = serde_json::to_vec(&request).map_err(|_| ModelError::RequestEncoding)?;
    if encoded.len() > MAX_MODEL_REQUEST_BYTES {
        return Err(ModelError::RequestTooLarge);
    }
    Ok(encoded)
}

#[allow(dead_code)]
pub(crate) fn enable_responses_streaming(request: &[u8]) -> Result<Vec<u8>, ModelError> {
    let mut body: Value = serde_json::from_slice(request)
        .map_err(|error| ModelError::Transport(error.to_string()))?;
    body["stream"] = Value::Bool(true);
    encode_model_request(&body)
}
