// Shared by several test and example targets; each target uses a subset.
#![allow(dead_code)]

use super::harness::{
    Config, ExecutionMode, FixtureClass, FixtureRequest, FixtureResult, ToolGrant,
    VerificationObservation,
};
use super::support::TempRoot;
use krypper::agent_loop::{
    run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use krypper::harness::execution::{
    run_goal_turn_with_completion_correction, run_goal_turn_with_plan, CompletionCorrectionResult,
    GoalTurnResult,
};
use krypper::harness::verification::{
    reconcile_task_state_with_checks, CompletionCheckEvidence, CompletionCheckEvidenceStatus,
    CompletionEvidenceSource, CompletionVerificationStatus, RequiredCompletionCheck,
    MAX_COMPLETION_CHECKS,
};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnState};
use krypper::WorkspacePlan;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const BRIEF: &str = "brief.txt";
const SUMMARY: &str = "summary.txt";
const SOURCE: &str = "source.txt";
const BUGGY_LINE: &str = "fn value() -> usize { 1 }";
const FIXED_LINE: &str = "fn value() -> usize { 2 }";
const FIRST_CHECKPOINT_FILE: &str = "first-turn.txt";
const SECOND_CHECKPOINT_FILE: &str = "second-turn.txt";

struct Evaluation {
    success: bool,
    expected: String,
    verification: VerificationObservation,
}

pub fn run(request: &FixtureRequest) -> Result<Vec<FixtureResult>, String> {
    request.config.validate()?;
    request
        .fixtures
        .iter()
        .map(|class| run_class(*class, request))
        .collect()
}

fn run_class(class: FixtureClass, request: &FixtureRequest) -> Result<FixtureResult, String> {
    let mut latencies = Vec::new();
    let mut effects = Vec::new();
    let mut last = None;
    let mut attempts = 0;

    for attempt in 1..=request.config.max_attempts {
        attempts = attempt;
        let started = Instant::now();
        let root = TempRoot::new(class.as_str())?;
        let (result, attempt_effects) = run_attempt(class, request, attempt, root.path())?;
        latencies.push(started.elapsed().as_millis() as u64);
        effects.extend(attempt_effects);
        let retryable = matches!(
            class,
            FixtureClass::Timeout | FixtureClass::MalformedProviderOutput
        ) && attempt < request.config.max_attempts;
        last = Some(result);
        if !retryable {
            break;
        }
    }

    let mut result = last.ok_or_else(|| "fixture made no attempt".to_owned())?;
    result.attempts = attempts;
    result.retries = attempts - 1;
    result.run = request.run;
    result.latencies_ms = latencies;
    result.duplicate_effects = duplicate_effect_count(&effects);
    Ok(result)
}

fn run_attempt(
    class: FixtureClass,
    request: &FixtureRequest,
    attempt: usize,
    root: &Path,
) -> Result<(FixtureResult, Vec<String>), String> {
    require_grants(&request.config, required_grants(class))?;
    let secret = request
        .config
        .secrets
        .first()
        .ok_or_else(|| "fixture config requires one configured secret".to_owned())?;
    let workspace = setup_workspace(root, secret)?;
    let outside = root.join("outside.txt");
    let (mut session, task_id, turn_id) = running_session(&workspace)?;

    let (observations, actions, stop_reason, evaluation, corrections) = match class {
        FixtureClass::CheckpointRestart => run_checkpoint_restart(&workspace)?,
        FixtureClass::ReflectionCorrection => {
            run_reflection_correction(&mut session, task_id, turn_id, &workspace, request)?
        }
        _ => {
            let actions = actions_for(class, secret, attempt);
            let result = run_actions(
                &mut session,
                task_id,
                turn_id,
                class,
                request,
                actions.clone(),
            )?;
            let evaluation = evaluate(class, &mut session, &workspace, &result, request, attempt)?;
            (
                result.observations().to_vec(),
                result.actions_taken(),
                result.stop_reason(),
                evaluation,
                0,
            )
        }
    };

    let observed_stop = format!("{stop_reason:?}");
    let outside_write = !canary_intact(&outside);
    let denied = denial_count(&observations);
    let effects = effect_signatures(&observations);
    let mut raw_evidence = vec![
        format!("class={}", class.as_str()),
        format!("arm={:?}", request.arm),
        format!("run={}", request.run),
        format!("attempt={attempt}"),
        format!("mode={:?}", request.config.mode),
        format!("runtime={:?}", request.config.runtime),
        format!("provider={:?}", request.config.provider),
        format!("model={:?}", request.config.model),
        format!("prompt_policy={:?}", request.config.prompt_policy),
        format!("fixture_revision={}", request.fixture_revision),
        format!("stop={observed_stop}"),
        format!("actions={actions}"),
        format!("denied={denied}"),
        format!("outside_write={outside_write}"),
        format!("verification={:?}", evaluation.verification),
        format!("scripted_outcome={}", class.as_str()),
        format!("redaction_probe={secret}"),
    ];
    if evaluation.verification != VerificationObservation::NotApplicable {
        raw_evidence.push(format!("verification_status={:?}", evaluation.verification));
    }
    let (evidence, redactions) = redact_evidence(&raw_evidence, &request.config);
    let secret_leaks = evidence
        .iter()
        .map(|line| {
            request
                .config
                .secrets
                .iter()
                .filter(|secret| line.contains(secret.as_str()))
                .count() as u64
        })
        .sum();
    let safety_incidents = u64::from(outside_write) + secret_leaks;
    let success = evaluation.success && safety_incidents == 0;
    let cost_units = (actions as u64).saturating_mul(request.config.cost_per_action);

    let result = FixtureResult {
        id: class,
        run: request.run,
        success,
        expected: evaluation.expected,
        observed_stop_reason: observed_stop,
        verification: evaluation.verification,
        scripted_outcome: class.as_str().to_owned(),
        actions,
        cost_units,
        tokens: 0,
        denied_operations: denied,
        redactions,
        secret_leaks,
        outside_write,
        safety_incidents,
        duplicate_effects: 0,
        attempts: 1,
        retries: 0,
        corrections,
        latencies_ms: Vec::new(),
        evidence,
    };
    Ok((result, effects))
}

fn required_grants(class: FixtureClass) -> &'static [ToolGrant] {
    match class {
        FixtureClass::ArtifactCreation | FixtureClass::CrossFileEdit => {
            &[ToolGrant::ReadFile, ToolGrant::CreateFile]
        }
        FixtureClass::BugFix | FixtureClass::FailedTest | FixtureClass::SandboxUnavailable => {
            &[ToolGrant::ReadFile, ToolGrant::ReplaceText]
        }
        FixtureClass::InterruptedExecution => &[ToolGrant::CreateFile, ToolGrant::Cancel],
        FixtureClass::DeniedOperation => &[ToolGrant::CreateFile],
        FixtureClass::Timeout | FixtureClass::MalformedProviderOutput => {
            &[ToolGrant::CreateFile, ToolGrant::ReadFile]
        }
        FixtureClass::TruncatedEvidence => &[ToolGrant::CreateFile, ToolGrant::ReadFile],
        FixtureClass::Cancellation => &[ToolGrant::Cancel],
        FixtureClass::CheckpointRestart => &[ToolGrant::CreateFile, ToolGrant::Checkpoint],
        FixtureClass::ReflectionCorrection => &[ToolGrant::CreateFile, ToolGrant::ReadFile],
    }
}

fn require_grants(config: &Config, required: &[ToolGrant]) -> Result<(), String> {
    for grant in required {
        if !config.tool_grants.contains(grant) {
            return Err(format!("fixture requires the {grant:?} tool grant"));
        }
    }
    Ok(())
}

fn setup_workspace(root: &Path, secret: &str) -> Result<PathBuf, String> {
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).map_err(|error| format!("create workspace: {error}"))?;
    fs::write(root.join("outside.txt"), "canary")
        .map_err(|error| format!("write canary: {error}"))?;
    fs::write(
        workspace.join(BRIEF),
        format!("public line\nfixture secret: {secret}\n"),
    )
    .map_err(|error| format!("write brief: {error}"))?;
    fs::write(workspace.join(SOURCE), format!("{BUGGY_LINE}\n"))
        .map_err(|error| format!("write source: {error}"))?;
    Ok(workspace)
}

fn running_session(workspace: &Path) -> Result<(TaskSession, uuid::Uuid, uuid::Uuid), String> {
    let mut session = TaskSession::new_in_workspace(workspace)
        .map_err(|error| format!("bind workspace: {error}"))?;
    let task_id = session.create_task("bounded fixture task").id();
    let task = session
        .get_task_mut(task_id)
        .ok_or_else(|| "created task is missing".to_owned())?;
    task.transition(TaskState::Running)
        .map_err(|error| format!("start task: {error}"))?;
    let turn_id = task
        .create_turn()
        .map_err(|error| format!("create turn: {error}"))?;
    task.transition_turn(turn_id, TurnState::Running)
        .map_err(|error| format!("start turn: {error}"))?;
    Ok((session, task_id, turn_id))
}

fn completed_action() -> WorkspaceAction {
    WorkspaceAction::Complete {
        result: "scripted completion".to_owned(),
    }
}

fn script_exhausted() -> WorkspaceAction {
    WorkspaceAction::Unsupported {
        action: "fixture_script_exhausted".to_owned(),
    }
}

fn artifact_success_action(secret: &str) -> WorkspaceAction {
    WorkspaceAction::Complete {
        result: format!("secret probe: {secret}"),
    }
}

fn actions_for(class: FixtureClass, secret: &str, attempt: usize) -> Vec<WorkspaceAction> {
    match class {
        FixtureClass::ArtifactCreation => vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from(BRIEF),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from(SUMMARY),
                contents: "public line\n".to_owned(),
            },
            artifact_success_action(secret),
        ],
        FixtureClass::BugFix => vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from(SOURCE),
            },
            WorkspaceAction::ReplaceText {
                path: PathBuf::from(SOURCE),
                match_text: BUGGY_LINE.to_owned(),
                replacement_text: FIXED_LINE.to_owned(),
            },
            artifact_success_action(secret),
        ],
        FixtureClass::CrossFileEdit => vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from(BRIEF),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("left.txt"),
                contents: "left public line\n".to_owned(),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("right.txt"),
                contents: "right public line\n".to_owned(),
            },
            artifact_success_action(secret),
        ],
        FixtureClass::FailedTest | FixtureClass::SandboxUnavailable => vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from(BRIEF),
            },
            WorkspaceAction::Unsupported {
                action: class.as_str().to_owned(),
            },
        ],
        FixtureClass::InterruptedExecution => vec![
            WorkspaceAction::CreateFile {
                path: PathBuf::from("partial.txt"),
                contents: "partial public line\n".to_owned(),
            },
            WorkspaceAction::Cancel,
        ],
        FixtureClass::DeniedOperation => vec![WorkspaceAction::CreateFile {
            path: PathBuf::from("../outside.txt"),
            contents: secret.to_owned(),
        }],
        FixtureClass::Timeout | FixtureClass::MalformedProviderOutput => {
            if attempt == 1 {
                vec![WorkspaceAction::Unsupported {
                    action: class.as_str().to_owned(),
                }]
            } else {
                vec![
                    WorkspaceAction::CreateFile {
                        path: PathBuf::from(SUMMARY),
                        contents: "recovered public line\n".to_owned(),
                    },
                    artifact_success_action(secret),
                ]
            }
        }
        FixtureClass::TruncatedEvidence => vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from(BRIEF),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from(SUMMARY),
                contents: "public line\n".to_owned(),
            },
            artifact_success_action(secret),
        ],
        FixtureClass::Cancellation => vec![WorkspaceAction::Cancel],
        FixtureClass::CheckpointRestart | FixtureClass::ReflectionCorrection => Vec::new(),
    }
}

fn run_actions(
    session: &mut TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    class: FixtureClass,
    request: &FixtureRequest,
    actions: Vec<WorkspaceAction>,
) -> Result<GoalTurnResult, String> {
    let can_plan = request.config.mode == ExecutionMode::Planned
        && !actions.is_empty()
        && actions.iter().all(|action| {
            !matches!(
                action,
                WorkspaceAction::Cancel | WorkspaceAction::Unsupported { .. }
            )
        });
    if can_plan {
        let operations = actions
            .iter()
            .filter(|action| !matches!(action, WorkspaceAction::Complete { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let control_action = actions.last().cloned().unwrap_or_else(completed_action);
        let plan = WorkspacePlan::try_from_actions(operations.clone(), operations.len())
            .map_err(|error| format!("build fixture plan: {error}"))?;
        let mut control = |_: &str, _: &[WorkspaceObservation]| control_action.clone();
        run_goal_turn_with_plan(
            session,
            task_id,
            turn_id,
            class.as_str(),
            request.config.action_budget,
            Ok(plan),
            &mut control,
        )
        .map_err(|error| format!("run planned fixture: {error}"))
    } else {
        let mut pending = actions.into_iter();
        let mut decision =
            |_: &str, _: &[WorkspaceObservation]| pending.next().unwrap_or_else(script_exhausted);
        run_goal_turn_with_budget(
            session,
            task_id,
            turn_id,
            class.as_str(),
            request.config.action_budget,
            &mut decision,
        )
        .map_err(|error| format!("run scripted fixture: {error}"))
    }
}

fn run_checkpoint_restart(
    workspace: &Path,
) -> Result<
    (
        Vec<WorkspaceObservation>,
        usize,
        GoalTurnStopReason,
        Evaluation,
        u64,
    ),
    String,
> {
    let (mut session, task_id, first_turn) = running_session(workspace)?;
    let first = run_turn_actions(
        &mut session,
        task_id,
        first_turn,
        vec![
            WorkspaceAction::CreateFile {
                path: PathBuf::from(FIRST_CHECKPOINT_FILE),
                contents: "first public line\n".to_owned(),
            },
            completed_action(),
        ],
        3,
    )?;
    let checkpoint = session
        .export_checkpoint()
        .map_err(|error| format!("export checkpoint: {error}"))?;
    drop(session);

    let mut restored = TaskSession::restore_checkpoint_in_workspace(&checkpoint, workspace)
        .map_err(|error| format!("restore checkpoint: {error}"))?;
    let task = restored
        .get_task_mut(task_id)
        .ok_or_else(|| "restored task is missing".to_owned())?;
    let second_turn = task
        .create_turn()
        .map_err(|error| format!("create restored turn: {error}"))?;
    task.transition_turn(second_turn, TurnState::Running)
        .map_err(|error| format!("start restored turn: {error}"))?;
    let second = run_turn_actions(
        &mut restored,
        task_id,
        second_turn,
        vec![
            WorkspaceAction::CreateFile {
                path: PathBuf::from(SECOND_CHECKPOINT_FILE),
                contents: "second public line\n".to_owned(),
            },
            completed_action(),
        ],
        3,
    )?;
    let checks = [RequiredCompletionCheck {
        id: "checkpoint_restart".to_owned(),
        description: "both checkpoint turns left their public artifacts".to_owned(),
    }];
    let evidence = [CompletionCheckEvidence {
        check_id: "checkpoint_restart".to_owned(),
        status: if workspace.join(FIRST_CHECKPOINT_FILE).is_file()
            && workspace.join(SECOND_CHECKPOINT_FILE).is_file()
        {
            CompletionCheckEvidenceStatus::Passed
        } else {
            CompletionCheckEvidenceStatus::Failed
        },
        source: CompletionEvidenceSource::DeterministicLocalCheck,
        summary: "both checkpoint turn artifacts are present".to_owned(),
    }];
    let verification = reconcile_task_state_with_checks(
        &mut restored,
        task_id,
        second.stop_reason(),
        &checks,
        &evidence,
    )
    .map_err(|error| format!("verify checkpoint fixture: {error}"))?;
    let success = verification.status == CompletionVerificationStatus::Passed
        && second.stop_reason() == GoalTurnStopReason::Completed;
    let mut observations = first.observations().to_vec();
    observations.extend(second.observations().iter().cloned());
    Ok((
        observations,
        first.actions_taken() + second.actions_taken(),
        second.stop_reason(),
        Evaluation {
            success,
            expected: "stop=Completed after checkpoint restore with both artifacts".to_owned(),
            verification: verification_observation(verification.status),
        },
        0,
    ))
}

fn run_reflection_correction(
    session: &mut TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    workspace: &Path,
    request: &FixtureRequest,
) -> Result<
    (
        Vec<WorkspaceObservation>,
        usize,
        GoalTurnStopReason,
        Evaluation,
        u64,
    ),
    String,
> {
    let checks = [RequiredCompletionCheck {
        id: "reflection_correction".to_owned(),
        description: "the corrected turn created its public artifact".to_owned(),
    }];
    let check_workspace = workspace.to_path_buf();
    let check_evidence = |_: &TaskSession, result: &GoalTurnResult| {
        let passed = result.stop_reason() == GoalTurnStopReason::Completed
            && check_workspace.join(SUMMARY).is_file();
        vec![CompletionCheckEvidence {
            check_id: "reflection_correction".to_owned(),
            status: if passed {
                CompletionCheckEvidenceStatus::Passed
            } else {
                CompletionCheckEvidenceStatus::Failed
            },
            source: CompletionEvidenceSource::DeterministicLocalCheck,
            summary: "the corrected artifact exists after a completed turn".to_owned(),
        }]
    };
    let correction_workspace = workspace.to_path_buf();
    let correction_plan =
        |_: &mut _, _: &TaskSession, _: &GoalTurnResult, _: &_, _: &str, remaining: usize| {
            let _ = remaining;
            WorkspacePlan::try_from_actions(
                vec![WorkspaceAction::CreateFile {
                    path: PathBuf::from(SUMMARY),
                    contents: "corrected public line\n".to_owned(),
                }],
                1,
            )
        };
    let mut decision = |_: &str, _: &[WorkspaceObservation]| completed_action();
    let result: CompletionCorrectionResult = run_goal_turn_with_completion_correction(
        session,
        task_id,
        turn_id,
        FixtureClass::ReflectionCorrection.as_str(),
        request.config.action_budget,
        None,
        &checks,
        check_evidence,
        correction_plan,
        &mut decision,
    )
    .map_err(|error| format!("run reflection correction fixture: {error}"))?;
    let correction = result.correction_result();
    let success = correction.is_some()
        && result.final_verification().status == CompletionVerificationStatus::Passed
        && correction_workspace.join(SUMMARY).is_file();
    let mut observations = result.initial_result().observations().to_vec();
    if let Some(correction) = correction {
        observations.extend(correction.observations().iter().cloned());
    }
    let actions = result.initial_result().actions_taken()
        + correction.map_or(0, GoalTurnResult::actions_taken);
    let stop_reason = correction
        .map(GoalTurnResult::stop_reason)
        .unwrap_or_else(|| result.initial_result().stop_reason());
    Ok((
        observations,
        actions,
        stop_reason,
        Evaluation {
            success,
            expected: "one correction after failed checks with passing final evidence".to_owned(),
            verification: verification_observation(result.final_verification().status),
        },
        u64::from(correction.is_some()),
    ))
}

fn run_turn_actions(
    session: &mut TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    actions: Vec<WorkspaceAction>,
    budget: usize,
) -> Result<GoalTurnResult, String> {
    let mut pending = actions.into_iter();
    let mut decision =
        |_: &str, _: &[WorkspaceObservation]| pending.next().unwrap_or_else(script_exhausted);
    run_goal_turn_with_budget(
        session,
        task_id,
        turn_id,
        "checkpoint fixture",
        budget,
        &mut decision,
    )
    .map_err(|error| format!("run checkpoint turn: {error}"))
}

fn evaluate(
    class: FixtureClass,
    session: &mut TaskSession,
    workspace: &Path,
    result: &GoalTurnResult,
    request: &FixtureRequest,
    attempt: usize,
) -> Result<Evaluation, String> {
    let observed = result.stop_reason();
    let artifact_ok = |path: &Path, needle: &str| {
        fs::read_to_string(path)
            .map(|value| value.contains(needle) && !value.contains(&request.config.secrets[0]))
            .unwrap_or(false)
    };
    let evaluation = match class {
        FixtureClass::ArtifactCreation => {
            let passed = observed == GoalTurnStopReason::Completed
                && artifact_ok(&workspace.join(SUMMARY), "public line");
            let checks = [RequiredCompletionCheck {
                id: "artifact_exists".to_owned(),
                description: "summary.txt contains the public fixture line".to_owned(),
            }];
            let evidence = [CompletionCheckEvidence {
                check_id: "artifact_exists".to_owned(),
                status: if passed {
                    CompletionCheckEvidenceStatus::Passed
                } else {
                    CompletionCheckEvidenceStatus::Failed
                },
                source: CompletionEvidenceSource::DeterministicLocalCheck,
                summary: "summary.txt contains the public fixture line".to_owned(),
            }];
            let verification = reconcile_task_state_with_checks(
                session,
                result.task_id(),
                observed,
                &checks,
                &evidence,
            );
            match verification {
                Ok(verification) => Evaluation {
                    success: passed && verification.status == CompletionVerificationStatus::Passed,
                    expected: "stop=Completed with an independently checked artifact".to_owned(),
                    verification: verification_observation(verification.status),
                },
                Err(error) => {
                    return Err(format!("verify artifact fixture: {error}"));
                }
            }
        }
        FixtureClass::BugFix => {
            let source = workspace.join(SOURCE);
            let fixed = fs::read_to_string(&source)
                .map(|value| value.contains(FIXED_LINE) && !value.contains(BUGGY_LINE))
                .unwrap_or(false);
            Evaluation {
                success: observed == GoalTurnStopReason::Completed && fixed,
                expected: "stop=Completed with one independently checked bug fix".to_owned(),
                verification: if fixed {
                    VerificationObservation::Passed
                } else {
                    VerificationObservation::Failed
                },
            }
        }
        FixtureClass::CrossFileEdit => {
            let left = artifact_ok(&workspace.join("left.txt"), "left public line");
            let right = artifact_ok(&workspace.join("right.txt"), "right public line");
            Evaluation {
                success: observed == GoalTurnStopReason::Completed && left && right,
                expected: "stop=Completed with both independent cross-file artifacts".to_owned(),
                verification: if left && right {
                    VerificationObservation::Passed
                } else {
                    VerificationObservation::Failed
                },
            }
        }
        FixtureClass::FailedTest | FixtureClass::SandboxUnavailable => Evaluation {
            success: observed == GoalTurnStopReason::ActionFailed
                && !workspace.join(SUMMARY).exists(),
            expected: "stop=ActionFailed with no workspace mutation".to_owned(),
            verification: VerificationObservation::NotApplicable,
        },
        FixtureClass::InterruptedExecution => Evaluation {
            success: observed == GoalTurnStopReason::Cancelled
                && workspace.join("partial.txt").is_file()
                && !workspace.join(SUMMARY).exists(),
            expected: "stop=Cancelled after one visible partial mutation".to_owned(),
            verification: VerificationObservation::NotApplicable,
        },
        FixtureClass::DeniedOperation => Evaluation {
            success: observed == GoalTurnStopReason::ActionFailed
                && denial_count(result.observations()) >= 1
                && !workspace.join("later.txt").exists(),
            expected: "stop=ActionFailed with the outside canary unchanged".to_owned(),
            verification: VerificationObservation::NotApplicable,
        },
        FixtureClass::Timeout | FixtureClass::MalformedProviderOutput => {
            if attempt == 1 {
                Evaluation {
                    success: observed == GoalTurnStopReason::ActionFailed,
                    expected: "first scripted failure is retried".to_owned(),
                    verification: VerificationObservation::NotApplicable,
                }
            } else {
                Evaluation {
                    success: observed == GoalTurnStopReason::Completed
                        && artifact_ok(&workspace.join(SUMMARY), "recovered public line"),
                    expected: "retry reaches a checked completion".to_owned(),
                    verification: if workspace.join(SUMMARY).is_file() {
                        VerificationObservation::Passed
                    } else {
                        VerificationObservation::Failed
                    },
                }
            }
        }
        FixtureClass::TruncatedEvidence => {
            return Ok(evaluate_truncated_evidence(
                session,
                result.task_id(),
                observed,
                workspace,
            ));
        }
        FixtureClass::Cancellation => Evaluation {
            success: observed == GoalTurnStopReason::Cancelled,
            expected: "stop=Cancelled before any workspace mutation".to_owned(),
            verification: VerificationObservation::NotApplicable,
        },
        FixtureClass::CheckpointRestart | FixtureClass::ReflectionCorrection => {
            return Err("custom fixture class must not use generic evaluation".to_owned());
        }
    };
    Ok(evaluation)
}

fn evaluate_truncated_evidence(
    session: &mut TaskSession,
    task_id: uuid::Uuid,
    stop_reason: GoalTurnStopReason,
    workspace: &Path,
) -> Evaluation {
    let checks = [RequiredCompletionCheck {
        id: "truncated_evidence".to_owned(),
        description: "the completed turn has bounded independent evidence".to_owned(),
    }];
    let evidence = (0..=MAX_COMPLETION_CHECKS)
        .map(|_| CompletionCheckEvidence {
            check_id: "truncated_evidence".to_owned(),
            status: CompletionCheckEvidenceStatus::Passed,
            source: CompletionEvidenceSource::DeterministicLocalCheck,
            summary: "fixture pass".to_owned(),
        })
        .collect::<Vec<_>>();
    let verification =
        reconcile_task_state_with_checks(session, task_id, stop_reason, &checks, &evidence)
            .expect("verify truncated fixture evidence");
    Evaluation {
        success: verification.status == CompletionVerificationStatus::Unknown
            && workspace.join(SUMMARY).is_file(),
        expected: "truncated evidence cannot establish completion".to_owned(),
        verification: VerificationObservation::Unknown,
    }
}

fn verification_observation(status: CompletionVerificationStatus) -> VerificationObservation {
    match status {
        CompletionVerificationStatus::Passed => VerificationObservation::Passed,
        CompletionVerificationStatus::Failed => VerificationObservation::Failed,
        CompletionVerificationStatus::Unknown => VerificationObservation::Unknown,
    }
}

fn canary_intact(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|value| value == "canary")
        .unwrap_or(false)
}

fn denial_count(observations: &[WorkspaceObservation]) -> u64 {
    observations
        .iter()
        .filter(|observation| {
            matches!(
                observation,
                WorkspaceObservation::FileCreationFailed { .. }
                    | WorkspaceObservation::DirectoryCreationFailed { .. }
                    | WorkspaceObservation::TextReplacementFailed { .. }
                    | WorkspaceObservation::PatchFailed { .. }
                    | WorkspaceObservation::TextSearchFailed { .. }
                    | WorkspaceObservation::TreeTextSearchFailed { .. }
                    | WorkspaceObservation::ActionFailed { .. }
            )
        })
        .count() as u64
}

pub fn effect_signatures(observations: &[WorkspaceObservation]) -> Vec<String> {
    observations
        .iter()
        .filter_map(|observation| match observation {
            WorkspaceObservation::FileCreated { path, .. } => {
                Some(format!("file_created:{}", path.display()))
            }
            WorkspaceObservation::DirectoryCreated { path } => {
                Some(format!("directory_created:{}", path.display()))
            }
            WorkspaceObservation::TextReplaced { path, .. } => {
                Some(format!("text_replaced:{}", path.display()))
            }
            WorkspaceObservation::PatchApplied { path, .. } => {
                Some(format!("patch_applied:{}", path.display()))
            }
            WorkspaceObservation::ProcessFinished { result } => {
                Some(format!("process:{:?}", result.outcome))
            }
            WorkspaceObservation::ActionFailed { action, .. } => {
                Some(format!("action_failed:{}", action_name(action)))
            }
            _ => None,
        })
        .collect()
}

pub fn duplicate_effect_count(signatures: &[String]) -> u64 {
    let mut seen = BTreeSet::new();
    let mut duplicates = 0;
    for signature in signatures {
        if !seen.insert(signature.as_str()) {
            duplicates += 1;
        }
    }
    duplicates
}

fn action_name(action: &WorkspaceAction) -> &'static str {
    match action {
        WorkspaceAction::OfflineCargoTest { .. } => "offline_cargo_test",
        WorkspaceAction::OfflineReadOnlyExec { .. } => "offline_read_only_exec",
        WorkspaceAction::ReadFile { .. } => "read_file",
        WorkspaceAction::ReadRange { .. } => "read_range",
        WorkspaceAction::ListDirectory { .. } => "list_directory",
        WorkspaceAction::CreateDirectory { .. } => "create_directory",
        WorkspaceAction::SearchText { .. } => "search_text",
        WorkspaceAction::SearchTreeText { .. } => "search_tree_text",
        WorkspaceAction::CreateFile { .. } => "create_file",
        WorkspaceAction::ReplaceText { .. } => "replace_text",
        WorkspaceAction::ApplyPatch { .. } => "apply_patch",
        WorkspaceAction::Complete { .. } => "complete_task",
        WorkspaceAction::Continue => "continue",
        WorkspaceAction::Cancel => "cancel",
        WorkspaceAction::Extension { .. } => "extension",
        WorkspaceAction::Unsupported { .. } => "unsupported",
    }
}

fn redact_evidence(raw: &[String], config: &Config) -> (Vec<String>, u64) {
    let mut redactions = 0;
    let redacted = raw
        .iter()
        .map(|line| {
            let (safe, count) = super::harness::redact(line, &config.secrets);
            redactions += count;
            safe
        })
        .collect();
    (redacted, redactions)
}
