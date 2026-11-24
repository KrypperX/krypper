use super::*;
use crate::agent_loop::{BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction};
use crate::harness::execution::{
    create_impact_checked_plan, review_existing_impact_plan,
    run_goal_turn_with_impact_checked_completion_correction,
};
use crate::harness::impact::{
    analyze_plan, ImpactCheckedPlan, ImpactCoverage, ImpactReferenceKind, PublicApiStatus,
};
use crate::harness::verification::{
    CompletionCheckEvidence, CompletionCheckEvidenceStatus, CompletionEvidenceSource,
    RequiredCompletionCheck,
};
use crate::task_lifecycle::{TaskState, TurnState};
use crate::{OfflineCargoPolicy, ProcessOutcome, ProcessResult};
use std::collections::VecDeque;
use std::path::PathBuf;

#[test]
fn impact_checked_planner_sends_references_to_same_model_before_accepting_plan() {
    let workspace = TempDir::new();
    fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    fs::create_dir_all(workspace.path().join("tests")).expect("create test directory");
    fs::write(workspace.path().join("src/lib.rs"), "pub fn old_api() {}\n").expect("write source");
    fs::write(
        workspace.path().join("tests/api.rs"),
        "fn api_smoke() { old_api(); }\n",
    )
    .expect("write test reference");
    let (session, task_id, turn_id) =
        running_turn_in_workspace(workspace.path(), "Rename old_api to new_api");
    let source_patch =
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn old_api() {}\n+pub fn new_api() {}\n";
    let test_patch = "--- a/tests/api.rs\n+++ b/tests/api.rs\n@@ -1 +1 @@\n-fn api_smoke() { old_api(); }\n+fn api_smoke() { new_api(); }\n";
    let multi_file_plan = || {
        json!({
            "message": {
                "role": "assistant",
                "tool_calls": [
                    {"function":{"name":"apply_patch","arguments":{"path":"src/lib.rs","patch":source_patch}}},
                    {"function":{"name":"apply_patch","arguments":{"path":"tests/api.rs","patch":test_patch}}}
                ]
            }
        })
        .to_string()
    };
    let proposed = multi_file_plan();
    let reviewed = multi_file_plan();
    let (address, server) = mock_server(vec![ok(proposed), ok(reviewed)]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 4, address);

    let plan = create_impact_checked_plan(
        &session,
        task_id,
        turn_id,
        &mut source,
        "Rename old_api to new_api",
        &[],
        4,
    )
    .expect("analyze and review plan");
    let requests = server.join().expect("join model fixture");

    assert_eq!(plan.plan().operations().len(), 2);
    assert_eq!(plan.report().files.len(), 2);
    assert_eq!(plan.report().coverage, Some(ImpactCoverage::Complete));
    assert_eq!(
        plan.report().files[0].public_api_status,
        PublicApiStatus::Changed
    );
    assert!(plan.report().files[0]
        .public_api_symbols
        .iter()
        .any(|symbol| symbol == "removed:old_api"));
    assert!(plan.report().references.iter().any(|reference| {
        reference.path == "tests/api.rs"
            && reference.symbol == "old_api"
            && reference.kind == ImpactReferenceKind::TestReference
            && reference.line == 1
            && reference.start_column == 18
    }));
    assert_eq!(requests.len(), 2);
    let review_request = request_json(&requests[1]);
    let review_prompt = review_request["messages"][1]["content"]
        .as_str()
        .expect("impact review prompt");
    assert!(review_prompt.contains("Impact report (untrusted JSON)"));
    assert!(review_prompt.contains("tests/api.rs"));
    assert!(review_prompt.contains("old_api"));
    assert!(review_prompt.contains("Proposed workspace plan (untrusted JSON)"));
    assert_eq!(source.remaining_decisions, 1);
    assert!(matches!(
        plan.plan().operations().first(),
        Some(WorkspaceAction::ApplyPatch { path, .. }) if path == "src/lib.rs"
    ));
}

#[test]
fn revised_impact_report_gets_confirmation_before_execution() {
    let workspace = TempDir::new();
    fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    fs::create_dir_all(workspace.path().join("tests")).expect("create test directory");
    fs::write(workspace.path().join("src/lib.rs"), "fn old_name() {}\n").expect("write source");
    fs::write(
        workspace.path().join("tests/api.rs"),
        "fn api_smoke() { old_name(); }\n",
    )
    .expect("write test reference");
    let (session, task_id, turn_id) =
        running_turn_in_workspace(workspace.path(), "Rename old_name and update its test");
    let source_patch =
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-fn old_name() {}\n+fn new_name() {}\n";
    let test_patch =
        "--- a/tests/api.rs\n+++ b/tests/api.rs\n@@ -1 +1 @@\n-fn api_smoke() { old_name(); }\n+fn api_smoke() { new_name(); }\n";
    let plan = |include_test: bool| {
        let mut calls = vec![json!({
            "function": { "name": "apply_patch", "arguments": { "path": "src/lib.rs", "patch": source_patch } }
        })];
        if include_test {
            calls.push(json!({
                "function": { "name": "apply_patch", "arguments": { "path": "tests/api.rs", "patch": test_patch } }
            }));
        }
        json!({ "message": { "role": "assistant", "tool_calls": calls } }).to_string()
    };
    let (address, server) = mock_server(vec![ok(plan(false)), ok(plan(true)), ok(plan(true))]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 5, address);

    let checked = create_impact_checked_plan(
        &session,
        task_id,
        turn_id,
        &mut source,
        "Rename old_name and update its test",
        &[],
        5,
    )
    .expect("confirm the revised impact report");
    let requests = server.join().expect("join model fixture");

    assert_eq!(checked.plan().operations().len(), 2);
    assert!(checked
        .report()
        .mutation_targets
        .iter()
        .any(|path| path == "tests/api.rs"));
    assert_eq!(requests.len(), 3);
    let final_review = request_json(&requests[2]);
    let prompt = final_review["messages"][1]["content"]
        .as_str()
        .expect("confirmation prompt");
    assert!(prompt.contains("tests/api.rs"));
    assert!(prompt.contains("new_name"));
    assert_eq!(source.remaining_decisions, 1);
}

#[test]
fn truncated_reference_evidence_is_sent_for_model_review() {
    let workspace = TempDir::new();
    fs::write(
        workspace.path().join("source.rs"),
        "fn old_alpha() {}\nfn old_bravo() {}\nfn old_charlie() {}\nfn old_delta() {}\nfn old_echo() {}\n",
    )
    .expect("write source");
    let (session, task_id, turn_id) =
        running_turn_in_workspace(workspace.path(), "rename the selected identifiers");
    let patch = "--- a/source.rs\n+++ b/source.rs\n@@ -1,5 +1,5 @@\n-fn old_alpha() {}\n-fn old_bravo() {}\n-fn old_charlie() {}\n-fn old_delta() {}\n-fn old_echo() {}\n+fn new_alpha() {}\n+fn new_bravo() {}\n+fn new_charlie() {}\n+fn new_delta() {}\n+fn new_echo() {}\n";
    let plan = json!({
        "message": { "role": "assistant", "tool_calls": [
            {"function":{"name":"apply_patch","arguments":{"path":"source.rs","patch":patch}}}
        ]}
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(plan.clone()), ok(plan)]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);

    let checked = create_impact_checked_plan(
        &session,
        task_id,
        turn_id,
        &mut source,
        "rename the selected identifiers",
        &[],
        3,
    )
    .expect("review a report with truncated reference coverage");
    let requests = server.join().expect("join model fixture");

    assert!(checked.report().truncated);
    assert_eq!(requests.len(), 2);
    let review_request = request_json(&requests[1]);
    let prompt = review_request["messages"][1]["content"]
        .as_str()
        .expect("impact review prompt");
    assert!(prompt.contains("truncated"));
    assert!(prompt.contains("remaining references are unknown"));
}

#[test]
fn unknown_public_signature_change_is_reviewed() {
    let workspace = TempDir::new();
    fs::create_dir_all(workspace.path().join("src")).expect("create source directory");
    fs::write(
        workspace.path().join("src/lib.rs"),
        "pub fn parse(input: &str) -> usize;\n",
    )
    .expect("write public API");
    let (session, task_id, turn_id) =
        running_turn_in_workspace(workspace.path(), "change the parse input type");
    let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn parse(input: &str) -> usize;\n+pub fn parse(input: Path) -> usize;\n";
    let plan = json!({
        "message": { "role": "assistant", "tool_calls": [
            {"function":{"name":"apply_patch","arguments":{"path":"src/lib.rs","patch":patch}}}
        ]}
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(plan.clone()), ok(plan)]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);

    let checked = create_impact_checked_plan(
        &session,
        task_id,
        turn_id,
        &mut source,
        "change the parse input type",
        &[],
        3,
    )
    .expect("route unknown signature change to model review");
    let requests = server.join().expect("join model fixture");

    assert_eq!(
        checked.report().files[0].public_api_status,
        PublicApiStatus::Unknown
    );
    assert_eq!(requests.len(), 2);
    let review = request_json(&requests[1]);
    assert!(review["messages"][1]["content"]
        .as_str()
        .expect("review prompt")
        .contains("public API status"));
}

#[test]
fn verified_process_failure_correction_is_impact_reviewed_and_reruns_tests() {
    let fixture = TempDir::new();
    let workspace = fixture.path().join("workspace");
    let toolchain = fixture.path().join("toolchain");
    let registry = fixture.path().join("registry");
    fs::create_dir_all(workspace.join("src")).expect("create source directory");
    fs::create_dir_all(workspace.join("tests")).expect("create test directory");
    fs::create_dir_all(toolchain.join("bin")).expect("create toolchain fixture");
    fs::create_dir_all(&registry).expect("create registry fixture");
    fs::write(toolchain.join("bin/cargo"), b"\x7fELF").expect("fake cargo header");
    fs::write(toolchain.join("bin/rustc"), b"\x7fELF").expect("fake rustc header");
    fs::write(workspace.join("src/lib.rs"), "pub fn old_api() {}\n").expect("write source");
    fs::write(
        workspace.join("tests/api.rs"),
        "fn api_smoke() { old_api(); }\n",
    )
    .expect("write test");
    let mut session = TaskSession::new_in_workspace(&workspace).expect("bind workspace");
    let task = session.create_task("repair the failing API test");
    let task_id = task.id();
    task.transition(TaskState::Running).expect("start task");
    session
        .authorize_offline_cargo_tests(
            task_id,
            OfflineCargoPolicy::new(&toolchain, &registry).expect("test policy"),
        )
        .expect("authorize fixture tests");
    session.queue_offline_cargo_results_for_test(
        task_id,
        [
            process_result(ProcessOutcome::Exited(101), false),
            process_result(ProcessOutcome::Exited(0), true),
        ],
    );
    let turn_id = session
        .get_task_mut(task_id)
        .expect("task exists")
        .create_turn()
        .expect("create turn");
    session
        .get_task_mut(task_id)
        .expect("task exists")
        .transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    let initial_plan = WorkspacePlan::try_from_actions(
        vec![WorkspaceAction::OfflineCargoTest {
            test_filter: Some("api_smoke".to_owned()),
        }],
        1,
    )
    .expect("initial test plan");
    let (initial_report, initial_revisions) =
        analyze_plan(&session, task_id, turn_id, &initial_plan).expect("analyze initial plan");
    let initial_checked = ImpactCheckedPlan {
        plan: initial_plan,
        report: initial_report,
        expected_revisions: initial_revisions,
    };

    let source_patch =
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn old_api() {}\n+pub fn new_api() {}\n";
    let test_patch = "--- a/tests/api.rs\n+++ b/tests/api.rs\n@@ -1 +1 @@\n-fn api_smoke() { old_api(); }\n+fn api_smoke() { new_api(); }\n";
    let correction_response = json!({
        "message": { "role": "assistant", "tool_calls": [
            {"function":{"name":"apply_patch","arguments":{"path":"src/lib.rs","patch":source_patch}}},
            {"function":{"name":"apply_patch","arguments":{"path":"tests/api.rs","patch":test_patch}}},
            {"function":{"name":"offline_cargo_test","arguments":{"test_filter":"api_smoke"}}}
        ]}
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(correction_response)]);
    let mut brain = OllamaDecisionSource::with_endpoint("fixture-model", 12, address);
    let mut controls = CompleteControl(VecDeque::from(["initial", "corrected"]));
    let required = [RequiredCompletionCheck {
        id: "api_updated".to_owned(),
        description: "the source and test agree on the renamed API".to_owned(),
    }];

    let result = run_goal_turn_with_impact_checked_completion_correction(
        &mut session,
        task_id,
        turn_id,
        "repair the failing API test",
        9,
        Some(Ok(initial_checked)),
        &required,
        |_session, _turn| {
            let passed = fs::read_to_string(workspace.join("src/lib.rs"))
                .is_ok_and(|text| text.contains("new_api"))
                && fs::read_to_string(workspace.join("tests/api.rs"))
                    .is_ok_and(|text| text.contains("new_api"));
            vec![CompletionCheckEvidence {
                check_id: "api_updated".to_owned(),
                status: if passed {
                    CompletionCheckEvidenceStatus::Passed
                } else {
                    CompletionCheckEvidenceStatus::Failed
                },
                source: CompletionEvidenceSource::DeterministicLocalCheck,
                summary: if passed {
                    "the renamed API and test are present".to_owned()
                } else {
                    "the API update is incomplete".to_owned()
                },
            }]
        },
        |_,
         session,
         correction_task_id,
         correction_turn_id,
         failure,
         prior,
         _verification,
         goal,
         remaining| {
            let failure = failure.expect("failed tests select process correction");
            assert_eq!(prior.stop_reason(), GoalTurnStopReason::ActionFailed);
            assert_eq!(failure.outcome, ProcessOutcome::Exited(101));
            let proposed = WorkspacePlan::try_from_actions(
                vec![
                    WorkspaceAction::ApplyPatch {
                        path: PathBuf::from("src/lib.rs"),
                        patch: source_patch.to_owned(),
                    },
                    WorkspaceAction::ApplyPatch {
                        path: PathBuf::from("tests/api.rs"),
                        patch: test_patch.to_owned(),
                    },
                    WorkspaceAction::OfflineCargoTest {
                        test_filter: Some("api_smoke".to_owned()),
                    },
                ],
                3,
            )
            .expect("correction includes API edit and test rerun");
            review_existing_impact_plan(
                session,
                correction_task_id,
                correction_turn_id,
                &mut brain,
                goal,
                prior.observations(),
                remaining,
                proposed,
            )
        },
        &mut controls,
    )
    .expect("run impact-checked process correction");
    let requests = server.join().expect("join model fixture");

    assert_eq!(
        result.initial_result().stop_reason(),
        GoalTurnStopReason::ActionFailed
    );
    assert_eq!(
        result.test_verification(),
        Some(crate::harness::execution::ProcessTestVerificationStatus::Passed)
    );
    assert_eq!(
        result.final_verification().status,
        crate::harness::verification::CompletionVerificationStatus::Passed
    );
    assert!(result.correction_result().is_some());
    assert_eq!(requests.len(), 1);
    let review = request_json(&requests[0]);
    assert!(review["messages"][1]["content"]
        .as_str()
        .expect("review prompt")
        .contains("tests/api.rs"));
}

fn process_result(outcome: ProcessOutcome, succeeded: bool) -> ProcessResult {
    ProcessResult {
        run_id: Uuid::new_v4(),
        outcome,
        stdout: if succeeded {
            "running 1 test\ntest api_smoke ... ok\n".to_owned()
        } else {
            "running 1 test\ntest api_smoke ... FAILED\n".to_owned()
        },
        stderr: String::new(),
        truncated: false,
        diagnostic: None,
        cleanup_confirmed: true,
        test_filter: Some("api_smoke".to_owned()),
        tests_ran: true,
        executed_tests: Some(1),
        elapsed_ms: 0,
        output_bytes: 0,
        output_limit: 1024,
        resource_events: Vec::new(),
        recovery_hold: false,
    }
}

struct CompleteControl(VecDeque<&'static str>);

impl BoundedDecisionSource for CompleteControl {
    fn choose_action(
        &mut self,
        _goal: &str,
        _observations: &[crate::agent_loop::WorkspaceObservation],
    ) -> WorkspaceAction {
        WorkspaceAction::Complete {
            result: self
                .0
                .pop_front()
                .expect("completion remains queued")
                .to_owned(),
        }
    }
}
