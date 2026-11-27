//! Convenience methods that compose Brain correction plans with Harness execution.

use super::{
    CompletionCorrectionError, CompletionCorrectionResult, ProcessFailureCorrectionError,
    ProcessFailureCorrectionResult,
};
use crate::brain::{BrainReflectionError, OllamaDecisionSource, WorkspacePlan, WorkspacePlanError};
use crate::contracts::completion::{CompletionCheckEvidence, RequiredCompletionCheck};
use crate::contracts::goal_turn::GoalTurnResult;
use crate::task_lifecycle::TaskSession;

impl OllamaDecisionSource {
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
        super::run_goal_turn_with_completion_correction(
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
        super::run_goal_turn_with_process_failure_correction(
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
}
