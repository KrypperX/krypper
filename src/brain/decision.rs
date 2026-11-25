//! Ollama decision source and its bounded decision budget.

use crate::agent_loop::{
    BoundedDecisionSource, GoalTurnResult, WorkspaceAction, WorkspaceObservation,
};
use crate::harness::context::RepositoryContext;
use crate::harness::execution::{
    CompletionCorrectionError, CompletionCorrectionResult, ProcessFailureCorrectionError,
    ProcessFailureCorrectionResult,
};
use crate::harness::impact::ImpactReport;
use crate::harness::verification::{
    CompletionCheckEvidence, CompletionVerification, RequiredCompletionCheck,
};
use crate::task_lifecycle::TaskSession;
use crate::ProcessResult;
use std::net::SocketAddr;
use std::path::PathBuf;
use ureq::Agent;

use super::action::action_from_error;
use super::errors::*;
use super::http::*;
use super::limits::*;
use super::parse::*;
use super::plan::*;
use super::reflection;
use super::reflection::*;
use super::request::*;
use super::tool_set::AdvertisedToolNames;

fn encode_plan_for_impact_review(plan: &WorkspacePlan) -> Result<String, WorkspacePlanError> {
    let operations = plan
        .operations()
        .iter()
        .map(|action| {
            let path_text = |path: &PathBuf| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or(WorkspacePlanError::UnsupportedOperation)
            };
            let (name, arguments) = match action {
                WorkspaceAction::OfflineCargoTest { test_filter } => (
                    "offline_cargo_test",
                    serde_json::json!({ "test_filter": test_filter }),
                ),
                WorkspaceAction::ReadFile { path } => (
                    "read_file",
                    serde_json::json!({ "path": path_text(path)? }),
                ),
                WorkspaceAction::ReadRange { path, start_line, max_lines } => (
                    "read_range",
                    serde_json::json!({ "path": path_text(path)?, "start_line": start_line, "max_lines": max_lines }),
                ),
                WorkspaceAction::ListDirectory { path } => (
                    "list_directory",
                    serde_json::json!({ "path": path_text(path)? }),
                ),
                WorkspaceAction::CreateDirectory { path } => (
                    "create_directory",
                    serde_json::json!({ "path": path_text(path)? }),
                ),
                WorkspaceAction::SearchText { directory, query } => (
                    "search_text",
                    serde_json::json!({ "directory": path_text(directory)?, "query": query }),
                ),
                WorkspaceAction::SearchTreeText { directory, query } => (
                    "search_tree_text",
                    serde_json::json!({ "directory": path_text(directory)?, "query": query }),
                ),
                WorkspaceAction::CreateFile { path, contents } => (
                    "create_file",
                    serde_json::json!({ "path": path_text(path)?, "contents": contents }),
                ),
                WorkspaceAction::ReplaceText { path, match_text, replacement_text } => (
                    "replace_text",
                    serde_json::json!({ "path": path_text(path)?, "match_text": match_text, "replacement_text": replacement_text }),
                ),
                WorkspaceAction::ApplyPatch { path, patch } => (
                    "apply_patch",
                    serde_json::json!({ "path": path_text(path)?, "patch": patch }),
                ),
                _ => return Err(WorkspacePlanError::UnsupportedOperation),
            };
            Ok(serde_json::json!({ "tool": name, "arguments": arguments }))
        })
        .collect::<Result<Vec<_>, WorkspacePlanError>>()?;
    serde_json::to_string(&operations).map_err(|_| WorkspacePlanError::ModelRequestFailed)
}

pub struct OllamaDecisionSource {
    pub(super) model: String,
    pub(super) address: SocketAddr,
    profile_endpoint: Option<String>,
    streaming_enabled: bool,
    pub(super) agent: Agent,
    pub(super) remaining_decisions: usize,
    pub(super) extension_declarations: Vec<serde_json::Value>,
    pub(super) repository_context: Option<RepositoryContext>,
}

impl OllamaDecisionSource {
    /// Creates an opt-in decision source for a locally served Ollama model.
    ///
    /// Model names are validated before requests are sent, and each returned tool call and its
    /// arguments are validated before a workspace action is made. No request is sent until this
    /// source is asked to choose an action.
    /// `action_budget` should match the runner's action limit so the model sees the remaining
    /// number of decisions available to the turn.
    pub fn new(model: impl Into<String>, action_budget: usize) -> Self {
        Self::with_endpoint_and_extensions(model, action_budget, OLLAMA_ADDRESS, &[])
    }

    /// Create a source that advertises host-authorized extension declarations.
    pub fn with_endpoint_and_extensions(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
        extension_declarations: &[serde_json::Value],
    ) -> Self {
        let streaming_enabled = super::providers::stream_flag_enabled(
            std::env::var_os("KRYPPER_OLLAMA_STREAM").as_deref(),
        );
        Self::with_endpoint_and_extensions_streaming(
            model,
            action_budget,
            address,
            extension_declarations,
            streaming_enabled,
        )
    }

    fn with_endpoint_and_extensions_streaming(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
        extension_declarations: &[serde_json::Value],
        streaming_enabled: bool,
    ) -> Self {
        Self {
            model: model.into(),
            address,
            profile_endpoint: None,
            streaming_enabled,
            agent: model_agent(),
            remaining_decisions: action_budget,
            extension_declarations: extension_declarations.to_vec(),
            repository_context: None,
        }
    }

    pub(crate) fn with_profile_endpoint(
        model: impl Into<String>,
        action_budget: usize,
        endpoint: String,
    ) -> Self {
        Self {
            model: model.into(),
            address: OLLAMA_ADDRESS,
            profile_endpoint: Some(endpoint),
            streaming_enabled: false,
            agent: hosted_model_agent(),
            remaining_decisions: action_budget,
            extension_declarations: Vec::new(),
            repository_context: None,
        }
    }

    pub(crate) fn set_repository_context(&mut self, context: RepositoryContext) {
        self.repository_context = Some(context);
    }

    pub(crate) fn clear_repository_context(&mut self) {
        self.repository_context = None;
    }

    pub(crate) fn has_builtin_endpoint(&self) -> bool {
        self.profile_endpoint.is_none()
    }

    /// Requests one bounded, ordered set of workspace operations from the local model.
    ///
    /// One action-budget slot is reserved for the existing task-control decision path. The
    /// returned plan contains only registry operations and does not declare task success. Pass
    /// the same action budget to this method and the Harness plan runner. For a task-bound plan
    /// with root workspace guidance, use `harness::execution::create_plan_with_workspace_context`.
    pub fn create_plan(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        action_budget: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        self.create_plan_with_project_guidance(goal, observations, action_budget, None)
    }

    pub(crate) fn create_plan_with_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        action_budget: usize,
        project_guidance: Option<&str>,
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
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        let maximum = action_budget
            .min(self.remaining_decisions)
            .saturating_sub(1)
            .min(MAX_PLAN_OPERATIONS);
        if maximum == 0 {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }

        let repository_context = self.repository_context.as_ref();
        let request = match proposed_targets {
            Some(targets) => build_plan_request_with_project_guidance_and_targets(
                &self.model,
                goal,
                observations,
                maximum,
                project_guidance,
                Some(targets),
                repository_context,
            ),
            None => build_plan_request_with_project_guidance(
                &self.model,
                goal,
                observations,
                maximum,
                project_guidance,
                repository_context,
            ),
        }
        .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let advertised_tools = AdvertisedToolNames::from_request(&request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let response = post_chat(&self.agent, self.address, &request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        parse_model_plan_response(&response, maximum, &advertised_tools)
    }

    pub(crate) fn accept_plan_with_project_guidance(
        &mut self,
        plan: WorkspacePlan,
        action_budget: usize,
        project_guidance_context: Option<String>,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        let maximum = action_budget
            .min(self.remaining_decisions)
            .saturating_sub(1)
            .min(MAX_PLAN_OPERATIONS);
        if maximum == 0 {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }
        if plan.operations().len() > maximum {
            return Err(WorkspacePlanError::TooManyOperations { maximum });
        }
        self.remaining_decisions = self
            .remaining_decisions
            .saturating_sub(plan.operations().len());
        Ok(plan.with_project_guidance_context(project_guidance_context))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn review_impact_plan(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        action_budget: usize,
        proposed_plan: &WorkspacePlan,
        report: &ImpactReport,
        project_guidance: Option<&str>,
        review_slots_used_before: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        let maximum = action_budget
            .min(self.remaining_decisions)
            .saturating_sub(2 + review_slots_used_before)
            .min(MAX_PLAN_OPERATIONS);
        if maximum == 0 {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }
        let proposed_plan_json = encode_plan_for_impact_review(proposed_plan)?;
        let impact_report_json =
            serde_json::to_string(report).map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        if proposed_plan_json.len() > 48 * 1024 || impact_report_json.len() > 24 * 1024 {
            return Err(WorkspacePlanError::ModelRequestFailed);
        }
        let request = build_impact_review_request(
            &self.model,
            goal,
            observations,
            maximum,
            project_guidance,
            ImpactReviewContext {
                proposed_plan_json: &proposed_plan_json,
                impact_report_json: &impact_report_json,
            },
            self.repository_context.as_ref(),
        )
        .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let advertised_tools = AdvertisedToolNames::from_request(&request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let response = post_chat(&self.agent, self.address, &request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        parse_model_plan_response(&response, maximum, &advertised_tools)
    }

    pub(crate) fn review_correction_impact_plan(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        proposed_plan: &WorkspacePlan,
        report: &ImpactReport,
        project_guidance: Option<&str>,
        maximum_operations: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        if self.remaining_decisions == 0 || maximum_operations == 0 {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }
        let proposed_plan_json = encode_plan_for_impact_review(proposed_plan)?;
        let impact_report_json =
            serde_json::to_string(report).map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        if proposed_plan_json.len() > 48 * 1024 || impact_report_json.len() > 24 * 1024 {
            return Err(WorkspacePlanError::ModelRequestFailed);
        }
        let request = build_impact_review_request(
            &self.model,
            goal,
            observations,
            maximum_operations.min(MAX_PLAN_OPERATIONS),
            project_guidance,
            ImpactReviewContext {
                proposed_plan_json: &proposed_plan_json,
                impact_report_json: &impact_report_json,
            },
            self.repository_context.as_ref(),
        )
        .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let advertised_tools = AdvertisedToolNames::from_request(&request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        let response = post_chat(&self.agent, self.address, &request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        parse_model_plan_response(
            &response,
            maximum_operations.min(MAX_PLAN_OPERATIONS),
            &advertised_tools,
        )
    }

    pub(crate) fn accept_impact_reviewed_plan(
        &mut self,
        plan: WorkspacePlan,
        action_budget: usize,
        project_guidance_context: Option<String>,
        review_slots_used: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        let maximum = action_budget
            .min(self.remaining_decisions)
            .saturating_sub(1 + review_slots_used)
            .min(MAX_PLAN_OPERATIONS);
        if maximum == 0 {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }
        if plan.operations().len() > maximum {
            return Err(WorkspacePlanError::TooManyOperations { maximum });
        }
        self.remaining_decisions = self
            .remaining_decisions
            .saturating_sub(review_slots_used + plan.operations().len());
        Ok(plan.with_project_guidance_context(project_guidance_context))
    }

    pub(crate) fn accept_impact_reviewed_correction_plan(
        &mut self,
        plan: WorkspacePlan,
        original_operation_count: usize,
        review_slots_used: usize,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        if plan.operations().len() > original_operation_count {
            return Err(WorkspacePlanError::TooManyOperations {
                maximum: original_operation_count,
            });
        }
        if self.remaining_decisions < review_slots_used {
            return Err(WorkspacePlanError::ActionBudgetExhausted);
        }
        self.remaining_decisions = self
            .remaining_decisions
            .saturating_sub(review_slots_used)
            .saturating_add(original_operation_count - plan.operations().len());
        Ok(plan)
    }

    /// Uses one verified prior Harness result to inform a bounded workspace-only plan.
    ///
    /// Reflection is opt-in, consumes one existing decision-budget slot, and is limited to one
    /// local model response of at most 2 KiB. The prior observations, stop reason, and outcome
    /// must match the immutable turn outcome and ordered activity records in `session`. The
    /// resulting reflection is advisory only; this method returns a plan and performs no
    /// workspace operation. Pass the plan through the existing Harness executor.
    pub fn create_reflective_plan(
        &mut self,
        session: &TaskSession,
        prior_result: &GoalTurnResult,
        goal: &str,
        observations: &[WorkspaceObservation],
        action_budget: usize,
    ) -> Result<WorkspacePlan, BrainReflectionError> {
        reflection::create_reflective_plan(
            self,
            session,
            prior_result,
            goal,
            observations,
            action_budget,
        )
    }

    /// Uses one failed or unknown independent check result to inform one bounded correction plan.
    ///
    /// The check result is sent to Brain as bounded, untrusted advisory context. A passing result
    /// yields no plan, and the returned plan contains only registered workspace operations.
    /// Harness must execute it through `TaskSession` and re-run independent checks before success.
    pub fn create_completion_correction_plan(
        &mut self,
        session: &TaskSession,
        prior_result: &GoalTurnResult,
        goal: &str,
        observations: &[WorkspaceObservation],
        verification: &CompletionVerification,
        action_budget: usize,
    ) -> Result<Option<WorkspacePlan>, BrainReflectionError> {
        reflection::create_completion_correction_plan(
            self,
            session,
            prior_result,
            goal,
            observations,
            verification,
            action_budget,
        )
    }

    /// Uses a verified complete offline Cargo failure to inform one bounded correction plan.
    ///
    /// The original typed ProcessResult and its bounded output are supplied as untrusted
    /// advisory context. Harness accepts only determinate failures and remains responsible for
    /// the one-correction limit, registered Tool execution, remaining action budget, and later
    /// successful test evidence.
    pub fn create_process_failure_correction_plan(
        &mut self,
        session: &TaskSession,
        prior_result: &GoalTurnResult,
        failure: &ProcessResult,
        goal: &str,
        action_budget: usize,
    ) -> Result<WorkspacePlan, BrainReflectionError> {
        reflection::create_process_failure_correction_plan(
            self,
            session,
            prior_result,
            failure,
            goal,
            action_budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    /// Runs one bounded initial turn, independent checks, and at most one correction cycle.
    ///
    /// `check_evidence` must use TaskSession observations or deterministic local checks outside
    /// Brain. An optional initial plan must have been created by this same decision source with
    /// the original action budget. The Harness reuses this source for both turns and only records
    /// task success after the required checks pass.
    pub fn run_goal_turn_with_completion_correction<E>(
        &mut self,
        session: &mut TaskSession,
        task_id: uuid::Uuid,
        turn_id: uuid::Uuid,
        goal: &str,
        max_actions: usize,
        initial_plan: Option<Result<WorkspacePlan, WorkspacePlanError>>,
        required_checks: &[RequiredCompletionCheck],
        check_evidence: E,
    ) -> Result<CompletionCorrectionResult, CompletionCorrectionError>
    where
        E: FnMut(&TaskSession, &GoalTurnResult) -> Vec<CompletionCheckEvidence>,
    {
        crate::harness::execution::run_goal_turn_with_completion_correction(
            session,
            task_id,
            turn_id,
            goal,
            max_actions,
            initial_plan,
            required_checks,
            check_evidence,
            |brain: &mut OllamaDecisionSource,
             session,
             prior_result,
             verification,
             goal,
             remaining_actions| {
                brain
                    .create_completion_correction_plan(
                        session,
                        prior_result,
                        goal,
                        prior_result.observations(),
                        verification,
                        remaining_actions,
                    )
                    .and_then(|plan| {
                        plan.ok_or(BrainReflectionError::InvalidPlan(
                            WorkspacePlanError::InvalidResponse,
                        ))
                    })
            },
            self,
        )
    }

    #[allow(clippy::too_many_arguments)]
    /// Runs a task turn with one bounded correction after a complete offline Cargo failure.
    ///
    /// Correction proposals use the same Brain decision budget and registered workspace tools.
    /// Task success still requires a later complete passing test observation and the supplied
    /// independent completion checks.
    pub fn run_goal_turn_with_process_failure_correction<E>(
        &mut self,
        session: &mut TaskSession,
        task_id: uuid::Uuid,
        turn_id: uuid::Uuid,
        goal: &str,
        max_actions: usize,
        required_checks: &[RequiredCompletionCheck],
        check_evidence: E,
    ) -> Result<ProcessFailureCorrectionResult, ProcessFailureCorrectionError>
    where
        E: FnMut(&TaskSession, &GoalTurnResult) -> Vec<CompletionCheckEvidence>,
    {
        crate::harness::execution::run_goal_turn_with_process_failure_correction(
            session,
            task_id,
            turn_id,
            goal,
            max_actions,
            required_checks,
            check_evidence,
            |brain: &mut OllamaDecisionSource,
             session,
             prior_result,
             failure,
             _initial_verification,
             goal,
             remaining_actions| {
                brain.create_process_failure_correction_plan(
                    session,
                    prior_result,
                    failure,
                    goal,
                    remaining_actions,
                )
            },
            self,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
    ) -> Self {
        Self::with_endpoint_and_extensions_streaming(model, action_budget, address, &[], false)
    }

    #[cfg(test)]
    pub(crate) fn with_streaming_endpoint(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
    ) -> Self {
        Self::with_endpoint_and_extensions_streaming(model, action_budget, address, &[], true)
    }

    #[cfg(test)]
    pub(crate) fn with_streaming_endpoint_and_extensions(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
        extension_declarations: &[serde_json::Value],
    ) -> Self {
        Self::with_endpoint_and_extensions_streaming(
            model,
            action_budget,
            address,
            extension_declarations,
            true,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint_and_extensions_non_streaming(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
        extension_declarations: &[serde_json::Value],
    ) -> Self {
        Self::with_endpoint_and_extensions_streaming(
            model,
            action_budget,
            address,
            extension_declarations,
            false,
        )
    }

    fn choose(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        project_guidance: Option<&str>,
    ) -> Result<WorkspaceAction, ModelError> {
        if self.remaining_decisions == 0 {
            return Err(ModelError::DecisionBudgetExhausted);
        }
        let remaining = self.remaining_decisions;
        self.remaining_decisions -= 1;

        let streaming = self.profile_endpoint.is_none() && self.streaming_enabled;
        let request = if streaming {
            build_streaming_chat_request_with_extensions(
                &self.model,
                goal,
                observations,
                remaining,
                project_guidance,
                &self.extension_declarations,
            )?
        } else {
            build_chat_request_with_extensions(
                &self.model,
                goal,
                observations,
                remaining,
                project_guidance,
                &self.extension_declarations,
            )?
        };
        let advertised_tools = AdvertisedToolNames::from_request(&request)?;
        if streaming {
            let mut tool_calls = Vec::new();
            let mut completed = false;
            post_chat_stream(&self.agent, self.address, &request, |record| {
                if completed {
                    return Err(ModelError::InvalidToolCall(
                        "event received after response completion",
                    ));
                }
                let chunk: serde_json::Value =
                    serde_json::from_slice(record).map_err(|_| ModelError::InvalidJson)?;
                let message = chunk
                    .get("message")
                    .and_then(serde_json::Value::as_object)
                    .ok_or(ModelError::InvalidToolCall("missing assistant message"))?;
                if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                    return Err(ModelError::InvalidToolCall(
                        "tool calls must come from an assistant message",
                    ));
                }
                if let Some(value) = message.get("tool_calls") {
                    let calls = value
                        .as_array()
                        .ok_or(ModelError::InvalidToolCall("invalid tool call chunks"))?;
                    if tool_calls.len().saturating_add(calls.len()) > 1 {
                        return Err(ModelError::InvalidToolCall(
                            "expected exactly one tool call",
                        ));
                    }
                    for call in calls {
                        if let Some(arguments) = call.pointer("/function/arguments") {
                            if serde_json::to_vec(arguments)
                                .map_err(|_| ModelError::InvalidJson)?
                                .len()
                                > MAX_STREAMED_TOOL_ARGUMENT_BYTES
                            {
                                return Err(ModelError::ResponseTooLarge);
                            }
                        }
                        tool_calls.push(call.clone());
                    }
                }
                let done = chunk
                    .get("done")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or(ModelError::InvalidJson)?;
                if done {
                    match chunk.get("done_reason") {
                        None | Some(serde_json::Value::Null) => {}
                        Some(serde_json::Value::String(reason)) if reason == "stop" => {}
                        Some(serde_json::Value::String(_)) => {
                            return Err(ModelError::InvalidToolCall(
                                "response did not stop normally",
                            ));
                        }
                        Some(_) => return Err(ModelError::InvalidJson),
                    }
                    completed = true;
                }
                Ok(())
            })?;
            if !completed {
                return Err(ModelError::InvalidToolCall(
                    "stream ended before response completion",
                ));
            }
            let response = serde_json::json!({
                "message": {
                    "role": "assistant",
                    "tool_calls": tool_calls
                }
            });
            let response = serde_json::to_vec(&response).map_err(|_| ModelError::InvalidJson)?;
            return parse_model_response(&response, &advertised_tools);
        }
        let response = match self.profile_endpoint.as_deref() {
            Some(endpoint) => post_chat_at_endpoint(&self.agent, endpoint, &request)?,
            None => post_chat(&self.agent, self.address, &request)?,
        };
        parse_model_response(&response, &advertised_tools)
    }
}

impl BoundedDecisionSource for OllamaDecisionSource {
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

pub(crate) fn consume_decision_budget(remaining: &mut usize) -> Result<usize, ModelError> {
    if *remaining == 0 {
        return Err(ModelError::DecisionBudgetExhausted);
    }
    let available = *remaining;
    *remaining -= 1;
    Ok(available)
}
