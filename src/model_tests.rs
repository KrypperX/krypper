use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction};
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-model-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove temporary directory");
    }
}

struct RestoreCurrentDir(PathBuf);

impl RestoreCurrentDir {
    fn enter(path: &Path) -> Self {
        let previous = std::env::current_dir().expect("read current directory");
        std::env::set_current_dir(path).expect("set temporary workspace");
        Self(previous)
    }
}

impl Drop for RestoreCurrentDir {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore current directory");
    }
}

fn in_workspace<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    let _lock = crate::CURRENT_DIR_TEST_LOCK
        .lock()
        .expect("lock current directory");
    let _restore = RestoreCurrentDir::enter(path);
    operation()
}

fn running_turn() -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new();
    let task = session.create_task("Complete the requested workspace task");
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (session, task_id, turn_id)
}

struct MockResponse {
    status: u16,
    body: String,
}

fn ok(body: String) -> MockResponse {
    MockResponse { status: 200, body }
}

fn tool_response(name: &str, arguments: Value) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": name, "arguments": arguments }
            }]
        }
    })
    .to_string()
}

fn mock_server(responses: Vec<MockResponse>) -> (SocketAddr, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock Ollama server");
    let address = listener.local_addr().expect("read mock server address");
    let handle = thread::spawn(move || {
        responses
                .into_iter()
                .map(|response| {
                    let (mut stream, _) = listener.accept().expect("accept model request");
                    let request = read_request(&mut stream);
                    let reason = if response.status == 200 { "OK" } else { "Failure" };
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        reason,
                        response.body.len(),
                        response.body
                    )
                    .expect("write mock response");
                    request
                })
                .collect()
    });
    (address, handle)
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read mock request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("request content length");
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn request_json(request: &[u8]) -> Value {
    let body_start = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("request header terminator")
        + 4;
    serde_json::from_slice(&request[body_start..]).expect("decode model request")
}

#[test]
fn model_name_validation_rejects_cloud_models() {
    assert!(validate_model_name("qwen3").is_ok());
    assert!(validate_model_name("gemma4:cloud").is_err());
    assert!(validate_model_name("gemma4:cloud ").is_err());
    assert!(validate_model_name("namespace/model:cloud").is_err());
}

#[test]
fn parser_maps_only_one_supported_tool_call() {
    let action = parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"create_file","arguments":{"path":"notes.txt","contents":"hello"}}}]}}"#,
        )
        .expect("parse supported action");
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("notes.txt"),
            contents: "hello".to_owned()
        }
    );

    assert!(parse_model_response(b"not json").is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"run_shell","arguments":{"command":"touch unsafe"}}}]}}"#
        )
        .is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"complete_task","arguments":{"result":"done"}}},{"function":{"name":"complete_task","arguments":{"result":"also done"}}}]}}"#
        )
        .is_err());
    assert!(parse_model_response(
            br#"{"message":{"tool_calls":[{"function":{"name":"create_file","arguments":{"path":"notes.txt","contents":"hello","command":"unsafe"}}}]}}"#
        )
        .is_err());
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
    let (result, session, task_id) = in_workspace(workspace.path(), || {
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
        (result, session, task_id)
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
    let (result, _) = in_workspace(&workspace, || {
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
        (result, session)
    });
    let result = result.expect("unsafe action is a recorded failure");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert!(!root.path().join("outside.txt").exists());
    assert!(fs::read_dir(&workspace)
        .expect("list workspace")
        .next()
        .is_none());
    assert_eq!(server.join().expect("join mock server").len(), 1);
}

#[test]
fn http_and_response_decoding_failures_become_failed_actions() {
    for response in [
        MockResponse {
            status: 503,
            body: "service unavailable".to_owned(),
        },
        ok("{invalid json".to_owned()),
    ] {
        let (address, server) = mock_server(vec![response]);
        let workspace = TempDir::new();
        let result = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
            run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Fail clearly",
                1,
                &mut source,
            )
        })
        .expect("model failure is recorded in the turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert!(matches!(
            result.outcome(),
            Some(TurnOutcome::Failure { .. })
        ));
        assert_eq!(server.join().expect("join mock server").len(), 1);
    }

    let workspace = TempDir::new();
    let result = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint(
            "mock-model",
            1,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        );
        run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Fail when the service cannot be reached",
            1,
            &mut source,
        )
    })
    .expect("connection failure is recorded in the turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}
