//! CLI-only verified planning, TaskSession evidence, and caller control.

use super::super::task_args::parse_task_action;
use super::MAX_TASK_ACTION_LINE_BYTES;
use crate::agent_loop::{
    BoundedDecisionSource, GoalTurnResult, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::brain::OllamaDecisionSource;
use crate::harness::execution::{
    create_impact_checked_plan, process_test_verification, review_existing_impact_plan,
    run_goal_turn_with_impact_checked_completion_correction,
    run_goal_turn_with_impact_checked_plan, stop_reason_after_test_verification,
    task_test_verification, ProcessTestVerificationStatus,
};
use crate::harness::verification::{
    reconcile_task_state_with_checks, verify_completion_checks, CompletionCheckEvidence,
    CompletionCheckEvidenceStatus, CompletionEvidenceSource, CompletionVerification,
    CompletionVerificationStatus, RequiredCompletionCheck,
};
use crate::task_lifecycle::TaskSession;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

pub(super) struct VerifiedTurnBatch {
    pub(super) turns: Vec<GoalTurnResult>,
    pub(super) verification: CompletionVerification,
    pub(super) correction_status: &'static str,
    pub(super) correction_attempted: bool,
    pub(super) stop_reason: GoalTurnStopReason,
    pub(super) actions_taken: usize,
    pub(super) test_verification: Option<ProcessTestVerificationStatus>,
}

pub(super) struct VerifiedTaskReport {
    pub(super) verification: CompletionVerification,
    pub(super) correction_status: &'static str,
    pub(super) stop_reason: GoalTurnStopReason,
    pub(super) test_verification: Option<ProcessTestVerificationStatus>,
}

#[allow(clippy::too_many_arguments)] // Session, IDs, goal, budget, checks, policy flags, and Brain/control sources are separate workflow inputs.
pub(super) fn run_verified_turn(
    session: &mut TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    goal: &str,
    action_budget: usize,
    verify_files: &[PathBuf],
    allow_correction: bool,
    pause_after_plan: bool,
    source: &mut OllamaDecisionSource,
    control: &mut VerifiedTaskControl<'_, '_>,
) -> io::Result<VerifiedTurnBatch> {
    let required_checks = verify_files
        .iter()
        .enumerate()
        .map(|(index, path)| RequiredCompletionCheck {
            id: format!("verify-file-{index}"),
            description: format!("Required file operation: {}", path.display()),
        })
        .collect::<Vec<_>>();
    let mut plan_goal = goal.to_owned();
    plan_goal.push_str("\n\nVerify these required output files by reading or producing them:");
    for path in verify_files {
        plan_goal.push_str("\n- ");
        plan_goal.push_str(&path.display().to_string());
    }
    let initial_plan = create_impact_checked_plan(
        session,
        task_id,
        turn_id,
        source,
        &plan_goal,
        &[],
        action_budget,
    );
    let (
        turns,
        verification,
        correction_status,
        correction_attempted,
        stop_reason,
        test_verification,
    ) = if allow_correction {
        let result = run_goal_turn_with_impact_checked_completion_correction(
            session,
            task_id,
            turn_id,
            goal,
            action_budget,
            Some(initial_plan),
            &required_checks,
            |_, result| verification_evidence(verify_files, result),
            |_,
             session,
             correction_task_id,
             correction_turn_id,
             process_failure,
             prior_result,
             verification,
             goal,
             remaining_actions| {
                let correction_budget = remaining_actions.saturating_sub(2);
                let plan = if let Some(failure) = process_failure {
                    source.create_process_failure_correction_plan(
                        session,
                        prior_result,
                        failure,
                        goal,
                        correction_budget,
                    )
                } else {
                    source
                        .create_completion_correction_plan(
                            session,
                            prior_result,
                            goal,
                            prior_result.observations(),
                            verification,
                            correction_budget,
                        )
                        .and_then(|plan| {
                            plan.ok_or(crate::brain::BrainReflectionError::InvalidPlan(
                                crate::brain::WorkspacePlanError::InvalidResponse,
                            ))
                        })
                }?;
                review_existing_impact_plan(
                    session,
                    correction_task_id,
                    correction_turn_id,
                    source,
                    goal,
                    prior_result.observations(),
                    remaining_actions,
                    plan,
                )
                .map_err(crate::brain::BrainReflectionError::InvalidPlan)
            },
            control,
        )
        .map_err(io::Error::other)?;
        let initial = result.initial_result();
        let correction_status = if result.correction_result().is_some() {
            "attempted once"
        } else if result.initial_verification().status == CompletionVerificationStatus::Passed {
            "not needed; checks passed"
        } else if pause_after_plan && initial.stop_reason() == GoalTurnStopReason::Continued {
            "deferred until checkpoint resume"
        } else if matches!(
            initial.stop_reason(),
            GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted
        ) {
            "skipped; insufficient remaining action budget"
        } else {
            "skipped; initial turn did not complete normally"
        };
        let correction_attempted = result.correction_result().is_some();
        let test_verification = result.test_verification();
        let stop_reason = result.final_stop_reason();
        let verification = if result.correction_result().is_none()
            && !matches!(
                initial.stop_reason(),
                GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted
            ) {
            verify_completion_checks(
                &required_checks,
                &verification_evidence(verify_files, initial),
            )
        } else {
            result.final_verification().clone()
        };
        let mut turns = vec![initial.clone()];
        if let Some(correction) = result.correction_result() {
            turns.push(correction.clone());
        }
        (
            turns,
            verification,
            correction_status,
            correction_attempted,
            stop_reason,
            test_verification,
        )
    } else {
        let initial_result = run_goal_turn_with_impact_checked_plan(
            session,
            task_id,
            turn_id,
            goal,
            action_budget,
            initial_plan,
            control,
        )
        .map_err(io::Error::other)?;
        let evidence = verification_evidence(verify_files, &initial_result);
        let current_test_verification = initial_result
            .observations()
            .iter()
            .any(|observation| matches!(observation, WorkspaceObservation::ProcessFinished { .. }))
            .then(|| process_test_verification(&initial_result, false, None));
        let test_verification = task_test_verification(session, task_id, current_test_verification);
        let stop_reason =
            stop_reason_after_test_verification(initial_result.stop_reason(), test_verification);
        let verification = reconcile_task_state_with_checks(
            session,
            task_id,
            stop_reason,
            &required_checks,
            &evidence,
        )
        .map_err(io::Error::other)?;
        let correction_status =
            if pause_after_plan && initial_result.stop_reason() == GoalTurnStopReason::Continued {
                "deferred until checkpoint resume; correction already used"
            } else {
                "not attempted; correction already used in this invocation"
            };
        (
            vec![initial_result],
            verification,
            correction_status,
            false,
            stop_reason,
            test_verification,
        )
    };
    let actions_taken = turns.iter().map(GoalTurnResult::actions_taken).sum();
    Ok(VerifiedTurnBatch {
        turns,
        verification,
        correction_status,
        correction_attempted,
        stop_reason,
        actions_taken,
        test_verification,
    })
}

fn verification_evidence(
    files: &[PathBuf],
    result: &GoalTurnResult,
) -> Vec<CompletionCheckEvidence> {
    files
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let mut passed = false;
            let mut failed = false;
            for observation in result.observations() {
                if observation_proves_file(observation, path) {
                    passed = true;
                } else if observation_failed_file(observation, path) {
                    failed = true;
                }
            }
            let (status, summary) = if failed {
                (
                    CompletionCheckEvidenceStatus::Failed,
                    format!(
                        "TaskSession recorded a failed file operation for {}",
                        path.display()
                    ),
                )
            } else if passed {
                (
                    CompletionCheckEvidenceStatus::Passed,
                    format!(
                        "TaskSession recorded a successful file operation for {}",
                        path.display()
                    ),
                )
            } else {
                (
                    CompletionCheckEvidenceStatus::Unknown,
                    format!(
                        "no TaskSession file evidence for {} in this turn",
                        path.display()
                    ),
                )
            };
            CompletionCheckEvidence {
                check_id: format!("verify-file-{index}"),
                status,
                source: CompletionEvidenceSource::TaskSessionObservation,
                summary,
            }
        })
        .collect()
}

fn observation_proves_file(observation: &WorkspaceObservation, expected: &Path) -> bool {
    match observation {
        WorkspaceObservation::FileRead { path, .. }
        | WorkspaceObservation::FileRangeRead { path, .. }
        | WorkspaceObservation::FileCreated { path, .. }
        | WorkspaceObservation::TextReplaced { path, .. }
        | WorkspaceObservation::PatchApplied { path, .. } => path == expected,
        _ => false,
    }
}

fn observation_failed_file(observation: &WorkspaceObservation, expected: &Path) -> bool {
    match observation {
        WorkspaceObservation::FileCreationFailed { path, .. }
        | WorkspaceObservation::TextReplacementFailed { path, .. }
        | WorkspaceObservation::PatchFailed { path, .. } => path == expected,
        WorkspaceObservation::ActionFailed { action, .. } => {
            workspace_action_file_path(action).is_some_and(|path| path == expected)
        }
        _ => false,
    }
}

fn workspace_action_file_path(action: &WorkspaceAction) -> Option<&Path> {
    match action {
        WorkspaceAction::ReadFile { path }
        | WorkspaceAction::ReadRange { path, .. }
        | WorkspaceAction::CreateFile { path, .. }
        | WorkspaceAction::ReplaceText { path, .. }
        | WorkspaceAction::ApplyPatch { path, .. } => Some(path),
        _ => None,
    }
}

pub(super) fn normalize_verification_path(value: &str) -> Result<PathBuf, &'static str> {
    if value.is_empty() {
        return Err("verification file path must not be empty");
    }
    if value.len() > 192 {
        return Err("verification file path must not exceed 192 UTF-8 bytes");
    }
    if value.chars().any(char::is_control) {
        return Err("verification file path must not contain control characters");
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err("verification file path must be relative to the task workspace");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            _ => return Err("verification file path must not escape the task workspace"),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err("verification file path must name a file inside the task workspace");
    }
    Ok(normalized)
}

pub(super) struct VerifiedTaskControl<'a, 'input> {
    pub(super) reader: &'a mut BufReader<&'input mut dyn Read>,
    pub(super) line: &'a mut Vec<u8>,
    pub(super) error: &'a mut dyn Write,
}

impl BoundedDecisionSource for VerifiedTaskControl<'_, '_> {
    fn choose_action(
        &mut self,
        _goal: &str,
        _observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        if write!(
            self.error,
            "verified plan finished; enter complete [result], continue, or cancel: "
        )
        .and_then(|()| self.error.flush())
        .is_err()
        {
            return WorkspaceAction::Unsupported {
                action: "unable to display verified task control prompt".to_owned(),
            };
        }
        self.line.clear();
        match self
            .reader
            .take(MAX_TASK_ACTION_LINE_BYTES as u64 + 1)
            .read_until(b'\n', self.line)
        {
            Ok(0) => WorkspaceAction::Unsupported {
                action: "end of input before verified task control".to_owned(),
            },
            Ok(bytes_read) if bytes_read > MAX_TASK_ACTION_LINE_BYTES => {
                WorkspaceAction::Unsupported {
                    action: format!(
                        "verified task control line exceeds limit of {MAX_TASK_ACTION_LINE_BYTES} bytes"
                    ),
                }
            }
            Ok(_) => match std::str::from_utf8(self.line) {
                Ok(line) => match parse_task_action(line) {
                    action @ (WorkspaceAction::Complete { .. }
                    | WorkspaceAction::Continue
                    | WorkspaceAction::Cancel) => action,
                    _ => WorkspaceAction::Unsupported {
                        action: "verified control must be complete, continue, or cancel".to_owned(),
                    },
                },
                Err(error) => WorkspaceAction::Unsupported {
                    action: format!("verified control input is not valid UTF-8: {error}"),
                },
            },
            Err(error) => WorkspaceAction::Unsupported {
                action: format!("unable to read verified task control: {error}"),
            },
        }
    }
}
