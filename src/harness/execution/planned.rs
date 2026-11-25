use super::{
    run_goal_turn_with_budget, workspace_action_name, workspace_mutation_target,
    BoundedDecisionSource, GoalTurnError, GoalTurnResult, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::brain::{OllamaDecisionSource, WorkspacePlan, WorkspacePlanError};
use crate::harness::context::collect_repository_context;
use crate::harness::impact::{
    analyze_plan, ImpactAnalysisError, ImpactCheckedPlan, ImpactCoverage, ImpactReport,
    PublicApiStatus,
};
use crate::harness::verification::{
    reconcile_task_state_with_checks, verify_completion_checks, CompletionCheckEvidence,
    CompletionVerification, CompletionVerificationStatus, RequiredCompletionCheck,
};
use crate::task_lifecycle::{
    TaskSession, TaskState, TurnActivityAction, TurnActivitySummary, TurnOutcome, TurnState,
};
use crate::tools::registry::workspace_operations;
use crate::{ProcessOutcome, ProcessResult};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use uuid::Uuid;

/// Reads root guidance before proposing targets, then supplies applicable nested guidance before
/// requesting the plan that Harness may execute.
///
/// Every guidance read uses the captured workspace root, no-follow descriptor checks, and the
/// existing 16 KiB UTF-8/context boundary. Missing guidance leaves the planner prompt unchanged.
/// Unsafe guidance and target paths fail before a plan can reach the executor. Guidance remains
/// untrusted prompt context; the resulting plan is validated and executed through the existing
/// Harness path.
pub fn create_plan_with_workspace_context(
    session: &TaskSession,
    task_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
) -> Result<WorkspacePlan, WorkspacePlanError> {
    let plan = propose_plan_with_workspace_context(
        session,
        task_id,
        decision_source,
        goal,
        observations,
        action_budget,
    )?;
    let guidance = plan.project_guidance_context().map(str::to_owned);
    decision_source.accept_plan_with_project_guidance(plan, action_budget, guidance)
}

fn propose_plan_with_workspace_context(
    session: &TaskSession,
    task_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
) -> Result<WorkspacePlan, WorkspacePlanError> {
    let root_guidance = session
        .read_root_project_guidance(task_id)
        .map_err(WorkspacePlanError::RootGuidanceRead)?;
    let proposal = decision_source.propose_plan_with_project_guidance(
        goal,
        observations,
        action_budget,
        root_guidance.as_deref(),
        None,
    )?;
    let proposal_targets = plan_mutation_targets(&proposal);
    if proposal_targets.is_empty() {
        return Ok(proposal.with_project_guidance_context(root_guidance));
    }

    let proposed_guidance = session
        .read_project_guidance_for_targets(task_id, &proposal_targets)
        .map_err(WorkspacePlanError::RootGuidanceRead)?;
    if proposed_guidance == root_guidance {
        return Ok(proposal.with_project_guidance_context(proposed_guidance));
    }

    let final_plan = decision_source.propose_plan_with_project_guidance(
        goal,
        observations,
        action_budget,
        proposed_guidance.as_deref(),
        Some(&proposal_targets),
    )?;
    let final_targets = plan_mutation_targets(&final_plan);
    let final_guidance = session
        .read_project_guidance_for_targets(task_id, &final_targets)
        .map_err(WorkspacePlanError::RootGuidanceRead)?;
    if final_guidance != proposed_guidance {
        return Err(WorkspacePlanError::GuidanceTargetsChanged);
    }

    Ok(final_plan.with_project_guidance_context(final_guidance))
}

/// Proposes, analyzes, model-reviews, then re-analyzes a bounded workspace plan before execution.
/// A material change in the reviewed report receives one bounded confirmation review. Each
/// existing file may be mutated once in the plan so its full SHA-256 precondition remains
/// unambiguous at the atomic write boundary.
pub fn create_impact_checked_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
) -> Result<ImpactCheckedPlan, WorkspacePlanError> {
    let proposal = propose_plan_with_workspace_context(
        session,
        task_id,
        decision_source,
        goal,
        observations,
        action_budget,
    )?;
    check_and_review_impact_plan(
        session,
        task_id,
        turn_id,
        decision_source,
        goal,
        observations,
        action_budget,
        proposal,
        false,
    )
}

/// Analyzes and, when the report requires it, sends a prebuilt correction plan and its impact
/// report through the same model review before returning a guarded plan.
#[allow(clippy::too_many_arguments)]
pub(crate) fn review_existing_impact_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
    proposed_plan: WorkspacePlan,
) -> Result<ImpactCheckedPlan, WorkspacePlanError> {
    check_and_review_impact_plan(
        session,
        task_id,
        turn_id,
        decision_source,
        goal,
        observations,
        action_budget,
        proposed_plan,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn check_and_review_impact_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
    proposal: WorkspacePlan,
    correction_operations_already_charged: bool,
) -> Result<ImpactCheckedPlan, WorkspacePlanError> {
    let guidance = proposal.project_guidance_context().map(str::to_owned);
    let proposal_targets = plan_mutation_targets(&proposal);
    let current_guidance = if proposal_targets.is_empty() {
        session.read_root_project_guidance(task_id)
    } else {
        session.read_project_guidance_for_targets(task_id, &proposal_targets)
    }
    .map_err(WorkspacePlanError::RootGuidanceRead)?;
    if current_guidance != guidance {
        return Err(WorkspacePlanError::GuidanceTargetsChanged);
    }
    let proposal = proposal.with_project_guidance_context(current_guidance);
    let (initial_report, _) =
        analyze_plan(session, task_id, turn_id, &proposal).map_err(map_impact_error)?;
    let guidance = proposal.project_guidance_context().map(str::to_owned);
    if initial_report.files.is_empty() {
        let plan = if correction_operations_already_charged {
            proposal
        } else {
            decision_source.accept_plan_with_project_guidance(
                proposal,
                action_budget,
                guidance.clone(),
            )?
        };
        return Ok(ImpactCheckedPlan {
            plan,
            report: initial_report,
            expected_revisions: Default::default(),
        });
    }

    let mut review_slots_used = 0;
    let mut report_sent = None;
    let mut reviewed = proposal.clone();
    if impact_needs_model_review(&initial_report) {
        reviewed = request_impact_review(
            decision_source,
            goal,
            observations,
            action_budget,
            &reviewed,
            &initial_report,
            guidance.as_deref(),
            correction_operations_already_charged,
            review_slots_used,
        )?;
        review_slots_used = 1;
        report_sent = Some(initial_report.clone());
        reviewed = reviewed.with_project_guidance_context(guidance.clone());
    }

    let (mut report, mut expected_revisions) =
        analyze_reviewed_plan(session, task_id, turn_id, &reviewed, &guidance)?;
    if report_sent.is_none() && impact_needs_model_review(&report) {
        reviewed = request_impact_review(
            decision_source,
            goal,
            observations,
            action_budget,
            &reviewed,
            &report,
            guidance.as_deref(),
            correction_operations_already_charged,
            review_slots_used,
        )?;
        review_slots_used = 1;
        report_sent = Some(report.clone());
        reviewed = reviewed.with_project_guidance_context(guidance.clone());
        (report, expected_revisions) =
            analyze_reviewed_plan(session, task_id, turn_id, &reviewed, &guidance)?;
    }
    if report_sent.as_ref().is_some_and(|sent| sent != &report) {
        if review_slots_used >= 2 {
            return Err(WorkspacePlanError::InvalidOperation);
        }
        let confirmation = request_impact_review(
            decision_source,
            goal,
            observations,
            action_budget,
            &reviewed,
            &report,
            guidance.as_deref(),
            correction_operations_already_charged,
            review_slots_used,
        )?;
        if confirmation.operations() != reviewed.operations() {
            return Err(WorkspacePlanError::InvalidOperation);
        }
        let confirmation = confirmation.with_project_guidance_context(guidance.clone());
        review_slots_used += 1;
        let (confirmed_report, confirmed_revisions) =
            analyze_reviewed_plan(session, task_id, turn_id, &confirmation, &guidance)?;
        if confirmed_report != report || confirmed_revisions != expected_revisions {
            return Err(WorkspacePlanError::InvalidOperation);
        }
        reviewed = confirmation;
        report = confirmed_report;
        expected_revisions = confirmed_revisions;
    }

    let plan = if correction_operations_already_charged {
        decision_source.accept_impact_reviewed_correction_plan(
            reviewed,
            proposal.operations().len(),
            review_slots_used,
        )?
    } else if review_slots_used > 0 {
        decision_source.accept_impact_reviewed_plan(
            reviewed,
            action_budget,
            guidance,
            review_slots_used,
        )?
    } else {
        decision_source.accept_plan_with_project_guidance(reviewed, action_budget, guidance)?
    };
    Ok(ImpactCheckedPlan {
        plan,
        report,
        expected_revisions,
    })
}

fn impact_needs_model_review(report: &ImpactReport) -> bool {
    report.mutation_targets.len() > 1
        || report.truncated
        || !report.unknowns.is_empty()
        || report.coverage != Some(ImpactCoverage::Complete)
        || report
            .references
            .iter()
            .any(|reference| reference.may_remain_after_plan)
        || report.files.iter().any(|file| {
            matches!(
                file.public_api_status,
                PublicApiStatus::Changed | PublicApiStatus::Unknown
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn request_impact_review(
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
    plan: &WorkspacePlan,
    report: &ImpactReport,
    guidance: Option<&str>,
    correction_operations_already_charged: bool,
    review_slots_used_before: usize,
) -> Result<WorkspacePlan, WorkspacePlanError> {
    if correction_operations_already_charged {
        decision_source.review_correction_impact_plan(
            goal,
            observations,
            plan,
            report,
            guidance,
            plan.operations().len(),
        )
    } else {
        decision_source.review_impact_plan(
            goal,
            observations,
            action_budget,
            plan,
            report,
            guidance,
            review_slots_used_before,
        )
    }
}

fn analyze_reviewed_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    plan: &WorkspacePlan,
    expected_guidance: &Option<String>,
) -> Result<
    (
        crate::harness::impact::ImpactReport,
        BTreeMap<std::path::PathBuf, String>,
    ),
    WorkspacePlanError,
> {
    let targets = plan_mutation_targets(plan);
    let current_guidance = if targets.is_empty() {
        session.read_root_project_guidance(task_id)
    } else {
        session.read_project_guidance_for_targets(task_id, &targets)
    }
    .map_err(WorkspacePlanError::RootGuidanceRead)?;
    if &current_guidance != expected_guidance {
        return Err(WorkspacePlanError::GuidanceTargetsChanged);
    }
    analyze_plan(session, task_id, turn_id, plan).map_err(map_impact_error)
}

fn map_impact_error(error: ImpactAnalysisError) -> WorkspacePlanError {
    match error {
        ImpactAnalysisError::ConflictingEdits
        | ImpactAnalysisError::WorkspaceRead
        | ImpactAnalysisError::UnsupportedMutation
        | ImpactAnalysisError::TooManyFiles
        | ImpactAnalysisError::InvalidPreview
        | ImpactAnalysisError::StaleDuringAnalysis => WorkspacePlanError::InvalidOperation,
    }
}

/// Opt-in bounded repository retrieval followed by the existing guidance-aware planner.
///
/// Retrieval uses the running turn and is advisory only: it never changes workspace state or
/// execution authority. Stale and oversized sources are skipped and recorded in the bounded
/// context; unsafe task/turn or search failures fail before a plan is requested.
pub fn create_plan_with_repository_context(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    decision_source: &mut OllamaDecisionSource,
    goal: &str,
    observations: &[WorkspaceObservation],
    action_budget: usize,
) -> Result<WorkspacePlan, WorkspacePlanError> {
    let context = collect_repository_context(session, task_id, turn_id, goal)
        .map_err(|_| WorkspacePlanError::RepositoryContextFailed)?;
    decision_source.set_repository_context(context);
    let plan = create_plan_with_workspace_context(
        session,
        task_id,
        decision_source,
        goal,
        observations,
        action_budget,
    );
    decision_source.clear_repository_context();
    plan
}

fn plan_mutation_targets(plan: &WorkspacePlan) -> Vec<std::path::PathBuf> {
    plan.operations()
        .iter()
        .filter_map(workspace_mutation_target)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

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
    run_goal_turn_with_plan_and_revisions(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        plan,
        None,
        control_source,
    )
}

/// Executes a model-reviewed impact-checked plan through the existing workspace operation path.
pub fn run_goal_turn_with_impact_checked_plan<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    plan: Result<ImpactCheckedPlan, WorkspacePlanError>,
    control_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    match plan {
        Ok(checked) => {
            let fresh = analyze_plan(session, task_id, turn_id, &checked.plan).is_ok_and(
                |(report, revisions)| {
                    report == checked.report && revisions == checked.expected_revisions
                },
            );
            let execution_plan = if fresh {
                Ok(checked.plan)
            } else {
                Err(WorkspacePlanError::InvalidOperation)
            };
            let revisions = fresh.then_some(&checked.expected_revisions);
            run_goal_turn_with_plan_and_revisions::<D, WorkspacePlanError>(
                session,
                task_id,
                turn_id,
                goal,
                max_actions,
                execution_plan,
                revisions,
                control_source,
            )
        }
        Err(error) => run_goal_turn_with_plan_result(
            session,
            task_id,
            turn_id,
            goal,
            max_actions,
            Err(error),
            control_source,
        ),
    }
}

#[allow(clippy::too_many_arguments)] // Session ownership, plan bounds, revisions, and the existing control source stay explicit.
fn run_goal_turn_with_plan_and_revisions<D, E>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    plan: Result<WorkspacePlan, E>,
    expected_revisions: Option<&BTreeMap<std::path::PathBuf, String>>,
    control_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError>
where
    D: BoundedDecisionSource + ?Sized,
    E: fmt::Display,
{
    let operations = match plan {
        Ok(plan) => {
            let targets = plan_mutation_targets(&plan);
            if !targets.is_empty() {
                match session.read_project_guidance_for_targets(task_id, &targets) {
                    Ok(current_guidance) => {
                        let expected_guidance = plan.project_guidance_context();
                        if current_guidance.as_deref() != expected_guidance {
                            return Err(GoalTurnError::RootProjectGuidance(
                                crate::task_lifecycle::WorkspaceGuidanceReadError::GuidanceChanged,
                            ));
                        }
                        plan.into_actions()
                    }
                    Err(crate::task_lifecycle::WorkspaceGuidanceReadError::InvalidTargetPath) => {
                        vec![WorkspaceAction::Unsupported {
                            action: "planned mutation target is unsafe".to_owned(),
                        }]
                    }
                    Err(error) => return Err(GoalTurnError::RootProjectGuidance(error)),
                }
            } else {
                plan.into_actions()
            }
        }
        Err(error) => vec![WorkspaceAction::Unsupported {
            action: error.to_string(),
        }],
    };
    let revisions = operations
        .iter()
        .map(|action| {
            workspace_mutation_target(action)
                .and_then(|path| expected_revisions.and_then(|items| items.get(path)))
                .cloned()
        })
        .collect::<Vec<_>>();
    let mut source = PlanDecisionSource {
        operations: operations.into_iter(),
        expected_revisions: revisions.into_iter(),
        pending_revision: None,
        control_source,
    };

    run_goal_turn_with_budget(session, task_id, turn_id, goal, max_actions, &mut source)
}

// The correction needs a plan decision, a workspace operation, and task control.
const MIN_COMPLETION_CORRECTION_ACTIONS: usize = 3;
/// Runs the opt-in independent-check flow and permits at most one bounded correction.
///
/// The initial turn and optional initial plan use the existing Harness path. `check_evidence`
/// must use TaskSession observations or deterministic local checks outside Brain. The same
/// decision source is passed to the correction planner and both Harness turns so its decision
/// budget is shared. The caller must pass the original total action budget; correction receives
/// only the slots left after the initial turn. A correction is considered only after a completed
/// initial turn whose checks are failed or unknown. TaskSession is transitioned to success only
/// after the final independent checks pass on a completed turn. A correction-phase
/// error fails the running task after closing any still-running correction turn.
#[allow(clippy::too_many_arguments)] // Session, IDs, goal, budget, plan, checks, and correction hooks are distinct inputs.
pub fn run_goal_turn_with_completion_correction<D, E, P, PE>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    initial_plan: Option<Result<WorkspacePlan, WorkspacePlanError>>,
    required_checks: &[RequiredCompletionCheck],
    check_evidence: E,
    correction_plan: P,
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
    run_goal_turn_with_completion_correction_inner(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        initial_plan.map(InitialPlan::Workspace),
        required_checks,
        check_evidence,
        correction_plan,
        decision_source,
    )
}

/// Runs the independent-check flow using an impact-reviewed initial plan and at most one
/// bounded correction.
#[allow(clippy::too_many_arguments)] // Inputs mirror the existing completion-correction API.
pub fn run_goal_turn_with_impact_checked_completion_correction<D, E, P, PE>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    initial_plan: Option<Result<ImpactCheckedPlan, WorkspacePlanError>>,
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
        Uuid,
        Uuid,
        Option<&ProcessResult>,
        &GoalTurnResult,
        &CompletionVerification,
        &str,
        usize,
    ) -> Result<ImpactCheckedPlan, PE>,
    PE: fmt::Display,
{
    let initial_result = run_goal_turn_with_impact_checked_plan(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        initial_plan.unwrap_or(Err(WorkspacePlanError::InvalidResponse)),
        decision_source,
    )
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
    let process_failure = last_correctable_process_failure(&initial_result);
    if (!initial_is_complete && process_failure.is_none())
        || (process_failure.is_none()
            && initial_verification.status == CompletionVerificationStatus::Passed)
    {
        return finish_completion_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
        );
    }

    let remaining_actions = max_actions.saturating_sub(initial_result.actions_taken());
    let minimum_correction_actions = if process_failure.is_some() {
        MIN_PROCESS_CORRECTION_ACTIONS + 2
    } else {
        MIN_COMPLETION_CORRECTION_ACTIONS + 2
    };
    if remaining_actions < minimum_correction_actions {
        return finish_completion_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
        );
    }

    let correction_turn_id = match create_follow_up_turn(session, task_id) {
        Ok(id) => id,
        Err(error) => {
            fail_correction_task(session, task_id, None)
                .map_err(CompletionCorrectionError::Session)?;
            return Err(CompletionCorrectionError::Session(error));
        }
    };
    let plan = correction_plan(
        decision_source,
        session,
        task_id,
        correction_turn_id,
        process_failure,
        &initial_result,
        &initial_verification,
        goal,
        remaining_actions,
    )
    .map_err(|_| WorkspacePlanError::InvalidResponse);
    let correction_result = match run_goal_turn_with_impact_checked_plan(
        session,
        task_id,
        correction_turn_id,
        goal,
        remaining_actions,
        plan,
        decision_source,
    ) {
        Ok(result) => result,
        Err(error) => {
            fail_correction_task(session, task_id, Some(correction_turn_id))
                .map_err(CompletionCorrectionError::Session)?;
            return Err(CompletionCorrectionError::Execution(error));
        }
    };
    let test_was_requested = initial_result
        .observations()
        .iter()
        .chain(correction_result.observations())
        .any(|observation| matches!(observation, WorkspaceObservation::ProcessFinished { .. }));
    let current_test_verification = if test_was_requested {
        Some(process_test_verification(
            &correction_result,
            true,
            process_failure,
        ))
    } else {
        None
    };
    let test_verification = task_test_verification(session, task_id, current_test_verification);
    let final_evidence = check_evidence(session, &correction_result);
    let final_stop_reason =
        stop_reason_after_test_verification(correction_result.stop_reason(), test_verification);
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        final_stop_reason,
        required_checks,
        &final_evidence,
    )
    .map_err(CompletionCorrectionError::Session)?;
    Ok(CompletionCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: Some(correction_result),
        final_verification,
        test_verification,
        final_stop_reason,
    })
}

enum InitialPlan {
    Workspace(Result<WorkspacePlan, WorkspacePlanError>),
}

#[allow(clippy::too_many_arguments)] // Mirrors the public correction inputs while sharing lifecycle logic.
fn run_goal_turn_with_completion_correction_inner<D, E, P, PE>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    initial_plan: Option<InitialPlan>,
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
        Some(InitialPlan::Workspace(plan)) => run_goal_turn_with_plan_result(
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
        return finish_completion_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
        );
    }

    let remaining_actions = max_actions.saturating_sub(initial_result.actions_taken());
    if remaining_actions < MIN_COMPLETION_CORRECTION_ACTIONS {
        return finish_completion_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
        );
    }

    let plan = correction_plan(
        decision_source,
        session,
        &initial_result,
        &initial_verification,
        goal,
        remaining_actions,
    );
    let correction_turn_id = match create_follow_up_turn(session, task_id) {
        Ok(id) => id,
        Err(error) => {
            fail_correction_task(session, task_id, None)
                .map_err(CompletionCorrectionError::Session)?;
            return Err(CompletionCorrectionError::Session(error));
        }
    };
    let correction_result = match run_goal_turn_with_plan_result(
        session,
        task_id,
        correction_turn_id,
        goal,
        remaining_actions,
        plan,
        decision_source,
    ) {
        Ok(result) => result,
        Err(error) => {
            fail_correction_task(session, task_id, Some(correction_turn_id))
                .map_err(CompletionCorrectionError::Session)?;
            return Err(CompletionCorrectionError::Execution(error));
        }
    };
    let final_evidence = check_evidence(session, &correction_result);
    let test_verification = task_test_verification(session, task_id, None);
    let final_stop_reason =
        stop_reason_after_test_verification(correction_result.stop_reason(), test_verification);
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        final_stop_reason,
        required_checks,
        &final_evidence,
    )
    .map_err(CompletionCorrectionError::Session)?;

    Ok(CompletionCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: Some(correction_result),
        final_verification,
        test_verification,
        final_stop_reason,
    })
}

// The correction needs a plan decision, a workspace operation, a focused test rerun, and task
// control to complete and verify the repair.
const MIN_PROCESS_CORRECTION_ACTIONS: usize = 4;

/// Runs one task turn and permits a single correction for a complete offline Cargo failure.
///
/// A correction is eligible only when the last observation is a complete, cleanup-confirmed
/// Cargo nonzero exit or a typed zero-match result. It runs as a new turn in the same task and
/// receives only the original action-budget slots left after the first attempt. A later test
/// observation verifies tests only when it is complete, successful, ran at least one test, and
/// followed the last workspace mutation. Existing independent completion checks remain required
/// for task success. Correction-phase errors fail the running task after
/// closing any still-running correction turn.
#[allow(clippy::too_many_arguments)] // Session, IDs, goal, budget, checks, and correction hooks are distinct inputs.
pub fn run_goal_turn_with_process_failure_correction<D, E, P, PE>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    required_checks: &[RequiredCompletionCheck],
    mut check_evidence: E,
    mut correction_plan: P,
    decision_source: &mut D,
) -> Result<ProcessFailureCorrectionResult, ProcessFailureCorrectionError>
where
    D: BoundedDecisionSource + ?Sized,
    E: FnMut(&TaskSession, &GoalTurnResult) -> Vec<CompletionCheckEvidence>,
    P: FnMut(
        &mut D,
        &TaskSession,
        &GoalTurnResult,
        &ProcessResult,
        &CompletionVerification,
        &str,
        usize,
    ) -> Result<WorkspacePlan, PE>,
    PE: fmt::Display,
{
    let initial_result = run_goal_turn_with_budget(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
    )
    .map_err(ProcessFailureCorrectionError::Execution)?;

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
    let Some(failure) = last_correctable_process_failure(&initial_result) else {
        let test_verification = process_test_verification(&initial_result, false, None);
        return finish_process_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
            test_verification,
        );
    };

    let remaining_actions = max_actions.saturating_sub(initial_result.actions_taken());
    if remaining_actions < MIN_PROCESS_CORRECTION_ACTIONS {
        return finish_process_without_correction(
            session,
            task_id,
            initial_result,
            initial_verification,
            required_checks,
            &initial_evidence,
            ProcessTestVerificationStatus::Failed,
        );
    }

    let plan = correction_plan(
        decision_source,
        session,
        &initial_result,
        failure,
        &initial_verification,
        goal,
        remaining_actions,
    );
    let correction_turn_id = match create_follow_up_turn(session, task_id) {
        Ok(id) => id,
        Err(error) => {
            fail_correction_task(session, task_id, None)
                .map_err(ProcessFailureCorrectionError::Session)?;
            return Err(ProcessFailureCorrectionError::Session(error));
        }
    };
    let correction_result = match run_goal_turn_with_plan_result(
        session,
        task_id,
        correction_turn_id,
        goal,
        remaining_actions,
        plan,
        decision_source,
    ) {
        Ok(result) => result,
        Err(error) => {
            fail_correction_task(session, task_id, Some(correction_turn_id))
                .map_err(ProcessFailureCorrectionError::Session)?;
            return Err(ProcessFailureCorrectionError::Execution(error));
        }
    };
    let test_verification = task_test_verification(
        session,
        task_id,
        Some(process_test_verification(
            &correction_result,
            true,
            Some(failure),
        )),
    )
    .unwrap_or(ProcessTestVerificationStatus::Failed);
    let final_evidence = check_evidence(session, &correction_result);
    let final_stop_reason = stop_reason_after_test_verification(
        correction_result.stop_reason(),
        Some(test_verification),
    );
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        final_stop_reason,
        required_checks,
        &final_evidence,
    )
    .map_err(ProcessFailureCorrectionError::Session)?;

    Ok(ProcessFailureCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: Some(correction_result),
        final_verification,
        test_verification,
        final_stop_reason,
    })
}

fn last_correctable_process_failure(result: &GoalTurnResult) -> Option<&ProcessResult> {
    if result.stop_reason() != GoalTurnStopReason::ActionFailed {
        return None;
    }
    let Some(WorkspaceObservation::ProcessFinished { result }) = result.observations().last()
    else {
        return None;
    };
    let complete = !result.truncated && result.cleanup_confirmed;
    let determinate_failure = match result.outcome {
        ProcessOutcome::Exited(code) => code != 0,
        ProcessOutcome::NoTestsMatched => {
            result.test_filter.is_some() && !result.tests_ran && result.executed_tests == Some(0)
        }
        _ => false,
    };
    (complete && determinate_failure).then_some(result)
}

pub(crate) fn process_test_verification(
    result: &GoalTurnResult,
    required_after_correction: bool,
    failed_test: Option<&ProcessResult>,
) -> ProcessTestVerificationStatus {
    if successful_test_follows_workspace_changes(result, failed_test) {
        return ProcessTestVerificationStatus::Passed;
    }
    if required_after_correction
        || result
            .observations()
            .iter()
            .any(|observation| matches!(observation, WorkspaceObservation::ProcessFinished { .. }))
    {
        ProcessTestVerificationStatus::Failed
    } else {
        ProcessTestVerificationStatus::NotRequested
    }
}

pub(crate) fn stop_reason_after_test_verification(
    stop_reason: GoalTurnStopReason,
    test_verification: Option<ProcessTestVerificationStatus>,
) -> GoalTurnStopReason {
    if matches!(
        stop_reason,
        GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted
    ) && test_verification == Some(ProcessTestVerificationStatus::Failed)
    {
        GoalTurnStopReason::ActionFailed
    } else {
        stop_reason
    }
}

pub(crate) fn task_test_verification(
    session: &TaskSession,
    task_id: Uuid,
    current_turn_status: Option<ProcessTestVerificationStatus>,
) -> Option<ProcessTestVerificationStatus> {
    let task = session.get_task(task_id)?;
    let mut test_requested = false;
    let mut passed_after_latest_change = false;
    let mut workspace_revision = 0_u64;
    let mut requirements = TestVerificationRequirements::default();
    let activity = task.activity_history();
    for (index, event) in activity.iter().enumerate() {
        match event.summary() {
            TurnActivitySummary::OfflineCargoTestFinished {
                filter_hash,
                passed,
                retry_any_filter,
            } => {
                test_requested = true;
                if passed {
                    requirements.record_pass(filter_hash, workspace_revision);
                    passed_after_latest_change = requirements.is_satisfied(workspace_revision);
                } else {
                    passed_after_latest_change = false;
                    requirements.record_failure(filter_hash, retry_any_filter);
                }
            }
            TurnActivitySummary::ActionFailed {
                action: TurnActivityAction::OfflineCargoTest,
            } => {
                test_requested = true;
                passed_after_latest_change = false;
                requirements.require_full_suite();
            }
            TurnActivitySummary::DirectoryCreated
            | TurnActivitySummary::FileCreated { .. }
            | TurnActivitySummary::TextReplaced { .. }
            | TurnActivitySummary::ExtensionCompleted => {
                workspace_revision = workspace_revision.saturating_add(1);
                if test_requested {
                    passed_after_latest_change = false;
                }
            }
            TurnActivitySummary::ActionFailed { .. } => {
                if test_requested {
                    workspace_revision = workspace_revision.saturating_add(1);
                    passed_after_latest_change = false;
                }
            }
            TurnActivitySummary::ProcessFinished { .. } => {
                let has_durable_test_summary = activity.get(index + 1).is_some_and(|next| {
                    next.turn_id() == event.turn_id()
                        && matches!(
                            next.summary(),
                            TurnActivitySummary::OfflineCargoTestFinished { .. }
                        )
                });
                if !has_durable_test_summary {
                    // Version 1 checkpoints kept only process success and byte counts. Treat that
                    // legacy evidence as unknown and require a fresh full-suite pass.
                    test_requested = true;
                    passed_after_latest_change = false;
                    requirements.require_full_suite();
                    workspace_revision = workspace_revision.saturating_add(1);
                }
            }
            TurnActivitySummary::FileRead { .. }
            | TurnActivitySummary::FileRangeRead { .. }
            | TurnActivitySummary::DirectoryListed { .. }
            | TurnActivitySummary::TextSearched { .. }
            | TurnActivitySummary::TreeTextSearched { .. }
            | TurnActivitySummary::Completed
            | TurnActivitySummary::Continued
            | TurnActivitySummary::BudgetExhausted { .. }
            | TurnActivitySummary::Cancelled => {}
        }
    }

    if current_turn_status == Some(ProcessTestVerificationStatus::Failed) {
        return Some(ProcessTestVerificationStatus::Failed);
    }
    if test_requested {
        Some(
            if passed_after_latest_change && requirements.is_satisfied(workspace_revision) {
                ProcessTestVerificationStatus::Passed
            } else {
                ProcessTestVerificationStatus::Failed
            },
        )
    } else {
        current_turn_status
    }
}

#[derive(Default)]
struct TestVerificationRequirements {
    any_filter: bool,
    any_filter_passed_revision: Option<u64>,
    failed_filters: BTreeMap<[u8; 32], Option<u64>>,
    full_suite: bool,
    full_suite_passed_revision: Option<u64>,
}

impl TestVerificationRequirements {
    fn record_failure(&mut self, filter_hash: Option<[u8; 32]>, retry_any_filter: bool) {
        if self.full_suite {
            self.full_suite_passed_revision = None;
        }
        if retry_any_filter {
            self.any_filter = true;
            self.any_filter_passed_revision = None;
        } else if let Some(filter_hash) = filter_hash {
            self.failed_filters.insert(filter_hash, None);
        } else {
            self.full_suite = true;
            self.full_suite_passed_revision = None;
        }
    }

    fn record_pass(&mut self, filter_hash: Option<[u8; 32]>, workspace_revision: u64) {
        if self.any_filter {
            self.any_filter_passed_revision = Some(workspace_revision);
        }
        if let Some(filter_hash) = filter_hash {
            if let Some(passed_revision) = self.failed_filters.get_mut(&filter_hash) {
                *passed_revision = Some(workspace_revision);
            }
        } else {
            if self.full_suite {
                self.full_suite_passed_revision = Some(workspace_revision);
            }
            for passed_revision in self.failed_filters.values_mut() {
                *passed_revision = Some(workspace_revision);
            }
        }
    }

    fn require_full_suite(&mut self) {
        self.full_suite = true;
        self.full_suite_passed_revision = None;
    }

    fn is_satisfied(&self, workspace_revision: u64) -> bool {
        (!self.any_filter || self.any_filter_passed_revision == Some(workspace_revision))
            && self
                .failed_filters
                .values()
                .all(|passed_revision| *passed_revision == Some(workspace_revision))
            && (!self.full_suite || self.full_suite_passed_revision == Some(workspace_revision))
    }
}

fn successful_test_follows_workspace_changes(
    result: &GoalTurnResult,
    failed_test: Option<&ProcessResult>,
) -> bool {
    if !matches!(
        result.stop_reason(),
        GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted
    ) {
        return false;
    }
    let Some((test_index, process_result)) = result
        .observations()
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, observation)| match observation {
            WorkspaceObservation::ProcessFinished { result } => Some((index, result)),
            _ => None,
        })
    else {
        return false;
    };
    let ran_complete_test = process_result.succeeded()
        && process_result.tests_ran
        && process_result.executed_tests.is_some_and(|count| count > 0);
    let filter_matches = failed_test.is_none_or(|failure| {
        matches!(failure.outcome, ProcessOutcome::NoTestsMatched)
            || process_result.test_filter.is_none()
            || process_result.test_filter == failure.test_filter
    });
    let later_workspace_change =
        result
            .observations()
            .iter()
            .enumerate()
            .any(|(index, observation)| {
                index > test_index
                    && matches!(
                        observation,
                        WorkspaceObservation::DirectoryCreated { .. }
                            | WorkspaceObservation::FileCreated { .. }
                            | WorkspaceObservation::TextReplaced { .. }
                            | WorkspaceObservation::PatchApplied { .. }
                    )
            });
    ran_complete_test && filter_matches && !later_workspace_change
}

/// The outcome of the test evidence carried by a process-correction flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTestVerificationStatus {
    /// No offline Cargo operation was requested in the turn.
    NotRequested,
    /// A requested test failed, was incomplete, ran no tests, or had no later passing rerun.
    Failed,
    /// A complete successful test ran after the last workspace mutation.
    Passed,
}

/// Results and separate completion-check and test evidence for one process correction flow.
#[derive(Debug)]
pub struct ProcessFailureCorrectionResult {
    initial_result: GoalTurnResult,
    initial_verification: CompletionVerification,
    correction_result: Option<GoalTurnResult>,
    final_verification: CompletionVerification,
    test_verification: ProcessTestVerificationStatus,
    final_stop_reason: GoalTurnStopReason,
}

impl ProcessFailureCorrectionResult {
    /// Returns the first Harness turn, including any original typed process failure evidence.
    pub fn initial_result(&self) -> &GoalTurnResult {
        &self.initial_result
    }

    /// Returns the initial independent completion-check result.
    pub fn initial_verification(&self) -> &CompletionVerification {
        &self.initial_verification
    }

    /// Returns the one correction turn, if an eligible process failure received enough budget.
    pub fn correction_result(&self) -> Option<&GoalTurnResult> {
        self.correction_result.as_ref()
    }

    /// Returns the independent completion checks after the final turn.
    pub fn final_verification(&self) -> &CompletionVerification {
        &self.final_verification
    }

    /// Returns whether complete test evidence passed after any correction.
    pub fn test_verification(&self) -> ProcessTestVerificationStatus {
        self.test_verification
    }

    /// Returns the effective stop reason after test evidence has been applied.
    pub fn final_stop_reason(&self) -> GoalTurnStopReason {
        self.final_stop_reason
    }
}

/// An error while executing the opt-in process-failure correction flow.
#[derive(Debug)]
pub enum ProcessFailureCorrectionError {
    /// A Harness turn could not run or be recorded.
    Execution(GoalTurnError),
    /// A TaskSession lifecycle operation could not be completed.
    Session(io::Error),
}

impl fmt::Display for ProcessFailureCorrectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Execution(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProcessFailureCorrectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Execution(error) => Some(error),
            Self::Session(error) => Some(error),
        }
    }
}

/// Fail a correction task only after closing an active follow-up turn. The original
/// execution error remains the caller-visible error when cleanup succeeds.
fn fail_correction_task(
    session: &mut TaskSession,
    task_id: Uuid,
    correction_turn_id: Option<Uuid>,
) -> io::Result<()> {
    if let Some(turn_id) = correction_turn_id {
        let task = session
            .get_task(task_id)
            .ok_or_else(|| io::Error::other("task was not found in its session"))?;
        if task
            .get_turn(turn_id)
            .is_some_and(|turn| turn.state() == TurnState::Running)
        {
            session
                .complete_turn(
                    task_id,
                    turn_id,
                    TurnOutcome::Failure {
                        detail: "correction execution failed".to_owned(),
                    },
                )
                .map_err(io::Error::other)?;
        }
    }
    let task = session
        .get_task_mut(task_id)
        .ok_or_else(|| io::Error::other("task was not found in its session"))?;
    if task.state() == TaskState::Running {
        task.transition(TaskState::Failed)
            .map_err(io::Error::other)?;
    }
    Ok(())
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
    test_verification: Option<ProcessTestVerificationStatus>,
    final_stop_reason: GoalTurnStopReason,
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

    /// Returns test evidence when either turn requested an offline Cargo test.
    pub fn test_verification(&self) -> Option<ProcessTestVerificationStatus> {
        self.test_verification
    }

    /// Returns the effective stop reason after test evidence has been applied.
    pub fn final_stop_reason(&self) -> GoalTurnStopReason {
        self.final_stop_reason
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

fn finish_completion_without_correction(
    session: &mut TaskSession,
    task_id: Uuid,
    initial_result: GoalTurnResult,
    initial_verification: CompletionVerification,
    required_checks: &[RequiredCompletionCheck],
    initial_evidence: &[CompletionCheckEvidence],
) -> Result<CompletionCorrectionResult, CompletionCorrectionError> {
    let current_test_verification = initial_result
        .observations()
        .iter()
        .any(|observation| matches!(observation, WorkspaceObservation::ProcessFinished { .. }))
        .then(|| process_test_verification(&initial_result, false, None));
    let test_verification = task_test_verification(session, task_id, current_test_verification);
    let final_stop_reason =
        stop_reason_after_test_verification(initial_result.stop_reason(), test_verification);
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        final_stop_reason,
        required_checks,
        initial_evidence,
    )
    .map_err(CompletionCorrectionError::Session)?;

    Ok(CompletionCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: None,
        final_verification,
        test_verification,
        final_stop_reason,
    })
}

fn finish_process_without_correction(
    session: &mut TaskSession,
    task_id: Uuid,
    initial_result: GoalTurnResult,
    initial_verification: CompletionVerification,
    required_checks: &[RequiredCompletionCheck],
    initial_evidence: &[CompletionCheckEvidence],
    test_verification: ProcessTestVerificationStatus,
) -> Result<ProcessFailureCorrectionResult, ProcessFailureCorrectionError> {
    let test_verification = task_test_verification(session, task_id, Some(test_verification))
        .unwrap_or(ProcessTestVerificationStatus::Failed);
    let final_stop_reason =
        stop_reason_after_test_verification(initial_result.stop_reason(), Some(test_verification));
    let final_verification = reconcile_task_state_with_checks(
        session,
        task_id,
        final_stop_reason,
        required_checks,
        initial_evidence,
    )
    .map_err(ProcessFailureCorrectionError::Session)?;

    Ok(ProcessFailureCorrectionResult {
        initial_result,
        initial_verification,
        correction_result: None,
        final_verification,
        test_verification,
        final_stop_reason,
    })
}

struct PlanDecisionSource<'a, D: ?Sized> {
    operations: std::vec::IntoIter<WorkspaceAction>,
    expected_revisions: std::vec::IntoIter<Option<String>>,
    pending_revision: Option<Option<String>>,
    control_source: &'a mut D,
}

impl<D: BoundedDecisionSource + ?Sized> BoundedDecisionSource for PlanDecisionSource<'_, D> {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self.operations.next() {
            Some(action) => {
                self.pending_revision = Some(self.expected_revisions.next().unwrap_or(None));
                if is_registered_workspace_operation(&action) {
                    action
                } else {
                    unsupported_action(action)
                }
            }
            None => {
                self.pending_revision = Some(None);
                match self.control_source.choose_action(goal, observations) {
                    action @ (WorkspaceAction::Complete { .. }
                    | WorkspaceAction::Continue
                    | WorkspaceAction::Cancel) => action,
                    action => unsupported_action(action),
                }
            }
        }
    }

    fn expected_file_revision(&mut self, _action: &WorkspaceAction) -> Option<String> {
        self.pending_revision.take().flatten()
    }
}

fn is_registered_workspace_operation(action: &WorkspaceAction) -> bool {
    matches!(
        action,
        WorkspaceAction::OfflineCargoTest { .. }
            | WorkspaceAction::ReadFile { .. }
            | WorkspaceAction::ReadRange { .. }
            | WorkspaceAction::ListDirectory { .. }
            | WorkspaceAction::CreateDirectory { .. }
            | WorkspaceAction::SearchText { .. }
            | WorkspaceAction::SearchTreeText { .. }
            | WorkspaceAction::CreateFile { .. }
            | WorkspaceAction::ReplaceText { .. }
            | WorkspaceAction::ApplyPatch { .. }
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

#[cfg(test)]
mod tests {
    use super::{
        finish_completion_without_correction, run_goal_turn_with_process_failure_correction,
        ProcessTestVerificationStatus,
    };
    use crate::brain::{WorkspacePlan, WorkspacePlanError};
    use crate::harness::execution::{
        BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    use crate::harness::verification::{
        verify_completion_checks, CompletionCheckEvidence, CompletionCheckEvidenceStatus,
        CompletionEvidenceSource, CompletionVerificationStatus, RequiredCompletionCheck,
    };
    use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
    use crate::{OfflineCargoPolicy, ProcessOutcome, ProcessResource, ProcessResult};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    const GOAL: &str = "correct one focused offline test failure";
    const FILTER: &str = "focused_test";

    struct TempWorkspace {
        root: PathBuf,
        _directory: crate::test_support::TempDir,
        workspace: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let root = crate::test_support::TempDir::with_prefix("krypper-process-correction-");
            let workspace = root.path().join("workspace");
            fs::create_dir_all(&workspace).expect("create fixture workspace");
            Self {
                root: root.path().to_path_buf(),
                _directory: root,
                workspace: workspace.to_path_buf(),
            }
        }
    }

    struct ScriptedDecisions(VecDeque<WorkspaceAction>);

    impl BoundedDecisionSource for ScriptedDecisions {
        fn choose_action(
            &mut self,
            _goal: &str,
            _observations: &[WorkspaceObservation],
        ) -> WorkspaceAction {
            self.0.pop_front().expect("scripted decision is available")
        }
    }

    fn process_result(
        outcome: ProcessOutcome,
        stdout: &str,
        stderr: &str,
        test_filter: Option<&str>,
        tests_ran: bool,
        executed_tests: Option<usize>,
    ) -> ProcessResult {
        ProcessResult {
            run_id: Uuid::new_v4(),
            outcome,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            test_filter: test_filter.map(str::to_owned),
            tests_ran,
            executed_tests,
            elapsed_ms: 0,
            output_bytes: stdout.len().saturating_add(stderr.len()),
            output_limit: 0,
            resource_events: Vec::new(),
            recovery_hold: false,
        }
    }

    fn running_session(
        workspace: &Path,
        results: impl IntoIterator<Item = ProcessResult>,
    ) -> (TaskSession, Uuid, Uuid) {
        let toolchain = workspace.parent().expect("fixture root").join("toolchain");
        let registry = workspace.parent().expect("fixture root").join("registry");
        fs::create_dir_all(toolchain.join("bin")).expect("create fixture toolchain");
        fs::create_dir_all(&registry).expect("create fixture registry");
        for binary in ["cargo", "rustc"] {
            fs::write(toolchain.join("bin").join(binary), b"\x7fELF")
                .expect("write inert toolchain header");
        }

        let mut session = TaskSession::new_in_workspace(workspace).expect("bind workspace");
        let created_task_id = session.create_task(GOAL).id();
        session
            .authorize_offline_cargo_tests(
                created_task_id,
                OfflineCargoPolicy::new(&toolchain, &registry).expect("fixture policy"),
            )
            .expect("grant task test authority");
        session.queue_offline_cargo_results_for_test(created_task_id, results);

        let task = session.get_task_mut(created_task_id).expect("created task");
        task.transition(TaskState::Running).expect("start task");
        let turn_id = task.create_turn().expect("create first turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("start first turn");
        (session, created_task_id, turn_id)
    }

    fn focused_test_action() -> WorkspaceAction {
        WorkspaceAction::OfflineCargoTest {
            test_filter: Some(FILTER.to_owned()),
        }
    }

    fn required_check() -> RequiredCompletionCheck {
        RequiredCompletionCheck {
            id: "correction_file_exists".to_owned(),
            description: "the registered correction created its fixture file".to_owned(),
        }
    }

    fn evidence_for_correction_file(
        workspace: &Path,
        result: &crate::harness::execution::GoalTurnResult,
    ) -> Vec<CompletionCheckEvidence> {
        let passed = result.stop_reason() == GoalTurnStopReason::Completed
            && workspace.join("correction.txt").is_file();
        vec![CompletionCheckEvidence {
            check_id: "correction_file_exists".to_owned(),
            status: if passed {
                CompletionCheckEvidenceStatus::Passed
            } else {
                CompletionCheckEvidenceStatus::Failed
            },
            source: CompletionEvidenceSource::DeterministicLocalCheck,
            summary: if passed {
                "correction.txt exists after a completed turn".to_owned()
            } else {
                "the completed correction file check did not pass".to_owned()
            },
        }]
    }

    fn correction_plan(
        remaining_actions: usize,
        filter: &str,
    ) -> Result<WorkspacePlan, WorkspacePlanError> {
        WorkspacePlan::try_from_actions(
            vec![
                WorkspaceAction::CreateFile {
                    path: PathBuf::from("correction.txt"),
                    contents: "registered correction".to_owned(),
                },
                WorkspaceAction::OfflineCargoTest {
                    test_filter: Some(filter.to_owned()),
                },
            ],
            remaining_actions,
        )
    }

    #[test]
    fn failed_test_can_be_corrected_once_and_later_pass_within_the_same_task() {
        let fixture = TempWorkspace::new();
        let outside = fixture.root.join("outside-canary.txt");
        fs::write(&outside, "unchanged outside canary").expect("write canary");
        let first_failure = process_result(
            ProcessOutcome::Exited(101),
            "running 1 test\n",
            "test focused_test ... FAILED\n",
            Some(FILTER),
            true,
            Some(1),
        );
        let later_pass = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) =
            running_session(&fixture.workspace, [first_failure, later_pass]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            WorkspaceAction::OfflineCargoTest {
                test_filter: Some(FILTER.to_owned()),
            },
            WorkspaceAction::Complete {
                result: "corrected test passed".to_owned(),
            },
        ]));
        let mut planner_calls = 0;
        let mut check_calls = 0;

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            5,
            &[required_check()],
            |_, turn_result| {
                check_calls += 1;
                evidence_for_correction_file(&fixture.workspace, turn_result)
            },
            |_, _session, prior, failure, _checks, _goal, remaining| {
                planner_calls += 1;
                assert_eq!(prior.stop_reason(), GoalTurnStopReason::ActionFailed);
                assert!(matches!(
                    prior.observations(),
                    [WorkspaceObservation::ProcessFinished { result }]
                        if result == failure
                            && result.outcome == ProcessOutcome::Exited(101)
                            && !result.succeeded()
                            && result.stderr.contains("focused_test ... FAILED")
                ));
                assert_eq!(remaining, 4);
                correction_plan(remaining, FILTER)
            },
            &mut decisions,
        )
        .expect("run bounded process correction");

        let correction = result.correction_result().expect("one correction ran");
        assert_eq!(planner_calls, 1);
        assert_eq!(check_calls, 1);
        assert_eq!(
            result.initial_result().stop_reason(),
            GoalTurnStopReason::ActionFailed
        );
        assert!(matches!(
            result.initial_result().observations(),
            [WorkspaceObservation::ProcessFinished { result }]
                if result.outcome == ProcessOutcome::Exited(101) && !result.succeeded()
        ));
        assert_eq!(correction.stop_reason(), GoalTurnStopReason::Completed);
        assert_eq!(correction.task_id(), task_id);
        assert_ne!(correction.turn_id(), turn_id);
        assert_eq!(correction.actions_taken(), 3);
        assert!(result.initial_result().actions_taken() + correction.actions_taken() <= 5);
        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Passed
        );
        assert_eq!(
            result.final_verification().status,
            CompletionVerificationStatus::Passed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Succeeded
        );
        assert_eq!(
            session.offline_cargo_filters_for_test(task_id),
            [Some(FILTER.to_owned()), Some(FILTER.to_owned())]
        );
        assert_eq!(
            fs::read_to_string(&outside).expect("read canary"),
            "unchanged outside canary"
        );
    }

    #[test]
    fn passing_test_before_final_workspace_edit_fails_no_correction_completion() {
        let fixture = TempWorkspace::new();
        let passed_test = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) = running_session(&fixture.workspace, [passed_test]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            focused_test_action(),
            WorkspaceAction::CreateFile {
                path: PathBuf::from("correction.txt"),
                contents: "written after the test".to_owned(),
            },
            WorkspaceAction::Complete {
                result: "finish after writing the file".to_owned(),
            },
        ]));
        let initial_result = crate::harness::execution::run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            3,
            &mut decisions,
        )
        .expect("run test then final workspace edit");
        let required_checks = [required_check()];
        let evidence = evidence_for_correction_file(&fixture.workspace, &initial_result);
        assert_eq!(
            verify_completion_checks(&required_checks, &evidence).status,
            CompletionVerificationStatus::Passed
        );
        let initial_verification = verify_completion_checks(&required_checks, &evidence);

        let result = finish_completion_without_correction(
            &mut session,
            task_id,
            initial_result,
            initial_verification,
            &required_checks,
            &evidence,
        )
        .expect("reconcile test evidence without correction");

        assert_eq!(
            result.test_verification(),
            Some(ProcessTestVerificationStatus::Failed)
        );
        assert_eq!(result.final_stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_eq!(
            result.final_verification().status,
            CompletionVerificationStatus::Passed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Failed
        );
    }

    #[test]
    fn incomplete_turn_with_test_evidence_remains_resumable() {
        let fixture = TempWorkspace::new();
        let passed_test = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) = running_session(&fixture.workspace, [passed_test]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            focused_test_action(),
            WorkspaceAction::Continue,
        ]));
        let initial_result = crate::harness::execution::run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            3,
            &mut decisions,
        )
        .expect("stop the turn as resumable");
        assert_eq!(initial_result.stop_reason(), GoalTurnStopReason::Continued);
        let required_checks = [required_check()];
        let initial_verification = verify_completion_checks(&required_checks, &[]);

        let result = finish_completion_without_correction(
            &mut session,
            task_id,
            initial_result,
            initial_verification,
            &required_checks,
            &[],
        )
        .expect("keep incomplete task resumable");

        assert_eq!(
            result.test_verification(),
            Some(ProcessTestVerificationStatus::Failed)
        );
        assert_eq!(result.final_stop_reason(), GoalTurnStopReason::Continued);
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Running
        );
    }

    #[test]
    fn correction_must_rerun_failed_filter_or_full_suite() {
        let fixture = TempWorkspace::new();
        let first_failure = process_result(
            ProcessOutcome::Exited(101),
            "running 1 test\ntest focused_test ... FAILED\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let unrelated_pass = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest unrelated_test ... ok\n",
            "",
            Some("unrelated_test"),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) =
            running_session(&fixture.workspace, [first_failure, unrelated_pass]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            focused_test_action(),
            WorkspaceAction::Complete {
                result: "an unrelated test passed".to_owned(),
            },
        ]));

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            5,
            &[required_check()],
            |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
            |_, _, _, failure, _, _, remaining| {
                assert_eq!(failure.test_filter.as_deref(), Some(FILTER));
                WorkspacePlan::try_from_actions(
                    vec![
                        WorkspaceAction::CreateFile {
                            path: PathBuf::from("correction.txt"),
                            contents: "created before the unrelated test".to_owned(),
                        },
                        WorkspaceAction::OfflineCargoTest {
                            test_filter: Some("unrelated_test".to_owned()),
                        },
                    ],
                    remaining,
                )
            },
            &mut decisions,
        )
        .expect("record a correction that ran the wrong test filter");

        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Failed
        );
        assert_eq!(result.final_stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Failed
        );
    }

    #[test]
    fn zero_match_can_trigger_one_correction_with_a_new_authorized_filter() {
        let fixture = TempWorkspace::new();
        let zero_match = process_result(
            ProcessOutcome::NoTestsMatched,
            "running 0 tests\n",
            "test filter executed no tests\n",
            Some("missing_test"),
            false,
            Some(0),
        );
        let later_pass = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) =
            running_session(&fixture.workspace, [zero_match, later_pass]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            WorkspaceAction::OfflineCargoTest {
                test_filter: Some("missing_test".to_owned()),
            },
            WorkspaceAction::Complete {
                result: "corrected focused test passed".to_owned(),
            },
        ]));
        let mut planner_calls = 0;

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            5,
            &[required_check()],
            |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
            |_, _, prior, failure, _, _, remaining| {
                planner_calls += 1;
                assert_eq!(prior.stop_reason(), GoalTurnStopReason::ActionFailed);
                assert!(matches!(
                    prior.observations(),
                    [WorkspaceObservation::ProcessFinished { result }]
                        if result == failure
                            && result.outcome == ProcessOutcome::NoTestsMatched
                            && result.test_filter.as_deref() == Some("missing_test")
                            && !result.tests_ran
                            && result.executed_tests == Some(0)
                ));
                correction_plan(remaining, FILTER)
            },
            &mut decisions,
        )
        .expect("run correction after a typed zero-match result");

        assert_eq!(planner_calls, 1);
        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Passed
        );
        assert_eq!(
            session.offline_cargo_filters_for_test(task_id),
            [Some("missing_test".to_owned()), Some(FILTER.to_owned())]
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Succeeded
        );
    }

    #[test]
    fn a_failed_or_zero_match_correction_does_not_pass_or_start_another_cycle() {
        for correction_result in [
            process_result(
                ProcessOutcome::Exited(101),
                "running 1 test\n",
                "test focused_test ... FAILED\n",
                Some(FILTER),
                true,
                Some(1),
            ),
            process_result(
                ProcessOutcome::NoTestsMatched,
                "running 0 tests\n",
                "test filter executed no tests\n",
                Some(FILTER),
                false,
                Some(0),
            ),
        ] {
            let fixture = TempWorkspace::new();
            let first_failure = process_result(
                ProcessOutcome::Exited(101),
                "running 1 test\n",
                "test focused_test ... FAILED\n",
                Some(FILTER),
                true,
                Some(1),
            );
            let (mut session, task_id, turn_id) =
                running_session(&fixture.workspace, [first_failure, correction_result]);
            let mut decisions = ScriptedDecisions(VecDeque::from([
                focused_test_action(),
                WorkspaceAction::Complete {
                    result: "must not be requested after a failed correction test".to_owned(),
                },
            ]));
            let mut planner_calls = 0;

            let result = run_goal_turn_with_process_failure_correction(
                &mut session,
                task_id,
                turn_id,
                GOAL,
                5,
                &[required_check()],
                |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
                |_, _, prior, failure, _, _, remaining| {
                    planner_calls += 1;
                    assert!(matches!(
                        prior.observations().last(),
                        Some(WorkspaceObservation::ProcessFinished { result }) if result == failure
                    ));
                    correction_plan(remaining, FILTER)
                },
                &mut decisions,
            )
            .expect("record failed correction");

            let correction = result.correction_result().expect("one correction ran");
            assert_eq!(planner_calls, 1, "a second correction cycle is forbidden");
            assert_eq!(correction.stop_reason(), GoalTurnStopReason::ActionFailed);
            assert_eq!(
                result.test_verification(),
                ProcessTestVerificationStatus::Failed
            );
            assert_ne!(
                result.final_verification().status,
                CompletionVerificationStatus::Passed
            );
            assert_eq!(
                session.get_task(task_id).expect("task").state(),
                TaskState::Failed
            );
            assert_eq!(
                session.offline_cargo_filters_for_test(task_id),
                [Some(FILTER.to_owned()), Some(FILTER.to_owned())]
            );
        }
    }

    #[test]
    fn a_workspace_edit_after_the_rerun_invalidates_test_verification() {
        let fixture = TempWorkspace::new();
        let first_failure = process_result(
            ProcessOutcome::Exited(101),
            "running 1 test\n",
            "test focused_test ... FAILED\n",
            Some(FILTER),
            true,
            Some(1),
        );
        let later_pass = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) =
            running_session(&fixture.workspace, [first_failure, later_pass]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            focused_test_action(),
            WorkspaceAction::Complete {
                result: "must not claim the test covers a later edit".to_owned(),
            },
        ]));

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            5,
            &[required_check()],
            |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
            |_, _, _, _, _, _, remaining| {
                WorkspacePlan::try_from_actions(
                    vec![
                        WorkspaceAction::CreateFile {
                            path: PathBuf::from("correction.txt"),
                            contents: "first correction".to_owned(),
                        },
                        focused_test_action(),
                        WorkspaceAction::CreateFile {
                            path: PathBuf::from("changed_after_test.txt"),
                            contents: "not covered by the passing test".to_owned(),
                        },
                    ],
                    remaining,
                )
            },
            &mut decisions,
        )
        .expect("record post-test workspace edit");

        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Failed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Failed
        );
    }

    #[test]
    fn insufficient_action_budget_does_not_request_a_correction_or_pass() {
        let fixture = TempWorkspace::new();
        let first_failure = process_result(
            ProcessOutcome::Exited(101),
            "running 1 test\n",
            "test focused_test ... FAILED\n",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) = running_session(&fixture.workspace, [first_failure]);
        let mut decisions = ScriptedDecisions(VecDeque::from([focused_test_action()]));
        let mut planner_calls = 0;

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            4,
            &[required_check()],
            |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
            |_, _, _, _, _, _, _| {
                planner_calls += 1;
                Err(WorkspacePlanError::InvalidResponse)
            },
            &mut decisions,
        )
        .expect("stop after the first failed result when budget is insufficient");

        assert_eq!(planner_calls, 0);
        assert!(result.correction_result().is_none());
        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Failed
        );
        assert_eq!(
            result.initial_result().stop_reason(),
            GoalTurnStopReason::ActionFailed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Failed
        );
        assert_eq!(session.offline_cargo_filters_for_test(task_id).len(), 1);
    }

    #[test]
    fn terminal_non_test_process_outcome_stops_without_correction_or_success() {
        for (outcome, truncated, cleanup_confirmed) in [
            (ProcessOutcome::Denied, false, true),
            (ProcessOutcome::InvalidTestFilter, false, true),
            (ProcessOutcome::SandboxUnavailable, false, true),
            (ProcessOutcome::SandboxSetupFailed, false, true),
            (ProcessOutcome::SpawnFailed, false, true),
            (ProcessOutcome::Signaled(9), false, true),
            (ProcessOutcome::TimedOut, false, true),
            (ProcessOutcome::Cancelled, false, true),
            (
                ProcessOutcome::ResourceLimitExceeded {
                    resource: ProcessResource::Cpu,
                },
                false,
                true,
            ),
            (ProcessOutcome::OutcomeUnknown, false, true),
            (ProcessOutcome::OutputLimitExceeded, true, true),
            (ProcessOutcome::Exited(101), false, false),
        ] {
            let fixture = TempWorkspace::new();
            let terminal = ProcessResult {
                truncated,
                cleanup_confirmed,
                ..process_result(outcome, "bounded output", "", Some(FILTER), false, None)
            };
            let (mut session, task_id, turn_id) = running_session(&fixture.workspace, [terminal]);
            let mut decisions = ScriptedDecisions(VecDeque::from([focused_test_action()]));
            let mut planner_calls = 0;

            let result = run_goal_turn_with_process_failure_correction(
                &mut session,
                task_id,
                turn_id,
                GOAL,
                5,
                &[required_check()],
                |_, turn_result| evidence_for_correction_file(&fixture.workspace, turn_result),
                |_, _, _, _, _, _, _| {
                    planner_calls += 1;
                    Err(WorkspacePlanError::InvalidResponse)
                },
                &mut decisions,
            )
            .expect("preserve terminal process result");

            assert_eq!(planner_calls, 0, "terminal outcome must not be retried");
            assert!(result.correction_result().is_none());
            assert_eq!(
                result.test_verification(),
                ProcessTestVerificationStatus::Failed
            );
            assert_ne!(
                result.final_verification().status,
                CompletionVerificationStatus::Passed
            );
            assert_eq!(
                session.get_task(task_id).expect("task").state(),
                TaskState::Failed
            );
        }
    }

    #[test]
    fn initial_complete_test_pass_keeps_the_no_failure_path() {
        let fixture = TempWorkspace::new();
        let success = process_result(
            ProcessOutcome::Exited(0),
            "running 1 test\ntest focused_test ... ok\n",
            "",
            Some(FILTER),
            true,
            Some(1),
        );
        let (mut session, task_id, turn_id) = running_session(&fixture.workspace, [success]);
        let mut decisions = ScriptedDecisions(VecDeque::from([
            focused_test_action(),
            WorkspaceAction::Complete {
                result: "test passed".to_owned(),
            },
        ]));
        let mut planner_calls = 0;

        let result = run_goal_turn_with_process_failure_correction(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            5,
            &[required_check()],
            |_, _| {
                vec![CompletionCheckEvidence {
                    check_id: "correction_file_exists".to_owned(),
                    status: CompletionCheckEvidenceStatus::Passed,
                    source: CompletionEvidenceSource::DeterministicLocalCheck,
                    summary: "the independent fixture check passed".to_owned(),
                }]
            },
            |_, _, _, _, _, _, _| {
                planner_calls += 1;
                Err(WorkspacePlanError::InvalidResponse)
            },
            &mut decisions,
        )
        .expect("keep existing no-failure behavior");

        assert_eq!(planner_calls, 0);
        assert!(result.correction_result().is_none());
        assert_eq!(
            result.test_verification(),
            ProcessTestVerificationStatus::Passed
        );
        assert_eq!(
            result.final_verification().status,
            CompletionVerificationStatus::Passed
        );
        assert_eq!(
            session.get_task(task_id).expect("task").state(),
            TaskState::Succeeded
        );
    }
}

#[cfg(test)]
mod correction_error_lifecycle_tests {
    use super::fail_correction_task;
    use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};

    #[test]
    fn correction_error_closes_running_turn_before_failing_task() {
        let mut session = TaskSession::new();
        let task_id = session.create_task("correction").id();
        let task = session.get_task_mut(task_id).unwrap();
        task.transition(TaskState::Running).unwrap();
        let turn_id = task.create_turn().unwrap();
        task.transition_turn(turn_id, TurnState::Running).unwrap();

        fail_correction_task(&mut session, task_id, Some(turn_id)).unwrap();

        let task = session.get_task(task_id).unwrap();
        assert_eq!(task.state(), TaskState::Failed);
        assert_eq!(task.get_turn(turn_id).unwrap().state(), TurnState::Failed);
    }

    #[test]
    fn correction_setup_error_fails_task_without_follow_up_turn() {
        let mut session = TaskSession::new();
        let task_id = session.create_task("correction").id();
        session
            .get_task_mut(task_id)
            .unwrap()
            .transition(TaskState::Running)
            .unwrap();

        fail_correction_task(&mut session, task_id, None).unwrap();

        assert_eq!(
            session.get_task(task_id).unwrap().state(),
            TaskState::Failed
        );
    }

    #[test]
    fn correction_cleanup_preserves_already_completed_turn() {
        let mut session = TaskSession::new();
        let task_id = session.create_task("correction").id();
        let task = session.get_task_mut(task_id).unwrap();
        task.transition(TaskState::Running).unwrap();
        let turn_id = task.create_turn().unwrap();
        task.transition_turn(turn_id, TurnState::Running).unwrap();
        task.complete_turn(
            turn_id,
            TurnOutcome::Failure {
                detail: "original".into(),
            },
        )
        .unwrap();

        fail_correction_task(&mut session, task_id, Some(turn_id)).unwrap();

        let task = session.get_task(task_id).unwrap();
        assert_eq!(task.state(), TaskState::Failed);
        assert_eq!(task.get_turn(turn_id).unwrap().state(), TurnState::Failed);
    }
}

#[cfg(test)]
mod task_test_verification_tests {
    use super::{task_test_verification, ProcessTestVerificationStatus};
    use crate::task_lifecycle::{
        TaskSession, TaskState, TurnActivityAction, TurnActivitySummary, TurnOutcome, TurnState,
    };

    fn verification_for(activity: &[TurnActivitySummary]) -> ProcessTestVerificationStatus {
        let mut session = TaskSession::new();
        let task_id = session.create_task("check test verification history").id();
        let task = session.get_task_mut(task_id).expect("created task");
        task.transition(TaskState::Running).expect("start task");
        let turn_id = task.create_turn().expect("create turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("start turn");
        session
            .complete_turn_with_activity(
                task_id,
                turn_id,
                TurnOutcome::Success {
                    result: "recorded test history".to_owned(),
                },
                activity,
            )
            .expect("complete turn with activity");

        task_test_verification(&session, task_id, None).expect("test history is present")
    }

    fn record_test(
        activity: &mut Vec<TurnActivitySummary>,
        filter_hash: Option<[u8; 32]>,
        passed: bool,
        retry_any_filter: bool,
    ) {
        activity.push(TurnActivitySummary::ProcessFinished {
            succeeded: passed,
            output_bytes: 0,
        });
        activity.push(TurnActivitySummary::OfflineCargoTestFinished {
            filter_hash,
            passed,
            retry_any_filter,
        });
    }

    #[test]
    fn all_failed_filters_must_be_rerun_or_covered_by_a_full_suite() {
        let filter_b = [2; 32];
        let filter_c = [3; 32];
        let mut activity = Vec::new();
        record_test(&mut activity, Some(filter_b), false, false);
        activity.push(TurnActivitySummary::TextReplaced { bytes_written: 10 });
        record_test(&mut activity, Some(filter_c), false, false);
        record_test(&mut activity, Some(filter_c), true, false);

        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "passing C must not clear the outstanding failure from B"
        );

        record_test(&mut activity, Some(filter_b), true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Passed,
            "matching passes for both failed filters should clear both obligations"
        );
    }

    #[test]
    fn passing_filter_evidence_expires_after_a_later_workspace_change() {
        let mut activity = Vec::new();
        record_test(&mut activity, Some([2; 32]), false, false);
        record_test(&mut activity, Some([2; 32]), true, false);
        activity.push(TurnActivitySummary::TextReplaced { bytes_written: 12 });
        record_test(&mut activity, Some([3; 32]), true, false);

        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "a passing unrelated filter after an edit cannot revive stale B evidence"
        );

        record_test(&mut activity, Some([2; 32]), true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Passed,
            "rerunning B after the edit should refresh its evidence"
        );
    }

    #[test]
    fn no_tests_matched_retry_cannot_weaken_an_outstanding_filter() {
        let filter_b = [2; 32];
        let mut activity = Vec::new();
        record_test(&mut activity, Some(filter_b), false, false);
        activity.push(TurnActivitySummary::OfflineCargoTestFinished {
            filter_hash: Some([3; 32]),
            passed: false,
            retry_any_filter: true,
        });
        record_test(&mut activity, Some([4; 32]), true, false);

        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "a different passing filter may satisfy NoTestsMatched but not filter B"
        );
    }

    #[test]
    fn filtered_reruns_cannot_weaken_legacy_full_suite_requirement() {
        let mut activity = vec![TurnActivitySummary::ProcessFinished {
            succeeded: true,
            output_bytes: 0,
        }];
        record_test(&mut activity, Some([3; 32]), false, false);
        record_test(&mut activity, Some([3; 32]), true, false);

        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "legacy process evidence requires a fresh full-suite pass"
        );

        record_test(&mut activity, None, true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Passed
        );

        activity.push(TurnActivitySummary::TextReplaced { bytes_written: 9 });
        record_test(&mut activity, Some([3; 32]), true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "filtered evidence after an edit cannot replace the legacy full-suite proof"
        );

        record_test(&mut activity, None, true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Passed
        );
    }

    #[test]
    fn failed_test_action_keeps_full_suite_requirement_after_filter_summary() {
        let mut activity = vec![TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::OfflineCargoTest,
        }];
        activity.push(TurnActivitySummary::OfflineCargoTestFinished {
            filter_hash: Some([2; 32]),
            passed: false,
            retry_any_filter: false,
        });
        record_test(&mut activity, Some([2; 32]), true, false);

        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Failed,
            "detailed filter metadata must not overwrite the conservative action failure"
        );

        record_test(&mut activity, None, true, false);
        assert_eq!(
            verification_for(&activity),
            ProcessTestVerificationStatus::Passed
        );
    }
}
