//! Task sessions run model turns without leaking the process directory.

use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use serde_json::json;
use std::fs;
#[test]
fn restored_task_session_runs_a_fresh_model_turn_without_changing_directory() {
    let (address, server) = mock_server(vec![
        ok(tool_response(
            "create_file",
            json!({"path":"first-turn.txt","contents":"created before restore"}),
        )),
        ok(tool_response(
            "complete_task",
            json!({"result":"first turn complete"}),
        )),
        ok(tool_response("read_file", json!({"path":"first-turn.txt"}))),
        ok(tool_response(
            "complete_task",
            json!({"result":"fresh turn complete"}),
        )),
    ]);
    let workspace = TempDir::new();
    let other_directory = TempDir::new();
    let (result, session, task_id, first_turn_id, second_turn_id) =
        in_workspace(workspace.path(), || {
            let (mut session, task_id, first_turn_id) = running_turn();
            let mut first_source = OllamaDecisionSource::with_endpoint("mock-model", 2, address);
            let first_result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                first_turn_id,
                "Create the first turn file",
                2,
                &mut first_source,
            )
            .expect("first mocked turn succeeds");
            assert_eq!(first_result.stop_reason(), GoalTurnStopReason::Completed);

            let checkpoint = session
                .export_checkpoint()
                .expect("export completed turn checkpoint");
            std::env::set_current_dir(other_directory.path())
                .expect("change to unrelated process directory");
            let original_directory =
                std::env::current_dir().expect("read unrelated process directory");
            let mut restored =
                TaskSession::restore_checkpoint_in_workspace(&checkpoint, workspace.path())
                    .expect("restore the session at its explicit workspace");
            assert_eq!(
                std::env::current_dir().expect("read process directory after restore"),
                original_directory
            );

            let second_turn_id = {
                let task = restored
                    .get_task_mut(task_id)
                    .expect("restored task exists");
                let turn_id = task.create_turn().expect("create fresh restored turn");
                task.transition_turn(turn_id, TurnState::Running)
                    .expect("start fresh restored turn");
                turn_id
            };
            let mut second_source = OllamaDecisionSource::with_endpoint("mock-model", 2, address);
            let result = run_goal_turn_with_budget(
                &mut restored,
                task_id,
                second_turn_id,
                "Read the first turn file",
                2,
                &mut second_source,
            );
            assert_eq!(
                std::env::current_dir().expect("read process directory after fresh turn"),
                original_directory
            );
            (result, restored, task_id, first_turn_id, second_turn_id)
        });

    let result = result.expect("fresh turn on restored session succeeds");
    assert_eq!(result.actions_taken(), 2);
    assert_eq!(result.observations().len(), 1);
    assert!(matches!(
        &result.observations()[0],
        crate::agent_loop::WorkspaceObservation::FileRead { contents, .. }
            if contents == "created before restore"
    ));
    let task = session.get_task(task_id).expect("restored task exists");
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(
        task.get_turn(first_turn_id)
            .expect("first turn exists")
            .state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.get_turn(second_turn_id)
            .expect("fresh turn exists")
            .state(),
        TurnState::Succeeded
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("first-turn.txt")).expect("read original file"),
        "created before restore"
    );
    let requests = server.join().expect("join mock server");
    assert_eq!(requests.len(), 4);
    let third_request = request_json(&requests[2]);
    let fresh_turn_prompt = third_request["messages"][1]["content"]
        .as_str()
        .expect("fresh turn prompt");
    assert!(fresh_turn_prompt.contains("Read the first turn file"));
    assert!(!fresh_turn_prompt.contains("file_created"));
}

#[test]
fn mocked_model_uses_an_explicit_workspace_without_changing_current_directory() {
    let _lock = crate::CURRENT_DIR_TEST_LOCK
        .lock()
        .expect("lock current directory");
    let (address, server) = mock_server(vec![
        ok(tool_response(
            "create_file",
            json!({"path":"model result.txt","contents":"created in selected workspace"}),
        )),
        ok(tool_response(
            "complete_task",
            json!({"result":"created model result.txt"}),
        )),
    ]);
    let temporary = TempDir::new();
    let workspace = temporary.path().join("selected workspace");
    fs::create_dir(&workspace).expect("create selected workspace");
    let original_directory = std::env::current_dir().expect("read current directory");
    assert_ne!(
        fs::canonicalize(&workspace).expect("canonicalize workspace"),
        original_directory
    );

    let mut session =
        TaskSession::new_in_workspace(&workspace).expect("capture selected workspace");
    let task = session.create_task("Create a result in the selected workspace");
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
        "Create a result file",
        2,
        &mut source,
    )
    .expect("run mocked local-model turn");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        fs::read_to_string(workspace.join("model result.txt")).expect("read model result"),
        "created in selected workspace"
    );
    assert_eq!(
        std::env::current_dir().expect("read current directory"),
        original_directory
    );
    assert_eq!(server.join().expect("join mock server").len(), 2);
}
