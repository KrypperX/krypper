//! End-to-end turns through the registered workspace tools.

use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceObservation};
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use serde_json::json;
use std::fs;
use std::path::Path;
#[test]
fn registered_patch_tool_returns_verified_change_evidence_through_task_observations() {
    let patch = "--- a/notes with spaces.txt\n+++ b/notes with spaces.txt\n@@ -1,3 +1,3 @@\n top\n-old\n+new\n bottom\n";
    let (address, server) = mock_server(vec![
        ok(tool_response(
            "apply_patch",
            json!({"path":"notes with spaces.txt", "patch":patch}),
        )),
        ok(tool_response(
            "complete_task",
            json!({"result":"patch applied and verified"}),
        )),
    ]);
    let temporary = TempDir::new();
    let workspace = temporary.path().join("selected workspace");
    fs::create_dir(&workspace).expect("create selected workspace");
    fs::write(
        workspace.join("notes with spaces.txt"),
        "top\nold\nbottom\n",
    )
    .expect("write patch fixture");
    let mut session = TaskSession::new_in_workspace(&workspace).expect("capture workspace");
    let task = session.create_task("Apply one context-checked patch");
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    let mut source = OllamaDecisionSource::with_endpoint("mock-model", 2, address);

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "Patch the requested lines",
        2,
        &mut source,
    )
    .expect("run registered patch tool");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert!(matches!(
        result.observations().first(),
        Some(WorkspaceObservation::PatchApplied {
            path,
            before_diff,
            after_diff,
            ..
        }) if path == Path::new("notes with spaces.txt")
            && before_diff == "top\nold\nbottom"
            && after_diff == "top\nnew\nbottom"
    ));
    assert_eq!(
        fs::read(workspace.join("notes with spaces.txt")).expect("read patched file"),
        b"top\nnew\nbottom\n"
    );
    assert_eq!(server.join().expect("join mock server").len(), 2);
}

#[test]
fn mocked_model_performs_multiple_actions_and_completes_the_task() {
    let (address, server) = mock_server(vec![
        ok(tool_response("create_directory", json!({"path":"notes"}))),
        ok(tool_response(
            "create_file",
            json!({"path":"notes/summary.txt","contents":"created locally"}),
        )),
        ok(tool_response(
            "complete_task",
            json!({"result":"Created notes/summary.txt."}),
        )),
    ]);
    let workspace = TempDir::new();
    let (result, mut session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint("mock-model", 3, address);
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a summary file",
            3,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("complete mocked model turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(result.actions_taken(), 3);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Success { result }) if result == "Created notes/summary.txt."
    ));
    session
        .get_task_mut(task_id)
        .expect("task exists")
        .transition(TaskState::Succeeded)
        .expect("finish task");
    let task = session.get_task(task_id).expect("task exists");
    assert_eq!(task.state(), TaskState::Succeeded);
    assert_eq!(
        task.get_turn(turn_id).expect("turn exists").state(),
        TurnState::Succeeded
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes/summary.txt")).expect("read created file"),
        "created locally"
    );

    let requests = server.join().expect("join mock server");
    assert_eq!(requests.len(), 3);
    let first = request_json(&requests[0]);
    let second = request_json(&requests[1]);
    let third = request_json(&requests[2]);
    assert_eq!(first["model"], "mock-model");
    assert_eq!(first["stream"], false);
    assert!(first["messages"][1]["content"]
        .as_str()
        .expect("prompt text")
        .contains("Create a summary file"));
    assert!(second["messages"][1]["content"]
        .as_str()
        .expect("second prompt")
        .contains("directory_created"));
    assert!(third["messages"][1]["content"]
        .as_str()
        .expect("third prompt")
        .contains("file_created"));
    assert_eq!(
        first["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>(),
        [
            "read_file",
            "read_range",
            "list_directory",
            "create_directory",
            "search_text",
            "search_tree_text",
            "create_file",
            "replace_text",
            "apply_patch",
            "offline_cargo_test",
            "offline_read_only_exec",
            "complete_task"
        ]
    );
}

#[test]
fn model_decisions_stop_at_the_existing_action_budget() {
    let (address, server) = mock_server(vec![
        ok(tool_response("create_directory", json!({"path":"one"}))),
        ok(tool_response(
            "create_file",
            json!({"path":"one/two.txt","contents":"bounded"}),
        )),
    ]);
    let workspace = TempDir::new();
    let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint("mock-model", 2, address);
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a file, then stop at the budget",
            2,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("bounded model turn");
    assert_eq!(
        result.stop_reason(),
        GoalTurnStopReason::ActionBudgetExhausted
    );
    assert_eq!(result.actions_taken(), 2);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert_failed_turn(&result, &session, task_id, turn_id);
    let requests = server.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(fs::read(workspace.path().join("one/two.txt")).is_ok());
    assert_eq!(
        session.get_task(task_id).expect("task exists").state(),
        TaskState::Running
    );
}

#[test]
fn unsafe_model_path_fails_without_writing_outside_the_workspace() {
    let (address, server) = mock_server(vec![ok(tool_response(
        "create_file",
        json!({"path":"../outside.txt","contents":"must not escape"}),
    ))]);
    let root = TempDir::new();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let (result, session, task_id, turn_id) = in_workspace(&workspace, || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a file in the workspace",
            1,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("unsafe action is a recorded failure");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert_failed_turn(&result, &session, task_id, turn_id);
    assert!(!root.path().join("outside.txt").exists());
    assert!(fs::read_dir(&workspace)
        .expect("list workspace")
        .next()
        .is_none());
    assert_eq!(server.join().expect("join mock server").len(), 1);
}
