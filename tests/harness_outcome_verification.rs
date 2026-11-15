use krypper::agent_loop::GoalTurnStopReason;
use krypper::harness::verification::{
    reconcile_task_state_with_checks, CompletionCheckEvidence, CompletionCheckEvidenceStatus,
    CompletionEvidenceSource, CompletionVerificationStatus, RequiredCompletionCheck,
    MAX_COMPLETION_CHECKS,
};
use krypper::task_lifecycle::{TaskHistoryTransition, TaskSession, TaskState};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-outcome-check-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create fixture workspace");
        Self(path)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove fixture workspace");
    }
}

fn running_task(workspace: &Path) -> (TaskSession, Uuid) {
    let mut session = TaskSession::new_in_workspace(workspace).expect("bind fixture workspace");
    let task_id = session.create_task("fixture task").id();
    session
        .get_task_mut(task_id)
        .expect("created task")
        .transition(TaskState::Running)
        .expect("start task");
    (session, task_id)
}

fn required_check() -> RequiredCompletionCheck {
    RequiredCompletionCheck {
        id: "artifact_exists".to_owned(),
        description: "The expected fixture artifact exists".to_owned(),
    }
}

fn evidence(status: CompletionCheckEvidenceStatus, summary: &str) -> CompletionCheckEvidence {
    CompletionCheckEvidence {
        check_id: "artifact_exists".to_owned(),
        status,
        source: CompletionEvidenceSource::DeterministicLocalCheck,
        summary: summary.to_owned(),
    }
}

#[test]
fn passing_independent_check_records_success_in_task_session() {
    let workspace = TempWorkspace::new();
    let artifact = workspace.0.join("artifact.txt");
    fs::write(&artifact, "fixture result").expect("write local fixture");
    let (mut session, task_id) = running_task(&workspace.0);
    let check_evidence = if artifact.is_file() {
        vec![evidence(
            CompletionCheckEvidenceStatus::Passed,
            "artifact.txt exists in the fixture workspace",
        )]
    } else {
        vec![evidence(
            CompletionCheckEvidenceStatus::Failed,
            "artifact.txt is missing",
        )]
    };

    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::Completed,
        &[required_check()],
        &check_evidence,
    )
    .expect("verify completed task");

    assert_eq!(verification.status, CompletionVerificationStatus::Passed);
    assert_eq!(verification.checks.len(), 1);
    let task = session.get_task(task_id).expect("task remains in session");
    assert_eq!(task.state(), TaskState::Succeeded);
    assert_eq!(
        task.history().last().expect("lifecycle record").sequence(),
        2
    );
    assert_eq!(
        task.history()
            .last()
            .expect("lifecycle record")
            .transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Running,
            new_state: TaskState::Succeeded,
        }
    );
}

#[test]
fn failed_evidence_overrides_a_completed_model_turn() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id) = running_task(&workspace.0);

    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::Completed,
        &[required_check()],
        &[evidence(
            CompletionCheckEvidenceStatus::Failed,
            "the expected artifact is absent",
        )],
    )
    .expect("verify completed task");

    assert_eq!(verification.status, CompletionVerificationStatus::Failed);
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn missing_or_unknown_evidence_cannot_succeed() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id) = running_task(&workspace.0);

    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::ActionCompleted,
        &[required_check()],
        &[],
    )
    .expect("verify completed task");

    assert_eq!(verification.status, CompletionVerificationStatus::Unknown);
    assert_eq!(
        verification.checks[0].status,
        CompletionVerificationStatus::Unknown
    );
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn explicitly_unknown_evidence_cannot_succeed() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id) = running_task(&workspace.0);

    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::Completed,
        &[required_check()],
        &[evidence(
            CompletionCheckEvidenceStatus::Unknown,
            "the fixture observation is inconclusive",
        )],
    )
    .expect("verify completed task");

    assert_eq!(verification.status, CompletionVerificationStatus::Unknown);
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}

#[test]
fn verification_does_not_change_workspace_contents() {
    let workspace = TempWorkspace::new();
    let sentinel = workspace.0.join("sentinel.txt");
    fs::write(&sentinel, "unchanged").expect("write local fixture");
    let before = fs::read_dir(&workspace.0)
        .expect("read fixture workspace")
        .map(|entry| entry.expect("fixture entry").file_name())
        .collect::<Vec<_>>();
    let (mut session, task_id) = running_task(&workspace.0);

    reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::Completed,
        &[required_check()],
        &[],
    )
    .expect("verify completed task");

    let after = fs::read_dir(&workspace.0)
        .expect("read fixture workspace")
        .map(|entry| entry.expect("fixture entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(before, after);
    assert_eq!(
        fs::read_to_string(sentinel).expect("read sentinel"),
        "unchanged"
    );
}

#[test]
fn too_many_required_checks_fail_closed_with_bounded_results() {
    let workspace = TempWorkspace::new();
    let (mut session, task_id) = running_task(&workspace.0);
    let checks = (0..=MAX_COMPLETION_CHECKS)
        .map(|index| RequiredCompletionCheck {
            id: format!("check_{index}"),
            description: "fixture check".to_owned(),
        })
        .collect::<Vec<_>>();
    let evidence = checks
        .iter()
        .map(|check| CompletionCheckEvidence {
            check_id: check.id.clone(),
            status: CompletionCheckEvidenceStatus::Passed,
            source: CompletionEvidenceSource::DeterministicLocalCheck,
            summary: "fixture passed".to_owned(),
        })
        .collect::<Vec<_>>();

    let verification = reconcile_task_state_with_checks(
        &mut session,
        task_id,
        GoalTurnStopReason::Completed,
        &checks,
        &evidence,
    )
    .expect("verify completed task");

    assert_eq!(verification.status, CompletionVerificationStatus::Unknown);
    assert_eq!(verification.checks.len(), MAX_COMPLETION_CHECKS);
    assert_eq!(verification.omitted_checks, 1);
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Failed
    );
}
