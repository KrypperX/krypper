use super::{
    run_goal_turn_with_budget, workspace_action_name, workspace_mutation_target,
    BoundedDecisionSource, GoalTurnError, GoalTurnResult, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::brain::{OllamaDecisionSource, WorkspacePlan, WorkspacePlanError};
use crate::harness::verification::{
    reconcile_task_state_with_checks, verify_completion_checks, CompletionCheckEvidence,
    CompletionVerification, CompletionVerificationStatus, RequiredCompletionCheck,
};
use crate::task_lifecycle::{TaskSession, TurnState};
use crate::tools::registry::workspace_operations;
use crate::{ProcessOutcome, ProcessResult};
use std::collections::BTreeSet;
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
        return decision_source.accept_plan_with_project_guidance(
            proposal,
            action_budget,
            root_guidance,
        );
    }

    let proposed_guidance = session
        .read_project_guidance_for_targets(task_id, &proposal_targets)
        .map_err(WorkspacePlanError::RootGuidanceRead)?;
    if proposed_guidance == root_guidance {
        return decision_source.accept_plan_with_project_guidance(
            proposal,
            action_budget,
            proposed_guidance,
        );
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

    decision_source.accept_plan_with_project_guidance(final_plan, action_budget, final_guidance)
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
    let operations = match plan {
        Ok(plan) => {
            let targets = plan_mutation_targets(&plan);
            if !targets.is_empty() {
                let current_guidance = session
                    .read_project_guidance_for_targets(task_id, &targets)
                    .map_err(GoalTurnError::RootProjectGuidance)?;
                let expected_guidance = plan.project_guidance_context();
                if current_guidance.as_deref() != expected_guidance {
                    return Err(GoalTurnError::RootProjectGuidance(
                        crate::task_lifecycle::WorkspaceGuidanceReadError::GuidanceChanged,
                    ));
                }
            }
            plan.into_actions()
        }
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

const MIN_PROCESS_CORRECTION_ACTIONS: usize = 4;

/// Runs one task turn and permits a single correction for a complete offline Cargo failure.
///
/// A correction is eligible only when the last observation is a complete, cleanup-confirmed
/// Cargo nonzero exit or a typed zero-match result. It runs as a new turn in the same task and
/// receives only the original action-budget slots left after the first attempt. A later test
/// observation verifies tests only when it is complete, successful, ran at least one test, and
/// followed the last workspace mutation. Existing independent completion checks remain required
/// for task success.
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
        let test_verification = process_test_verification(&initial_result, false);
        let final_verification = reconcile_task_state_with_checks(
            session,
            task_id,
            initial_result.stop_reason(),
            required_checks,
            &initial_evidence,
        )
        .map_err(ProcessFailureCorrectionError::Session)?;
        return Ok(ProcessFailureCorrectionResult {
            initial_result,
            initial_verification,
            correction_result: None,
            final_verification,
            test_verification,
        });
    };

    let remaining_actions = max_actions.saturating_sub(initial_result.actions_taken());
    if remaining_actions < MIN_PROCESS_CORRECTION_ACTIONS {
        let final_verification = reconcile_task_state_with_checks(
            session,
            task_id,
            initial_result.stop_reason(),
            required_checks,
            &initial_evidence,
        )
        .map_err(ProcessFailureCorrectionError::Session)?;
        return Ok(ProcessFailureCorrectionResult {
            initial_result,
            initial_verification,
            correction_result: None,
            final_verification,
            test_verification: ProcessTestVerificationStatus::Failed,
        });
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
    let correction_turn_id =
        create_follow_up_turn(session, task_id).map_err(ProcessFailureCorrectionError::Session)?;
    let correction_result = run_goal_turn_with_plan_result(
        session,
        task_id,
        correction_turn_id,
        goal,
        remaining_actions,
        plan,
        decision_source,
    )
    .map_err(ProcessFailureCorrectionError::Execution)?;
    let test_verification = process_test_verification(&correction_result, true);
    let final_evidence = check_evidence(session, &correction_result);
    let final_stop_reason = if test_verification == ProcessTestVerificationStatus::Passed {
        correction_result.stop_reason()
    } else {
        GoalTurnStopReason::ActionFailed
    };
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

fn process_test_verification(
    result: &GoalTurnResult,
    required_after_correction: bool,
) -> ProcessTestVerificationStatus {
    if successful_test_follows_workspace_changes(result) {
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

fn successful_test_follows_workspace_changes(result: &GoalTurnResult) -> bool {
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
    ran_complete_test && !later_workspace_change
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
        WorkspaceAction::OfflineCargoTest { .. }
            | WorkspaceAction::ReadFile { .. }
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

#[cfg(test)]
mod tests {
    use super::{run_goal_turn_with_process_failure_correction, ProcessTestVerificationStatus};
    use crate::brain::{WorkspacePlan, WorkspacePlanError};
    use crate::harness::execution::{
        BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    use crate::harness::verification::{
        CompletionCheckEvidence, CompletionCheckEvidenceStatus, CompletionEvidenceSource,
        CompletionVerificationStatus, RequiredCompletionCheck,
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
        workspace: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("krypper-process-correction-{}", Uuid::new_v4()));
            let workspace = root.join("workspace");
            fs::create_dir_all(&workspace).expect("create fixture workspace");
            Self { root, workspace }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove fixture workspace");
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
