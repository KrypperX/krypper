use super::*;
use crate::agent_loop::{
    run_goal_turn_with_budget, GoalTurnResult, GoalTurnStopReason, WorkspaceAction,
};
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

fn assert_failed_turn(
    result: &GoalTurnResult,
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
) {
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    let task = session.get_task(task_id).expect("task exists");
    assert_eq!(task.state(), TaskState::Running);
    let turn = task.get_turn(turn_id).expect("turn exists");
    assert_eq!(turn.state(), TurnState::Failed);
    assert!(matches!(turn.outcome(), Some(TurnOutcome::Failure { .. })));
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

fn assert_anthropic_schema_supported(schema: &Value) {
    match schema {
        Value::Object(fields) => {
            for unsupported in ["minimum", "maximum", "minLength", "maxLength"] {
                assert!(
                    !fields.contains_key(unsupported),
                    "Anthropic schema contains unsupported `{unsupported}`"
                );
            }
            if fields.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    fields.get("additionalProperties"),
                    Some(&Value::Bool(false))
                );
            }
            fields.values().for_each(assert_anthropic_schema_supported);
        }
        Value::Array(values) => values.iter().for_each(assert_anthropic_schema_supported),
        _ => {}
    }
}

#[test]
fn model_name_validation_rejects_cloud_models() {
    assert!(validate_model_name_for_provider("qwen3", ModelProvider::Ollama).is_ok());
    assert!(validate_model_name_for_provider("gemma4:cloud", ModelProvider::Ollama).is_err());
    assert!(validate_model_name_for_provider("gemma4:cloud ", ModelProvider::Ollama).is_err());
    assert!(
        validate_model_name_for_provider("namespace/model:cloud", ModelProvider::Ollama).is_err()
    );
    assert!(
        validate_model_name_for_provider("qwen3-coder:480b-cloud", ModelProvider::Ollama).is_err()
    );
    assert!(validate_model_name_for_provider("gpt-oss:20b-cloud", ModelProvider::Ollama).is_err());
    assert!(
        validate_model_name_for_provider("qwen3-coder:480b-Cloud", ModelProvider::Ollama).is_err()
    );
}

#[test]
fn public_model_source_can_be_constructed_without_a_request() {
    let _source = crate::OllamaDecisionSource::new("mock-model", 1);
}

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
fn malformed_and_unsupported_model_calls_fail_the_active_turn() {
    for response in [
        ok("not json".to_owned()),
        ok(tool_response(
            "run_shell",
            json!({"command":"touch must-not-exist"}),
        )),
    ] {
        let (address, server) = mock_server(vec![response]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject malformed or unsupported calls",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("model call failure is recorded in the turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(fs::read_dir(workspace.path())
            .expect("list workspace")
            .next()
            .is_none());
        assert_eq!(server.join().expect("join mock server").len(), 1);
    }
}

#[test]
fn parser_maps_only_one_supported_tool_call() {
    let action = parse_model_response(
            br#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"create_file","arguments":{"path":"notes.txt","contents":"hello"}}}]}}"#,
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
fn parser_rejects_tool_calls_without_an_assistant_role() {
    assert!(parse_model_response(
        br#"{"message":{"tool_calls":[{"function":{"name":"create_file","arguments":{"path":"unsafe.txt","contents":"must not write"}}}]}}"#
    )
    .is_err());
    assert!(parse_model_response(
        br#"{"message":{"role":"user","tool_calls":[{"function":{"name":"create_file","arguments":{"path":"unsafe.txt","contents":"must not write"}}}]}}"#
    )
    .is_err());
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
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OllamaDecisionSource::with_endpoint("mock-model", 1, address);
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Fail clearly",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("model failure is recorded in the turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(matches!(
            result.outcome(),
            Some(TurnOutcome::Failure { .. })
        ));
        assert_eq!(server.join().expect("join mock server").len(), 1);
    }

    let workspace = TempDir::new();
    let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OllamaDecisionSource::with_endpoint(
            "mock-model",
            1,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        );
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Fail when the service cannot be reached",
            1,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("connection failure is recorded in the turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert_failed_turn(&result, &session, task_id, turn_id);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}

fn openai_function_call(name: &str, arguments: Value) -> Value {
    json!({
        "type": "function_call",
        "name": name,
        "arguments": arguments.to_string()
    })
}

fn anthropic_tool_use(name: &str, input: Value) -> Value {
    json!({"type":"tool_use", "id":"toolu_test", "name":name, "input":input})
}

fn request_header(request: &[u8], wanted: &str) -> Option<String> {
    let header_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    String::from_utf8_lossy(&request[..header_end])
        .lines()
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted)
                .then(|| value.trim().to_owned())
        })
}

fn request_body(request: &[u8]) -> Value {
    let body_start = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("request header terminator")
        + 4;
    serde_json::from_slice(&request[body_start..]).expect("decode provider request")
}

fn compatible_chat_function_call(name: &str, arguments: Value) -> Value {
    compatible_chat_function_call_with_raw_arguments(name, Value::String(arguments.to_string()))
}

fn compatible_chat_function_call_with_raw_arguments(name: &str, arguments: Value) -> Value {
    json!({
        "id":"call_test",
        "type":"function",
        "function":{"name":name,"arguments":arguments}
    })
}

fn compatible_chat_response(calls: Vec<Value>) -> String {
    json!({
        "choices":[{
            "finish_reason":"tool_calls",
            "message":{"role":"assistant","tool_calls":calls}
        }]
    })
    .to_string()
}

fn run_compatible_profile(
    provider: OpenAiCompatibleProvider,
    key: &str,
    response: String,
) -> (WorkspaceAction, Vec<u8>) {
    let (address, server) = mock_server(vec![ok(response)]);
    let endpoint = format!("http://{address}/v1/chat/completions");
    let mut source =
        OpenAiCompatibleDecisionSource::with_endpoint("test-model", provider, key, 3, endpoint);
    let action = source.choose_action(
        "Create a result from the observation",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("fixture.txt"),
            contents: "mock-only workspace text".to_owned(),
        }],
    );
    let requests = server.join().expect("join compatible chat mock");
    (action, requests.into_iter().next().expect("one request"))
}

fn assert_compatible_chat_request_with_tool_choice(
    request: &[u8],
    key: &str,
    expected_tool_choice: &str,
) -> Value {
    let first_line = String::from_utf8_lossy(request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    assert!(first_line.starts_with("POST /v1/chat/completions "));
    assert_eq!(
        request_header(request, "authorization"),
        Some(format!("Bearer {key}"))
    );
    let body = request_body(request);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["tool_choice"], expected_tool_choice);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["n"], 1);
    assert_eq!(body["stream"], false);
    assert!(body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["messages"][1]["content"].as_str().is_some_and(|text| {
        text.contains("Create a result from the observation")
            && text.contains("mock-only workspace text")
            && text.contains("including this decision: 3")
    }));
    let tools = body["tools"].as_array().expect("compatible tools array");
    assert!(tools
        .iter()
        .any(|tool| tool["function"]["name"] == "read_file"));
    assert!(tools
        .iter()
        .any(|tool| tool["function"]["name"] == "complete_task"));
    assert!(tools.iter().all(|tool| {
        tool["function"]["parameters"]["additionalProperties"] == false
            && tool["function"]["parameters"]["required"].is_array()
    }));
    assert!(!body.to_string().contains(key));
    body
}

fn assert_compatible_chat_request(request: &[u8], key: &str) -> Value {
    assert_compatible_chat_request_with_tool_choice(request, key, "required")
}

#[test]
fn compatible_chat_profiles_have_fixed_endpoints_and_cloudflare_path_validation() {
    let account_id = CloudflareAccountId::try_from("account-123").expect("safe account ID");
    assert_eq!(
        OpenAiCompatibleProvider::Cloudflare
            .endpoint(Some(&account_id))
            .expect("Cloudflare endpoint"),
        "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::Groq
            .endpoint(None)
            .expect("Groq endpoint"),
        "https://api.groq.com/openai/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::OpenRouter
            .endpoint(None)
            .expect("OpenRouter endpoint"),
        "https://openrouter.ai/api/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::DeepSeek
            .endpoint(None)
            .expect("DeepSeek endpoint"),
        "https://api.deepseek.com/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::Zai
            .endpoint(None)
            .expect("Z.AI endpoint"),
        "https://api.z.ai/api/paas/v4/chat/completions"
    );
    for invalid in [
        "",
        ".",
        "..",
        "account/other",
        "account\\other",
        "account?query",
        "%2f",
    ] {
        assert!(
            CloudflareAccountId::try_from(invalid).is_err(),
            "{invalid:?}"
        );
    }
    assert!(OpenAiCompatibleProvider::Cloudflare.endpoint(None).is_err());
}

#[test]
fn cloudflare_chat_profile_uses_compatible_request_and_normalizes_one_action() {
    let key = "local-cloudflare-test-token";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "create_file",
        json!({"path":"summary.txt","contents":"from Cloudflare"}),
    )]);
    let (action, request) =
        run_compatible_profile(OpenAiCompatibleProvider::Cloudflare, key, response);
    assert_compatible_chat_request(&request, key);
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: "from Cloudflare".to_owned(),
        }
    );
}

#[test]
fn groq_chat_profile_uses_compatible_request_and_normalizes_one_action() {
    let key = "local-groq-test-key";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "read_file",
        json!({"path":"plan.md"}),
    )]);
    let (action, request) = run_compatible_profile(OpenAiCompatibleProvider::Groq, key, response);
    assert_compatible_chat_request(&request, key);
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn openrouter_chat_profile_disables_provider_fallback_and_normalizes_one_action() {
    let key = "local-openrouter-test-key";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "complete_task",
        json!({"result":"done"}),
    )]);
    let (action, request) =
        run_compatible_profile(OpenAiCompatibleProvider::OpenRouter, key, response);
    let body = assert_compatible_chat_request(&request, key);
    assert_eq!(body["provider"]["allow_fallbacks"], false);
    assert_eq!(
        action,
        WorkspaceAction::Complete {
            result: "done".to_owned()
        }
    );
}

#[test]
fn deepseek_and_zai_chat_profiles_use_auto_tool_choice_and_accept_supported_calls() {
    let deepseek_key = "local-deepseek-test-key";
    let deepseek_response = compatible_chat_response(vec![compatible_chat_function_call(
        "create_file",
        json!({"path":"deepseek.txt","contents":"from DeepSeek"}),
    )]);
    let (deepseek_action, deepseek_request) = run_compatible_profile(
        OpenAiCompatibleProvider::DeepSeek,
        deepseek_key,
        deepseek_response,
    );
    assert_compatible_chat_request_with_tool_choice(&deepseek_request, deepseek_key, "auto");
    assert_eq!(
        deepseek_action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("deepseek.txt"),
            contents: "from DeepSeek".to_owned(),
        }
    );

    let zai_key = "local-zai-test-key";
    let zai_response =
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "read_file",
            json!({"path":"zai.md"}),
        )]);
    let (zai_action, zai_request) =
        run_compatible_profile(OpenAiCompatibleProvider::Zai, zai_key, zai_response);
    assert_compatible_chat_request_with_tool_choice(&zai_request, zai_key, "auto");
    assert_eq!(
        zai_action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("zai.md")
        }
    );
}

#[test]
fn deepseek_and_zai_parsers_accept_object_and_json_text_arguments() {
    for provider in [
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
    ] {
        for arguments in [
            Value::String(json!({"result":"done"}).to_string()),
            json!({"result":"done"}),
        ] {
            let response =
                compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
                    "complete_task",
                    arguments,
                )]);
            assert_eq!(
                parse_compatible_chat_response(
                    response.as_bytes(),
                    ModelProvider::OpenAiCompatible(provider)
                )
                .expect("valid provider arguments"),
                WorkspaceAction::Complete {
                    result: "done".to_owned()
                }
            );
        }
    }
}

#[test]
fn compatible_chat_parser_rejects_missing_malformed_unsupported_and_multiple_calls() {
    let one_call = compatible_chat_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"choices":[]}).to_string().into_bytes(),
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","tool_calls":[]}}]}).to_string().into_bytes(),
        compatible_chat_response(vec![one_call.clone(), one_call.clone()]).into_bytes(),
        json!({"choices":[
            {"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[one_call.clone()]}},
            {"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[one_call.clone()]}}
        ]}).to_string().into_bytes(),
        compatible_chat_response(vec![compatible_chat_function_call("run_shell", json!({}))]).into_bytes(),
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{
            "id":"call_bad","type":"function","function":{"name":"complete_task","arguments":"{invalid"}
        }]}}]}).to_string().into_bytes(),
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "create_file",
            json!({"path":"missing-contents.txt"}),
        )])
        .into_bytes(),
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"user","tool_calls":[one_call]}}]}).to_string().into_bytes(),
    ];

    for provider in [
        OpenAiCompatibleProvider::Groq,
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
    ] {
        for response in &invalid_responses {
            assert!(parse_compatible_chat_response(
                response,
                ModelProvider::OpenAiCompatible(provider)
            )
            .is_err());
        }
    }

    let object_arguments =
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "complete_task",
            json!({"result":"done"}),
        )]);
    assert!(parse_compatible_chat_response(
        object_arguments.as_bytes(),
        ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Groq)
    )
    .is_err());
}

#[test]
fn compatible_chat_multiple_calls_fail_without_executing_either_action() {
    for provider in [
        OpenAiCompatibleProvider::Groq,
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
    ] {
        let response = compatible_chat_response(vec![
            compatible_chat_function_call(
                "create_file",
                json!({"path":"must-not-exist.txt","contents":"blocked"}),
            ),
            compatible_chat_function_call("complete_task", json!({"result":"done"})),
        ]);
        let (address, server) = mock_server(vec![ok(response)]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
                "test-model",
                provider,
                "local-test-key",
                1,
                format!("http://{address}/v1/chat/completions"),
            );
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject multiple action calls",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("invalid action response is recorded as a failed turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(!workspace.path().join("must-not-exist.txt").exists());
        assert_eq!(
            server
                .join()
                .expect("join compatible multiple-call mock")
                .len(),
            1
        );
    }
}

#[test]
fn openai_responses_request_uses_fixed_contract_and_normalizes_one_action() {
    let (address, server) = mock_server(vec![ok(json!({
        "status":"completed",
        "output":[openai_function_call(
            "create_file",
            json!({"path":"summary.txt", "contents":"from OpenAI"})
        )]
    })
    .to_string())]);
    let api_key = "local-openai-test-key";
    let endpoint = format!("http://{address}/v1/responses");
    let mut source = OpenAiDecisionSource::with_endpoint("test-model:cloud", api_key, 3, endpoint);
    let action = source.choose_action(
        "Summarize the note",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("note.txt"),
            contents: "local-only fixture text".to_owned(),
        }],
    );
    let request = server.join().expect("join OpenAI mock server");
    let raw_request = &request[0];
    let first_line = String::from_utf8_lossy(raw_request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    let body = request_body(raw_request);

    assert!(first_line.starts_with("POST /v1/responses "));
    assert_eq!(
        request_header(raw_request, "authorization").as_deref(),
        Some("Bearer local-openai-test-key")
    );
    assert_eq!(body["model"], "test-model:cloud");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], false);
    assert!(body["instructions"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["input"].as_str().is_some_and(|text| {
        text.contains("Summarize the note") && text.contains("local-only fixture text")
    }));
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["path"]["type"],
        "string"
    );
    assert!(!body.to_string().contains(api_key));
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: "from OpenAI".to_owned(),
        }
    );
}

#[test]
fn anthropic_messages_request_uses_fixed_contract_and_normalizes_one_action() {
    let (address, server) = mock_server(vec![ok(json!({
        "stop_reason":"tool_use",
        "content":[anthropic_tool_use("read_file", json!({"path":"plan.md"}))]
    })
    .to_string())]);
    let api_key = "local-anthropic-test-key";
    let endpoint = format!("http://{address}/v1/messages");
    let mut source =
        AnthropicDecisionSource::with_endpoint("test-model:cloud", api_key, 2, endpoint);
    let action = source.choose_action("Inspect the plan", &[]);
    let request = server.join().expect("join Anthropic mock server");
    let raw_request = &request[0];
    let first_line = String::from_utf8_lossy(raw_request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    let body = request_body(raw_request);

    assert!(first_line.starts_with("POST /v1/messages "));
    assert_eq!(
        request_header(raw_request, "authorization").as_deref(),
        Some("Bearer local-anthropic-test-key")
    );
    assert_eq!(
        request_header(raw_request, "anthropic-version").as_deref(),
        Some("2026-10-21")
    );
    assert_eq!(body["model"], "test-model:cloud");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], false);
    assert!(body["system"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("Inspect the plan")));
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(body["tool_choice"]["type"], "auto");
    assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
    for tool in body["tools"].as_array().expect("Anthropic tools array") {
        assert_eq!(tool["strict"], true);
        assert_anthropic_schema_supported(&tool["input_schema"]);
    }
    assert!(!body.to_string().contains(api_key));
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn openai_responses_parser_rejects_malformed_unsupported_and_multiple_calls() {
    let one_call = openai_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"status":"completed", "output":[]})
            .to_string()
            .into_bytes(),
        json!({"status":"incomplete", "output":[one_call.clone()]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[one_call.clone(), one_call]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[openai_function_call("run_shell", json!({}))]})
            .to_string()
            .into_bytes(),
        json!({"status":"completed", "output":[{
            "type":"function_call", "name":"complete_task", "arguments":"{invalid"
        }]})
        .to_string()
        .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(parse_openai_response(&response).is_err());
    }
}

#[test]
fn anthropic_messages_parser_rejects_malformed_unsupported_and_multiple_calls() {
    let one_call = anthropic_tool_use("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"stop_reason":"end_turn", "content":[]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[one_call.clone(), one_call]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[anthropic_tool_use("run_shell", json!({}))]})
            .to_string()
            .into_bytes(),
        json!({"stop_reason":"tool_use", "content":[{
            "type":"tool_use", "name":"complete_task", "input":"not an object"
        }]})
        .to_string()
        .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(parse_anthropic_response(&response).is_err());
    }
}

#[test]
fn anthropic_schema_drops_provider_unsupported_bounds_but_runtime_validation_remains() {
    let tools = anthropic_tool_definitions();
    let tools = tools.as_array().expect("Anthropic tools array");
    let read_range = tools
        .iter()
        .find(|tool| tool["name"] == "read_range")
        .expect("read_range tool");
    assert_eq!(
        read_range["input_schema"]["properties"]["start_line"],
        json!({"type":"integer"})
    );
    assert_eq!(
        read_range["input_schema"]["properties"]["max_lines"],
        json!({"type":"integer"})
    );

    let invalid_calls = [
        (
            "read_range",
            json!({"path":"notes.txt", "start_line":0, "max_lines":1}),
        ),
        (
            "read_range",
            json!({"path":"notes.txt", "start_line":1, "max_lines":201}),
        ),
        ("search_text", json!({"directory":".", "query":""})),
        (
            "search_text",
            json!({"directory":".", "query":"x".repeat(4097)}),
        ),
        (
            "replace_text",
            json!({"path":"notes.txt", "match_text":"", "replacement_text":"next"}),
        ),
    ];

    for (name, input) in invalid_calls {
        let response = json!({
            "stop_reason":"tool_use",
            "content":[anthropic_tool_use(name, input)]
        });
        assert!(parse_anthropic_response(response.to_string().as_bytes()).is_err());
    }
}

#[test]
fn hosted_provider_errors_never_echo_keys_or_response_bodies() {
    let openai_key = "local-openai-error-key";
    let anthropic_key = "local-anthropic-error-key";
    for (key, provider, path) in [
        (openai_key, "OpenAI", "/v1/responses"),
        (anthropic_key, "Anthropic", "/v1/messages"),
    ] {
        let (address, server) = mock_server(vec![MockResponse {
            status: 401,
            body: json!({"error":{"message":key}}).to_string(),
        }]);
        let endpoint = format!("http://{address}{path}");
        let mut source = if provider == "OpenAI" {
            ModelDecisionSource::OpenAi(OpenAiDecisionSource::with_endpoint(
                "test-model",
                key,
                1,
                endpoint,
            ))
        } else {
            ModelDecisionSource::Anthropic(AnthropicDecisionSource::with_endpoint(
                "test-model",
                key,
                1,
                endpoint,
            ))
        };
        let action = source.choose_action("Test a failed request", &[]);
        let requests = server.join().expect("join provider error mock server");
        let recorded = String::from_utf8_lossy(&requests[0]);
        assert!(recorded.contains(key));
        assert!(!request_body(&requests[0]).to_string().contains(key));
        let WorkspaceAction::Unsupported { action } = action else {
            panic!("provider error should fail as an unsupported bounded action");
        };
        assert!(action.contains("401"));
        assert!(!action.contains(key));
        assert!(!action.contains("error\":{\"message"));
    }
}

#[test]
fn compatible_chat_errors_never_echo_keys_or_response_bodies() {
    for (provider, key) in [
        (OpenAiCompatibleProvider::Cloudflare, "local-cf-error-token"),
        (OpenAiCompatibleProvider::Groq, "local-groq-error-key"),
        (
            OpenAiCompatibleProvider::OpenRouter,
            "local-openrouter-error-key",
        ),
        (
            OpenAiCompatibleProvider::DeepSeek,
            "local-deepseek-error-key",
        ),
        (OpenAiCompatibleProvider::Zai, "local-zai-error-key"),
    ] {
        let (address, server) = mock_server(vec![MockResponse {
            status: 401,
            body: json!({"error":{"message":key}}).to_string(),
        }]);
        let endpoint = format!("http://{address}/v1/chat/completions");
        let mut source =
            OpenAiCompatibleDecisionSource::with_endpoint("test-model", provider, key, 1, endpoint);
        let action = source.choose_action("Test a failed request", &[]);
        let requests = server.join().expect("join compatible provider error mock");
        let recorded = String::from_utf8_lossy(&requests[0]);
        assert!(recorded.contains(key));
        assert!(!request_body(&requests[0]).to_string().contains(key));
        let WorkspaceAction::Unsupported { action } = action else {
            panic!("provider error should fail as an unsupported bounded action");
        };
        assert!(action.contains("401"));
        assert!(!action.contains(key));
        assert!(!action.contains("error\":{\"message"));
    }
}

fn gemini_function_call(name: &str, arguments: Value) -> Value {
    json!({"functionCall":{"name":name,"args":arguments}})
}

fn gemini_response(parts: Vec<Value>) -> String {
    json!({
        "candidates":[{
            "finishReason":"STOP",
            "content":{"role":"model","parts":parts}
        }]
    })
    .to_string()
}

#[test]
fn gemini_model_name_builds_only_a_safe_fixed_generate_content_path() {
    assert_eq!(
        gemini_generate_content_endpoint("gemini-2.5-flash").expect("valid model endpoint"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    for invalid in [
        "",
        ".",
        "..",
        "gemini/other",
        "gemini\\other",
        "gemini?query",
        "gemini#fragment",
        "gemini%2fother",
        "https://example.test/model",
    ] {
        assert!(
            validate_model_name_for_provider(invalid, ModelProvider::Gemini).is_err(),
            "accepted unsafe Gemini model name {invalid:?}"
        );
        assert!(gemini_generate_content_endpoint(invalid).is_err());
    }
}

#[test]
fn gemini_generate_content_uses_native_schema_api_key_and_one_bounded_action() {
    let key = "local-gemini-test-key";
    let response = gemini_response(vec![gemini_function_call(
        "read_file",
        json!({"path":"plan.md"}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let endpoint = format!("http://{address}/v1beta/models/test-model:generateContent");
    let mut source = GeminiDecisionSource::with_endpoint("test-model", key, 3, endpoint);
    let action = source.choose_action(
        "Inspect the plan and create a result",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("brief.txt"),
            contents: "local-only fixture contents".to_owned(),
        }],
    );
    let requests = server.join().expect("join Gemini mock server");
    let request = requests.first().expect("one Gemini request");
    let request_line = String::from_utf8_lossy(request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();

    assert!(request_line.starts_with("POST /v1beta/models/test-model:generateContent "));
    assert_eq!(
        request_header(request, "x-goog-api-key").as_deref(),
        Some(key)
    );
    assert_eq!(request_header(request, "authorization"), None);
    let body = request_body(request);
    assert!(body["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("Select exactly one tool call")));
    assert_eq!(body["contents"][0]["role"], "user");
    let prompt = body["contents"][0]["parts"][0]["text"]
        .as_str()
        .expect("Gemini user prompt");
    assert!(prompt.contains("Inspect the plan and create a result"));
    assert!(prompt.contains("local-only fixture contents"));
    assert!(prompt.contains("including this decision: 3"));
    assert_eq!(body["generationConfig"]["candidateCount"], 1);
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    let declarations = body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("Gemini function declarations");
    let names = declarations
        .iter()
        .filter_map(|declaration| declaration["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
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
    let allowed = body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"]
        .as_array()
        .expect("allowed Gemini function names")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(allowed, names);
    let read_range = declarations
        .iter()
        .find(|declaration| declaration["name"] == "read_range")
        .expect("read_range declaration");
    assert_eq!(read_range["parameters"]["type"], "OBJECT");
    assert_eq!(
        read_range["parameters"]["properties"]["path"]["type"],
        "STRING"
    );
    assert_eq!(
        read_range["parameters"]["properties"]["max_lines"]["type"],
        "INTEGER"
    );
    assert!(read_range["parameters"]
        .get("additionalProperties")
        .is_none());
    assert!(read_range["parameters"]["properties"]["max_lines"]
        .get("maximum")
        .is_none());
    assert!(read_range["parameters"]["properties"]["max_lines"]
        .get("minimum")
        .is_none());
    assert!(!body.to_string().contains(key));
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn gemini_response_parser_requires_exactly_one_well_formed_supported_call() {
    let one_call = gemini_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"candidates":[]}).to_string().into_bytes(),
        gemini_response(vec![json!({"text":"no function call"})]).into_bytes(),
        gemini_response(vec![one_call.clone(), one_call.clone()]).into_bytes(),
        json!({"candidates":[
            {"content":{"parts":[one_call.clone()]}},
            {"content":{"parts":[one_call.clone()]}}
        ]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"MALFORMED_FUNCTION_CALL",
            "content":{"parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"SAFETY",
            "content":{"role":"model","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"MAX_TOKENS",
            "content":{"role":"model","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "content":{"role":"user","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[
            {"content":{"role":"model","parts":[one_call.clone()]}},
            {"content":{"role":"model"}}
        ]})
        .to_string()
        .into_bytes(),
        gemini_response(vec![json!({"functionCall":{"args":{"result":"done"}}})]).into_bytes(),
        gemini_response(vec![
            json!({"functionCall":{"name":"complete_task","args":"not an object"}}),
        ])
        .into_bytes(),
        gemini_response(vec![gemini_function_call(
            "run_shell",
            json!({"command":"unsafe"}),
        )])
        .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(parse_gemini_response(&response).is_err());
    }
}

#[test]
fn gemini_parser_keeps_local_argument_bounds_authoritative() {
    for (name, args) in [
        (
            "read_range",
            json!({"path":"plan.md","start_line":1,"max_lines":201}),
        ),
        (
            "search_text",
            json!({"directory":".","query":"x".repeat(4097)}),
        ),
    ] {
        assert!(parse_gemini_response(
            gemini_response(vec![gemini_function_call(name, args)]).as_bytes()
        )
        .is_err());
    }
}

#[test]
fn gemini_invalid_call_responses_fail_without_executing_workspace_actions() {
    let invalid_responses = [
        gemini_response(vec![json!({"text":"no function call"})]),
        gemini_response(vec![
            gemini_function_call(
                "create_file",
                json!({"path":"must-not-exist.txt","contents":"blocked"}),
            ),
            gemini_function_call("complete_task", json!({"result":"done"})),
        ]),
        gemini_response(vec![gemini_function_call(
            "create_file",
            json!({"path":"must-not-exist.txt"}),
        )]),
        gemini_response(vec![gemini_function_call(
            "run_shell",
            json!({"command":"touch must-not-exist.txt"}),
        )]),
    ];

    for response in invalid_responses {
        let (address, server) = mock_server(vec![ok(response)]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = GeminiDecisionSource::with_endpoint(
                "test-model",
                "local-gemini-key",
                1,
                format!("http://{address}/v1beta/models/test-model:generateContent"),
            );
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject an invalid function call response",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("invalid call failure is recorded in the turn");

        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(!workspace.path().join("must-not-exist.txt").exists());
        assert_eq!(server.join().expect("join Gemini mock server").len(), 1);
    }
}

#[test]
fn gemini_provider_errors_never_echo_api_keys_or_response_bodies() {
    let key = "local-gemini-error-key";
    let (address, server) = mock_server(vec![MockResponse {
        status: 401,
        body: json!({"error":{"message":key}}).to_string(),
    }]);
    let mut source = GeminiDecisionSource::with_endpoint(
        "test-model",
        key,
        1,
        format!("http://{address}/v1beta/models/test-model:generateContent"),
    );
    let action = source.choose_action("Test a failed Gemini request", &[]);
    let requests = server.join().expect("join Gemini error mock server");
    let request = requests.first().expect("one failed Gemini request");
    assert_eq!(
        request_header(request, "x-goog-api-key").as_deref(),
        Some(key)
    );
    assert!(!request_body(request).to_string().contains(key));

    let WorkspaceAction::Unsupported { action } = action else {
        panic!("provider error should fail as an unsupported bounded action");
    };
    assert!(action.contains("401"));
    assert!(!action.contains(key));
    assert!(!action.contains("error\":{\"message"));
}
