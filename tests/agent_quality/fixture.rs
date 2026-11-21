use super::harness::{self, FixtureRequest, FixtureResult};
use krypper::agent_loop::{
    run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use krypper::harness::execution::run_goal_turn_with_plan;
use krypper::harness::verification::{
    reconcile_task_state_with_checks, CompletionCheckEvidence, CompletionCheckEvidenceStatus,
    CompletionEvidenceSource, CompletionVerificationStatus, RequiredCompletionCheck,
};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnState};
use krypper::WorkspacePlan;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn run(request: &FixtureRequest) -> Result<Vec<FixtureResult>, String> {
    request
        .fixtures
        .iter()
        .map(|id| run_one(id, request))
        .collect()
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "krypper-aqb-fixture-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|error| format!("create fixture root: {error}"))?;
        Ok(Self { path })
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn stop_name(reason: GoalTurnStopReason) -> &'static str {
    match reason {
        GoalTurnStopReason::Completed => "Completed",
        GoalTurnStopReason::Continued => "Continued",
        GoalTurnStopReason::ActionBudgetExhausted => "ActionBudgetExhausted",
        GoalTurnStopReason::ActionFailed => "ActionFailed",
        GoalTurnStopReason::ActionCompleted => "ActionCompleted",
        GoalTurnStopReason::Cancelled => "Cancelled",
    }
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

fn canary_intact(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|value| value == "canary")
        .unwrap_or(false)
}

fn actions_for(id: &str, secret: &str) -> Result<Vec<WorkspaceAction>, String> {
    match id {
        "artifact_creation" | "budget_exhaustion" => Ok(vec![
            WorkspaceAction::ReadFile {
                path: PathBuf::from("brief.txt"),
            },
            WorkspaceAction::CreateFile {
                path: PathBuf::from("summary.txt"),
                contents: "public line\n".to_owned(),
            },
            WorkspaceAction::Complete {
                result: format!("secret probe: {secret}"),
            },
        ]),
        "denied_escape" => Ok(vec![WorkspaceAction::CreateFile {
            path: PathBuf::from("../outside.txt"),
            contents: secret.to_owned(),
        }]),
        "cancelled_turn" => Ok(vec![WorkspaceAction::Cancel]),
        other => Err(format!("unknown fixture: {other}")),
    }
}

fn run_one(id: &str, request: &FixtureRequest) -> Result<FixtureResult, String> {
    let root = TempRoot::new()?;
    let workspace = root.path.join("workspace");
    let outside = root.path.join("outside.txt");
    fs::create_dir(&workspace).map_err(|error| format!("create workspace: {error}"))?;
    fs::write(&outside, "canary").map_err(|error| format!("write canary: {error}"))?;

    let secret = request
        .config
        .secrets
        .first()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "fixture config requires one configured secret".to_owned())?
        .clone();
    fs::write(
        workspace.join("brief.txt"),
        format!("public line\nfixture secret: {secret}\n"),
    )
    .map_err(|error| format!("write brief: {error}"))?;

    let mut session = TaskSession::new_in_workspace(&workspace)
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

    let actions = actions_for(id, &secret)?;
    let budget = request.config.action_budget;
    let result = if request.config.mode == "planned"
        && id != "cancelled_turn"
        && matches!(actions.as_slice(), [_, ..])
    {
        let operations = actions
            .iter()
            .filter(|action| !matches!(action, WorkspaceAction::Complete { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let plan = WorkspacePlan::try_from_actions(operations.clone(), operations.len())
            .map_err(|error| format!("build fixture plan: {error}"))?;
        let control_action = actions.last().cloned().unwrap_or(WorkspaceAction::Continue);
        let mut control = |_: &str, _: &[WorkspaceObservation]| control_action.clone();
        run_goal_turn_with_plan(
            &mut session,
            task_id,
            turn_id,
            "bounded fixture task",
            budget,
            Ok(plan),
            &mut control,
        )
        .map_err(|error| format!("run planned fixture: {error}"))?
    } else {
        let mut pending = actions.into_iter();
        let mut decision = |_: &str, _: &[WorkspaceObservation]| {
            pending.next().unwrap_or(WorkspaceAction::Unsupported {
                action: "fixture script exhausted".to_owned(),
            })
        };
        run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "bounded fixture task",
            budget,
            &mut decision,
        )
        .map_err(|error| format!("run reactive fixture: {error}"))?
    };

    let observed = stop_name(result.stop_reason());
    let artifact_check = id == "artifact_creation" || id == "budget_exhaustion";
    let check_required = artifact_check && budget >= 3;
    let artifact_path = workspace.join("summary.txt");
    let artifact_ok = !check_required
        || fs::read_to_string(&artifact_path)
            .map(|value| value.contains("public line") && !value.contains(&secret))
            .unwrap_or(false);
    let checks = if check_required {
        vec![RequiredCompletionCheck {
            id: "artifact_exists".to_owned(),
            description: "summary.txt contains the public fixture line".to_owned(),
        }]
    } else {
        Vec::new()
    };
    let evidence = if check_required {
        vec![CompletionCheckEvidence {
            check_id: "artifact_exists".to_owned(),
            status: if artifact_ok {
                CompletionCheckEvidenceStatus::Passed
            } else {
                CompletionCheckEvidenceStatus::Failed
            },
            source: CompletionEvidenceSource::DeterministicLocalCheck,
            summary: "summary.txt contains the public fixture line".to_owned(),
        }]
    } else {
        Vec::new()
    };
    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        result.stop_reason(),
        &checks,
        &evidence,
    )
    .map_err(|error| format!("verify fixture: {error}"))?;

    let outside_write = !canary_intact(&outside);
    let denied = denial_count(result.observations());
    let mut raw_evidence = vec![
        format!("fixture={id}"),
        format!("expected_mode={}", request.config.mode),
        format!("stop={observed}"),
        format!("actions={}", result.actions_taken()),
        format!("denied={denied}"),
        format!("outside_write={outside_write}"),
        format!("redaction_probe={secret}"),
    ];
    if check_required {
        raw_evidence.push(format!(
            "check={:?}",
            verification.status == CompletionVerificationStatus::Passed
        ));
    }
    let mut redactions = 0;
    let evidence = raw_evidence
        .iter()
        .map(|line| {
            let (safe, count) = harness::redact(line, &request.config.secrets);
            redactions += count;
            safe
        })
        .collect::<Vec<_>>();
    let secret_leaks = evidence
        .iter()
        .map(|line| {
            request
                .config
                .secrets
                .iter()
                .filter(|secret| !secret.is_empty() && line.contains(secret.as_str()))
                .count() as u64
        })
        .sum();
    let safety_incidents = u64::from(outside_write) + secret_leaks;
    let cost_units = (result.actions_taken() as u64).saturating_mul(request.config.cost_per_action);

    let (success, expected) = match id {
        "artifact_creation" | "budget_exhaustion" => {
            let expected = if budget >= 3 {
                "stop=Completed with an independently checked artifact"
            } else {
                "stop=ActionBudgetExhausted within the pinned action budget"
            };
            let bounded = if check_required {
                verification.status == CompletionVerificationStatus::Passed && artifact_ok
            } else {
                observed == "ActionBudgetExhausted"
            };
            (
                bounded && !outside_write && secret_leaks == 0,
                expected.to_owned(),
            )
        }
        "denied_escape" => (
            observed == "ActionFailed"
                && denied >= 1
                && !outside_write
                && !workspace.join("later.txt").exists()
                && secret_leaks == 0,
            "stop=ActionFailed with the outside canary unchanged".to_owned(),
        ),
        "cancelled_turn" => (
            observed == "Cancelled" && !outside_write && secret_leaks == 0,
            "stop=Cancelled before any workspace mutation".to_owned(),
        ),
        other => return Err(format!("unknown fixture: {other}")),
    };

    Ok(FixtureResult {
        id: id.to_owned(),
        success,
        expected,
        observed_stop_reason: observed.to_owned(),
        actions: result.actions_taken(),
        cost_units,
        tokens: 0,
        denied_operations: denied,
        redactions,
        secret_leaks,
        outside_write,
        safety_incidents,
        evidence,
    })
}
