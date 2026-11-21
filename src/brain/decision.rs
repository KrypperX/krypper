//! Ollama decision source and its bounded decision budget.

use crate::agent_loop::{
    BoundedDecisionSource, GoalTurnResult, WorkspaceAction, WorkspaceObservation,
};
use crate::harness::context::RepositoryContext;
use crate::harness::execution::{
    CompletionCorrectionError, CompletionCorrectionResult, ProcessFailureCorrectionError,
    ProcessFailureCorrectionResult,
};
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

pub struct OllamaDecisionSource {
    pub(super) model: String,
    pub(super) address: SocketAddr,
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
        Self::with_endpoint(model, action_budget, OLLAMA_ADDRESS)
    }

    /// Create a source that advertises host-authorized extension declarations.
    pub fn with_endpoint_and_extensions(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
        extension_declarations: &[serde_json::Value],
    ) -> Self {
        Self {
            model: model.into(),
            address,
            agent: model_agent(),
            remaining_decisions: action_budget,
            extension_declarations: extension_declarations.to_vec(),
            repository_context: None,
        }
    }

    pub(crate) fn set_repository_context(&mut self, context: RepositoryContext) {
        self.repository_context = Some(context);
    }

    pub(crate) fn clear_repository_context(&mut self) {
        self.repository_context = None;
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
        let response = post_chat(&self.agent, self.address, &request)
            .map_err(|_| WorkspacePlanError::ModelRequestFailed)?;
        parse_model_plan_response(&response, maximum)
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

    pub(crate) fn with_endpoint(
        model: impl Into<String>,
        action_budget: usize,
        address: SocketAddr,
    ) -> Self {
        Self::with_endpoint_and_extensions(model, action_budget, address, &[])
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

        let request = build_chat_request_with_extensions(
            &self.model,
            goal,
            observations,
            remaining,
            project_guidance,
            &self.extension_declarations,
        )?;
        let response = post_chat(&self.agent, self.address, &request)?;
        parse_model_response(&response)
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
