use super::{
    run_goal_turn_with_budget, workspace_action_name, BoundedDecisionSource, GoalTurnError,
    GoalTurnResult, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use crate::brain::{WorkspacePlan, WorkspacePlanError};
use crate::harness::verification::{
    reconcile_task_state_with_checks, verify_completion_checks, CompletionCheckEvidence,
    CompletionVerification, CompletionVerificationStatus, RequiredCompletionCheck,
};
use crate::task_lifecycle::{TaskSession, TurnState};
use crate::tools::registry::workspace_operations;
use std::fmt;
use std::io;
use uuid::Uuid;

/// Feeds plan operations through the existing bounded turn executor.
pub fn run_goal_turn_with_plan<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    plan: Result<WorkspacePlan, WorkspacePlanError>,
    control_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_plan_result(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        plan,
        control_source,
    )
}

fn run_goal_turn_with_plan_result<D, E>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    plan: Result<WorkspacePlan, E>,
    control_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError>
where
    D: BoundedDecisionSource + ?Sized,
    E: fmt::Display,
{
    let operations = match plan {
        Ok(plan) => plan.into_actions(),
        Err(error) => vec![WorkspaceAction::Unsupported {
            action: error.to_string(),
        }],
    };
    let mut source = PlanDecisionSource {
        operations: operations.into_iter(),
        control_source,
    };

    run_goal_turn_with_budget(session, task_id, turn_id, goal, max_actions, &mut source)
}

/// Runs the opt-in independent-check flow and permits at most one bounded correction.
///
/// The initial turn and optional initial plan use the existing Harness path. `check_evidence`
/// must use TaskSession observations or deterministic local checks outside Brain. The same
/// decision source is passed to the correction planner and both Harness turns so its decision
/// budget is shared. The caller must pass the original total action budget; correction receives
/// only the slots left after the initial turn. A correction is considered only after a completed
/// initial turn whose checks are failed or unknown. TaskSession is transitioned to success only
/// after the final independent checks pass on a completed turn.
pub fn run_goal_turn_with_completion_correction<D, E, P, PE>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    initial_plan: Option<Result<WorkspacePlan, WorkspacePlanError>>,
    required_checks: &[RequiredCompletionCheck],
    mut check_evidence: E,
    mut correction_plan: P,
    decision_source: &mut D,
) -> Result<CompletionCorrectionResult, CompletionCorrectionError>
where
    D: BoundedDecisionSource + ?Sized,
    E: FnMut(&TaskSession, &GoalTurnResult) -> Vec<CompletionCheckEvidence>,
    P: FnMut(
        &mut D,
        &TaskSession,
        &GoalTurnResult,
        &CompletionVerification,
        &str,
        usize,
    ) -> Result<WorkspacePlan, PE>,
    PE: fmt::Display,
{
    let initial_result = match initial_plan {
        Some(plan) => run_goal_turn_with_plan_result(
            session,
            task_id,
            turn_id,
            goal,
            max_actions,
            plan,
            decision_source,
        ),
        None => run_goal_turn_with_budget(
            session,
            task_id,
            turn_id,
            goal,
            max_actions,
            decision_source,
        ),
    }
    .map_err(CompletionCorrectionError::Execution)?;

    let initial_is_complete = matches!(
        initial_result.stop_reason(),
        GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted
    );
    let initial_evidence = if initial_is_complete {
        check_evidence(session, &initial_result)
    } else {
        Vec::new()
    };
    let initial_verification = verify_completion_checks(required_checks, &initial_evidence);
    if !initial_is_complete || initial_verification.status == CompletionVerificationStatus::Passed {
        let final_verification = reconcile_task_state_with_checks(
            session,
            task_id,
            initial_result.stop_reason(),
            required_checks,
            &initial_evidence,
        )
        .map_err(CompletionCorrectionError::Session)?;
        return Ok(CompletionCorrectionResult {
            initial_result,
            initial_verification,
            correction_result: None,
            final_verification,
        });
    }

    let remaining_actions = max_actions.saturating_sub(initial_result.actions_taken());
    // The existing Brain path reserves a reflection decision, one workspace operation, and
    // task control before it can make a correction proposal.
    if remaining_actions < 3 {
        let final_verification = reconcile_task_state_with_checks(
            session,
            task_id,
            initial_result.stop_reason(),
            required_checks,
            &initial_evidence,
        )
        .map_err(CompletionCorrectionError::Session)?;
        return Ok(CompletionCorrectionResult {
            initial_result,
            initial_verification,
            correction_result: None,
            final_verification,
        });
    }

    let plan = correction_plan(
        decision_source,
        session,
        &initial_result,
        &initial_verification,
        goal,
        remaining_actions,
    );
    let correction_turn_id =
        create_follow_up_turn(session, task_id).map_err(CompletionCorrectionError::Session)?;
    let correction_result = run_goal_turn_with_plan_result(
        session,
        task_id,
        correction_turn_id,
        goal,
        remaining_actions,
        plan,
        decision_source,
    )
    .map_err(CompletionCorrectionError::Execution)?;
    let final_evidence = check_evidence(session, &correction_result);
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        correction_result.stop_reason(),
        required_checks,
        &final_evidence,
    )
    .map_err(CompletionCorrectionError::Session)?;

    Ok(CompletionCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: Some(correction_result),
        final_verification,
    })
}

fn create_follow_up_turn(session: &mut TaskSession, task_id: Uuid) -> io::Result<Uuid> {
    let task = session
        .get_task_mut(task_id)
        .ok_or_else(|| io::Error::other("task was not found in its session"))?;
    let turn_id = task.create_turn().map_err(io::Error::other)?;
    task.transition_turn(turn_id, TurnState::Running)
        .map_err(io::Error::other)?;
    Ok(turn_id)
}

/// Results and independent evidence from the initial turn and optional correction turn.
#[derive(Debug)]
pub struct CompletionCorrectionResult {
    initial_result: GoalTurnResult,
    initial_verification: CompletionVerification,
    correction_result: Option<GoalTurnResult>,
    final_verification: CompletionVerification,
}

impl CompletionCorrectionResult {
    /// Returns the result of the initial Harness turn.
    pub fn initial_result(&self) -> &GoalTurnResult {
        &self.initial_result
    }

    /// Returns the initial independent completion-check result.
    pub fn initial_verification(&self) -> &CompletionVerification {
        &self.initial_verification
    }

    /// Returns the single correction turn result, if a correction was attempted.
    pub fn correction_result(&self) -> Option<&GoalTurnResult> {
        self.correction_result.as_ref()
    }

    /// Returns the independent checks that determine the final task state.
    pub fn final_verification(&self) -> &CompletionVerification {
        &self.final_verification
    }
}

/// An error while executing the opt-in completion-check and correction flow.
#[derive(Debug)]
pub enum CompletionCorrectionError {
    /// A Harness turn could not run or be recorded.
    Execution(GoalTurnError),
    /// A TaskSession lifecycle operation could not be completed.
    Session(io::Error),
}

impl fmt::Display for CompletionCorrectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Execution(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CompletionCorrectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Execution(error) => Some(error),
            Self::Session(error) => Some(error),
        }
    }
}

struct PlanDecisionSource<'a, D: ?Sized> {
    operations: std::vec::IntoIter<WorkspaceAction>,
    control_source: &'a mut D,
}

impl<D: BoundedDecisionSource + ?Sized> BoundedDecisionSource for PlanDecisionSource<'_, D> {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self.operations.next() {
            Some(action) if is_registered_workspace_operation(&action) => action,
            Some(action) => unsupported_action(action),
            None => match self.control_source.choose_action(goal, observations) {
                action @ (WorkspaceAction::Complete { .. }
                | WorkspaceAction::Continue
                | WorkspaceAction::Cancel) => action,
                action => unsupported_action(action),
            },
        }
    }
}

fn is_registered_workspace_operation(action: &WorkspaceAction) -> bool {
    matches!(
        action,
        WorkspaceAction::ReadFile { .. }
            | WorkspaceAction::ReadRange { .. }
            | WorkspaceAction::ListDirectory { .. }
            | WorkspaceAction::CreateDirectory { .. }
            | WorkspaceAction::SearchText { .. }
            | WorkspaceAction::SearchTreeText { .. }
            | WorkspaceAction::CreateFile { .. }
            | WorkspaceAction::ReplaceText { .. }
    ) && workspace_operations()
        .iter()
        .any(|operation| operation.name() == workspace_action_name(action))
}

fn unsupported_action(action: WorkspaceAction) -> WorkspaceAction {
    match action {
        WorkspaceAction::Unsupported { .. } => action,
        action => WorkspaceAction::Unsupported {
            action: workspace_action_name(&action).to_owned(),
        },
    }
}
