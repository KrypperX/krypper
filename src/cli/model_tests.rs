use super::model_selection::prepare_model_source_with_credentials;
use super::settings::{save_credentials, CredentialStore};
use super::{prepare_model_source, run_task_with_endpoint, snapshot_task_and_turn_state};
use crate::brain::{AdapterId, ModelProvider};
use crate::harness::execution::GoalTurnError;
use crate::task_lifecycle::{
    TaskSession, TaskState, TurnOutcome, TurnState, WorkspaceGuidanceReadError,
};
use crate::test_support::{with_current_dir, RestoreEnvironmentVariable, TempDir};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::thread::{self, JoinHandle};

#[path = "model_tests/provider_profile_tests.rs"]
mod provider_profile_tests;

#[test]
fn task_result_states_use_snapshot_entries_for_matching_ids() {
    let mut session = TaskSession::new();
    let earlier_task_id = session.create_task("Earlier task").id();
    let earlier_turn_id = session
        .get_task_mut(earlier_task_id)
        .unwrap()
        .create_turn()
        .unwrap();

    let task_id = session.create_task("Completed task").id();
    let task = session.get_task_mut(task_id).unwrap();
    task.transition(TaskState::Running).unwrap();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();
    session
        .complete_turn(
            task_id,
            turn_id,
            TurnOutcome::Success {
                result: "saved result".to_owned(),
            },
        )
        .unwrap();
    session
        .get_task_mut(task_id)
        .unwrap()
        .transition(TaskState::Succeeded)
        .unwrap();

    assert_eq!(
        snapshot_task_and_turn_state(&session, task_id, turn_id).unwrap(),
        (TaskState::Succeeded, TurnState::Succeeded)
    );
    assert_eq!(
        snapshot_task_and_turn_state(&session, earlier_task_id, earlier_turn_id).unwrap(),
        (TaskState::Pending, TurnState::Pending)
    );
}

fn in_workspace<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    with_current_dir(path, operation)
}

struct MockResponse {
    status: u16,
    body: String,
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local mock");
    let address = listener.local_addr().expect("read mock address");
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

fn redirecting_mock_server() -> (SocketAddr, TcpListener, JoinHandle<Vec<u8>>) {
    let redirect_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind redirect target");
    redirect_listener
        .set_nonblocking(true)
        .expect("make redirect target nonblocking");
    let redirect_address = redirect_listener
        .local_addr()
        .expect("read redirect target address");
    let redirect_to = format!("http://{redirect_address}/capture");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind redirect source");
    let address = listener.local_addr().expect("read redirect source address");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept initial profile request");
        let request = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {redirect_to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("write redirect response");
        request
    });
    (address, redirect_listener, handle)
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read model request");
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

fn request_header(request: &[u8], wanted_name: &str) -> Option<String> {
    let header_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    String::from_utf8_lossy(&request[..header_end])
        .lines()
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted_name)
                .then(|| value.trim().to_owned())
        })
}

fn task_args(max_actions: &str) -> Vec<OsString> {
    [
        "--goal",
        "Create a result file",
        "--max-actions",
        max_actions,
        "--model",
        "mock-model",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[test]
fn caller_directed_cli_uses_explicit_workspace_without_changing_current_directory() {
    let temporary = TempDir::new();
    let process_directory = temporary.path().join("caller directory");
    let workspace = temporary.path().join("selected workspace");
    fs::create_dir(&process_directory).expect("create caller directory");
    fs::create_dir(&workspace).expect("create selected workspace");

    let (code, output, error, current_directory) = in_workspace(&process_directory, || {
        let original_directory = std::env::current_dir().expect("read current directory");
        let mut args = [
            "--goal",
            "Create a file in the selected workspace",
            "--max-actions",
            "2",
            "--workspace",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.push(workspace.as_os_str().to_owned());
        let mut input =
            Cursor::new(b"create \"selected file.txt\" created\ncomplete saved\n".to_vec());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .expect("run caller-directed task");
        let current_directory = std::env::current_dir().expect("read current directory after task");
        assert_eq!(current_directory, original_directory);
        (
            code,
            String::from_utf8(output).expect("UTF-8 output"),
            String::from_utf8(error).expect("UTF-8 error output"),
            current_directory,
        )
    });

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert_eq!(
        fs::read_to_string(workspace.join("selected file.txt")).expect("read selected result"),
        "created"
    );
    assert!(!process_directory.join("selected file.txt").exists());
    assert_eq!(
        current_directory,
        fs::canonicalize(&process_directory).unwrap()
    );
}

#[test]
fn caller_directed_cli_without_workspace_uses_current_directory() {
    let workspace = TempDir::new();
    let (code, output, error, current_directory) = in_workspace(workspace.path(), || {
        let original_directory = std::env::current_dir().expect("read current directory");
        let args = [
            "--goal",
            "Create a file in the current directory",
            "--max-actions",
            "2",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        let mut input = Cursor::new(b"create default.txt created\ncomplete saved\n".to_vec());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .expect("run caller-directed task");
        let current_directory = std::env::current_dir().expect("read current directory after task");
        assert_eq!(current_directory, original_directory);
        (
            code,
            String::from_utf8(output).expect("UTF-8 output"),
            String::from_utf8(error).expect("UTF-8 error output"),
            current_directory,
        )
    });

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("default.txt")).expect("read default result"),
        "created"
    );
    assert_eq!(
        current_directory,
        fs::canonicalize(workspace.path()).unwrap()
    );
}

#[test]
fn task_cli_rejects_missing_and_invalid_workspace_before_lifecycle() {
    let temporary = TempDir::new();
    let process_directory = temporary.path().join("caller directory");
    let file_workspace = temporary.path().join("workspace file");
    let missing_workspace = temporary.path().join("missing workspace");
    fs::create_dir(&process_directory).expect("create caller directory");
    fs::write(&file_workspace, "not a directory").expect("create workspace file");

    let (results, current_directory) = in_workspace(&process_directory, || {
        let original_directory = std::env::current_dir().expect("read current directory");
        let mut cases = vec![[
            "--goal",
            "Check workspace input",
            "--max-actions",
            "1",
            "--workspace",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()];
        for workspace in [&missing_workspace, &file_workspace] {
            let mut args = [
                "--goal",
                "Check workspace input",
                "--max-actions",
                "1",
                "--workspace",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
            args.push(workspace.as_os_str().to_owned());
            cases.push(args);
        }

        let results = cases
            .into_iter()
            .map(|args| {
                let mut input = Cursor::new(Vec::<u8>::new());
                let mut output = Vec::new();
                let mut error = Vec::new();
                let code = run_task_with_endpoint(
                    &args,
                    &mut input,
                    &mut output,
                    &mut error,
                    SocketAddr::from(([127, 0, 0, 1], 0)),
                )
                .expect("reject invalid workspace before task start");
                (code, output, error)
            })
            .collect::<Vec<_>>();
        let current_directory = std::env::current_dir().expect("read current directory after task");
        assert_eq!(current_directory, original_directory);
        (results, current_directory)
    });

    assert_eq!(
        current_directory,
        fs::canonicalize(&process_directory).unwrap()
    );
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, 2);
    assert!(String::from_utf8_lossy(&results[0].2)
        .contains("the following argument is required: --workspace"));
    for (code, output, error) in &results[1..] {
        assert_eq!(*code, 2);
        assert!(String::from_utf8_lossy(error).contains("invalid workspace path"));
        assert!(output.is_empty());
    }
}

#[test]
fn model_cli_reports_successful_task_and_turn_states_and_outcomes() {
    let (endpoint, server) = mock_server(vec![
        MockResponse {
            status: 200,
            body: tool_response(
                "create_file",
                json!({"path":"result.txt","contents":"done"}),
            ),
        },
        MockResponse {
            status: 200,
            body: tool_response("complete_task", json!({"result":"Created result.txt."})),
        },
    ]);
    let workspace = TempDir::new();
    let (code, output, error) = in_workspace(workspace.path(), || {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &task_args("2"),
            &mut input,
            &mut output,
            &mut error,
            endpoint,
        )
        .expect("run model task");
        (code, output, error)
    });
    let output = String::from_utf8(output).expect("UTF-8 output");
    let error = String::from_utf8(error).expect("UTF-8 error output");
    assert_eq!(code, 0);
    assert!(output.contains("Model selected action: create_file"));
    assert!(output.contains("Model selected action: complete_task"));
    assert!(output.contains("Task state: succeeded"));
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Turn state: succeeded"));
    assert!(output.contains("Turn outcome: success: Created result.txt."));
    assert!(output.contains("Actions: 2/2"));
    assert!(error.contains("model decision 1/2"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).expect("read result"),
        "done"
    );
    assert_eq!(server.join().expect("join mock server").len(), 2);
}

#[test]
fn model_cli_forwards_captured_root_guidance_to_each_provider_request() {
    let workspace = TempDir::new();
    let guidance = "Use the repository's existing file naming convention.";
    fs::write(workspace.path().join("AGENTS.md"), guidance).expect("write root guidance");
    let (endpoint, server) = mock_server(vec![
        MockResponse {
            status: 200,
            body: tool_response(
                "create_file",
                json!({"path":"result.txt","contents":"done"}),
            ),
        },
        MockResponse {
            status: 200,
            body: tool_response("complete_task", json!({"result":"Created result.txt."})),
        },
    ]);
    let mut args = task_args("2");
    args.push(OsString::from("--workspace"));
    args.push(workspace.path().as_os_str().to_owned());
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
        .expect("run the model-backed CLI task");
    assert_eq!(code, 0);
    assert!(String::from_utf8(output)
        .expect("UTF-8 CLI output")
        .contains("Task state: succeeded"));

    let requests = server.join().expect("join mock model server");
    assert_eq!(requests.len(), 2);
    let first = request_json(&requests[0]);
    let second = request_json(&requests[1]);
    let section = "\n\nRoot AGENTS.md project guidance (lower priority):\n";
    for request in [&first, &second] {
        let prompt = request["messages"][1]["content"]
            .as_str()
            .expect("provider task prompt");
        let (_, guidance_context) = prompt.split_once(section).expect("guidance section");
        assert!(guidance_context.starts_with(guidance));
        assert_eq!(guidance_context.matches(guidance).count(), 1);
        assert!(prompt.contains("Task goal:\nCreate a result file"));
    }
    assert!(first["messages"][1]["content"]
        .as_str()
        .expect("first prompt")
        .contains("Action slots remaining, including this decision: 2"));
    assert!(second["messages"][1]["content"]
        .as_str()
        .expect("second prompt")
        .contains("Action slots remaining, including this decision: 1"));
    assert!(second["messages"][1]["content"]
        .as_str()
        .expect("second prompt")
        .contains("file_created"));
}

#[test]
fn model_cli_reconsiders_nested_edits_with_only_applicable_guidance() {
    let workspace = TempDir::new();
    fs::create_dir_all(workspace.path().join("src/deep")).expect("create nested workspace");
    fs::create_dir_all(workspace.path().join("tests")).expect("create sibling workspace");
    fs::write(workspace.path().join("AGENTS.md"), "root CLI guidance")
        .expect("write root guidance");
    fs::write(workspace.path().join("src/AGENTS.md"), "src CLI guidance")
        .expect("write src guidance");
    fs::write(
        workspace.path().join("src/deep/AGENTS.md"),
        "deep CLI guidance",
    )
    .expect("write deep guidance");
    fs::write(
        workspace.path().join("tests/AGENTS.md"),
        "CLI sibling sentinel",
    )
    .expect("write sibling guidance");
    let (endpoint, server) = mock_server(vec![
        MockResponse {
            status: 200,
            body: tool_response(
                "create_file",
                json!({"path":"src/deep/generated.rs","contents":"done"}),
            ),
        },
        MockResponse {
            status: 200,
            body: tool_response(
                "create_file",
                json!({"path":"src/deep/generated.rs","contents":"done"}),
            ),
        },
        MockResponse {
            status: 200,
            body: tool_response("complete_task", json!({"result":"Created generated.rs."})),
        },
    ]);
    let mut args = task_args("3");
    args.push(OsString::from("--workspace"));
    args.push(workspace.path().as_os_str().to_owned());
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
        .expect("run the guidance-aware model task");

    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(workspace.path().join("src/deep/generated.rs"))
            .expect("read generated file"),
        "done"
    );
    let requests = server.join().expect("join mock model server");
    assert_eq!(requests.len(), 3);
    let first_request = request_json(&requests[0]);
    let first_prompt = first_request["messages"][1]["content"]
        .as_str()
        .expect("initial edit proposal");
    assert!(first_prompt.contains("root CLI guidance"));
    assert!(!first_prompt.contains("src CLI guidance"));
    let second_request = request_json(&requests[1]);
    let second_prompt = second_request["messages"][1]["content"]
        .as_str()
        .expect("guidance-aware edit decision");
    let root_at = second_prompt
        .find("root CLI guidance")
        .expect("root guidance is included");
    let src_at = second_prompt
        .find("src CLI guidance")
        .expect("src guidance is included");
    let deep_at = second_prompt
        .find("deep CLI guidance")
        .expect("deep guidance is included");
    assert!(root_at < src_at && src_at < deep_at);
    assert!(!second_prompt.contains("CLI sibling sentinel"));
    assert!(String::from_utf8(error)
        .expect("UTF-8 progress output")
        .contains("model decision 3/3"));
}

#[test]
#[cfg(unix)]
fn model_cli_stops_with_a_typed_guidance_error_before_dispatch() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new();
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let outside = fixture.path().join("outside.txt");
    fs::write(&outside, "outside canary").expect("write outside canary");
    symlink(&outside, workspace.join("AGENTS.md")).expect("create guidance symlink");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind unused model endpoint");
    listener
        .set_nonblocking(true)
        .expect("make model endpoint nonblocking");
    let endpoint = listener.local_addr().expect("read model endpoint");
    let mut args = task_args("1");
    args.push(OsString::from("--workspace"));
    args.push(workspace.as_os_str().to_owned());
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error_output = Vec::new();

    let error = run_task_with_endpoint(&args, &mut input, &mut output, &mut error_output, endpoint)
        .expect_err("reject the symlink before dispatching the model");

    assert!(matches!(
        error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<GoalTurnError>()),
        Some(GoalTurnError::RootProjectGuidance(
            WorkspaceGuidanceReadError::Symlink
        ))
    ));
    assert!(
        matches!(listener.accept(), Err(ref cause) if cause.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(
        String::from_utf8(output).expect("task output is UTF-8"),
        "Permission mode: Ask\n"
    );
    assert_eq!(
        fs::read_to_string(outside).expect("read outside canary"),
        "outside canary"
    );
}

#[test]
fn resumed_model_cli_uses_the_stored_description_with_the_local_mock_server() {
    let (endpoint, server) = mock_server(vec![MockResponse {
        status: 200,
        body: tool_response("complete_task", json!({"result":"resumed task complete"})),
    }]);
    let temporary = TempDir::new();
    let workspace = temporary.path().join("saved workspace");
    fs::create_dir(&workspace).expect("create checkpoint workspace");
    let checkpoint_path = temporary.path().join("saved task.json");
    let stored_description = "Use the description stored in the checkpoint";
    let mut session = TaskSession::new_in_workspace(&workspace).expect("bind checkpoint workspace");
    session
        .create_task(stored_description)
        .transition(TaskState::Running)
        .expect("start checkpoint task");
    fs::write(
        &checkpoint_path,
        session
            .export_checkpoint()
            .expect("export running checkpoint"),
    )
    .expect("write checkpoint");

    let args = vec![
        OsString::from("--resume"),
        checkpoint_path.as_os_str().to_owned(),
        OsString::from("--max-actions"),
        OsString::from("2"),
        OsString::from("--workspace"),
        workspace.as_os_str().to_owned(),
        OsString::from("--model"),
        OsString::from("mock-model"),
    ];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
        .expect("run resumed model task");
    let requests = server.join().expect("join mock server");

    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&error));
    assert!(String::from_utf8_lossy(&output).contains("Task outcome: succeeded"));
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8_lossy(&requests[0]);
    let (_, body) = request.split_once("\r\n\r\n").expect("HTTP request body");
    let request: Value = serde_json::from_str(body).expect("decode model request");
    let prompt = request["messages"][1]["content"]
        .as_str()
        .expect("model prompt");
    assert!(prompt.contains(stored_description));
    assert!(prompt.contains("Prior workspace observations (JSON):\n[]"));
}

#[test]
fn resume_cli_preserves_malformed_checkpoint_message_and_exit_code() {
    let temporary = TempDir::new();
    let checkpoint_path = temporary.path().join("bad checkpoint.json");
    fs::write(&checkpoint_path, b"not a checkpoint").expect("write malformed checkpoint");
    let args = vec![
        OsString::from("--resume"),
        checkpoint_path.as_os_str().to_owned(),
        OsString::from("--max-actions"),
        OsString::from("1"),
    ];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .expect("report malformed checkpoint");

    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(String::from_utf8_lossy(&error).contains(&format!(
        "cannot restore checkpoint '{}': invalid TaskSession checkpoint",
        checkpoint_path.display()
    )));
}

#[test]
fn resume_cli_preserves_workspace_binding_error_message_and_exit_code() {
    let temporary = TempDir::new();
    let stored_workspace = temporary.path().join("stored workspace");
    let requested_workspace = temporary.path().join("requested workspace");
    fs::create_dir(&stored_workspace).expect("create stored workspace");
    fs::create_dir(&requested_workspace).expect("create requested workspace");
    let checkpoint_path = temporary.path().join("saved task.json");
    let mut session = TaskSession::new_in_workspace(&stored_workspace).expect("bind workspace");
    let task = session.create_task("resume with its original workspace");
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    fs::write(
        &checkpoint_path,
        session.export_checkpoint().expect("export checkpoint"),
    )
    .expect("write checkpoint");
    let args = vec![
        OsString::from("--resume"),
        checkpoint_path.as_os_str().to_owned(),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--workspace"),
        requested_workspace.as_os_str().to_owned(),
    ];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .expect("report workspace binding mismatch");

    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(String::from_utf8_lossy(&error).contains(&format!(
        "cannot restore checkpoint '{}': workspace binding for task '{task_id}' does not match the checkpoint",
        checkpoint_path.display()
    )));
}

#[test]
fn model_cli_uses_explicit_workspace_without_changing_current_directory() {
    let _lock = crate::test_support::lock_current_dir();
    let _unused_gemini_key = RestoreEnvironmentVariable::set("GEMINI_API_KEY", "");
    let (endpoint, server) = mock_server(vec![
        MockResponse {
            status: 200,
            body: tool_response(
                "create_file",
                json!({"path":"model result.txt","contents":"selected workspace"}),
            ),
        },
        MockResponse {
            status: 200,
            body: tool_response(
                "complete_task",
                json!({"result":"Created model result.txt."}),
            ),
        },
    ]);
    let temporary = TempDir::new();
    let process_directory = temporary.path().join("caller directory");
    let workspace = temporary.path().join("selected workspace");
    fs::create_dir(&process_directory).expect("create caller directory");
    fs::create_dir(&workspace).expect("create selected workspace");

    let (code, output, error, current_directory) =
        crate::test_support::with_current_dir_under_lock(&process_directory, || {
            let original_directory = std::env::current_dir().expect("read current directory");
            let mut args = task_args("2");
            args.push(OsString::from("--workspace"));
            args.push(workspace.as_os_str().to_owned());
            let mut input = Cursor::new(Vec::<u8>::new());
            let mut output = Vec::new();
            let mut error = Vec::new();
            let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
                .expect("run mocked model task");
            let current_directory =
                std::env::current_dir().expect("read current directory after task");
            assert_eq!(current_directory, original_directory);
            (
                code,
                String::from_utf8(output).expect("UTF-8 output"),
                String::from_utf8(error).expect("UTF-8 error output"),
                current_directory,
            )
        });

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Model selected action: create_file"));
    assert!(output.contains("Task outcome: succeeded"));
    assert_eq!(
        fs::read_to_string(workspace.join("model result.txt")).expect("read model result"),
        "selected workspace"
    );
    assert!(!process_directory.join("model result.txt").exists());
    assert_eq!(
        current_directory,
        fs::canonicalize(&process_directory).unwrap()
    );
    assert_eq!(server.join().expect("join mock server").len(), 2);
}

#[test]
fn model_cli_marks_an_uncompleted_budget_exhausted_task_failed() {
    let (endpoint, server) = mock_server(vec![MockResponse {
        status: 200,
        body: tool_response(
            "create_file",
            json!({"path":"result.txt","contents":"created"}),
        ),
    }]);
    let workspace = TempDir::new();
    let (code, output) = in_workspace(workspace.path(), || {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &task_args("1"),
            &mut input,
            &mut output,
            &mut error,
            endpoint,
        )
        .expect("run bounded model task");
        (code, output)
    });
    let output = String::from_utf8(output).expect("UTF-8 output");
    assert_eq!(code, 1);
    assert!(output.contains("Task state: failed"));
    assert!(output.contains("Task outcome: failed"));
    assert!(output.contains("Turn state: failed"));
    assert!(output.contains("Turn outcome: failure: action budget exhausted after 1 actions"));
    assert!(output.contains("Actions: 1/1"));
    assert!(!output.contains("Task outcome: succeeded"));
    assert_eq!(server.join().expect("join mock server").len(), 1);
}

#[test]
fn model_cli_reports_connection_http_and_decode_failures_as_failed_task() {
    let (http_endpoint, http_server) = mock_server(vec![MockResponse {
        status: 503,
        body: "service unavailable".to_owned(),
    }]);
    let (decode_endpoint, decode_server) = mock_server(vec![MockResponse {
        status: 200,
        body: "{invalid json".to_owned(),
    }]);
    let closed_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind closed port");
    let closed_endpoint = closed_listener.local_addr().expect("read closed port");
    drop(closed_listener);

    for (endpoint, server) in [
        (closed_endpoint, None),
        (http_endpoint, Some(http_server)),
        (decode_endpoint, Some(decode_server)),
    ] {
        let workspace = TempDir::new();
        let (code, output) = in_workspace(workspace.path(), || {
            let mut input = Cursor::new(Vec::<u8>::new());
            let mut output = Vec::new();
            let mut error = Vec::new();
            let code = run_task_with_endpoint(
                &task_args("1"),
                &mut input,
                &mut output,
                &mut error,
                endpoint,
            )
            .expect("report model failure through task outcomes");
            (code, output)
        });
        let output = String::from_utf8(output).expect("UTF-8 task output");
        assert_eq!(code, 1);
        assert!(output.contains("Task state: failed"));
        assert!(output.contains("Task outcome: failed"));
        assert!(output.contains("Turn state: failed"));
        assert!(output.contains("Turn outcome: failure:"));
        assert!(!output.contains("Task outcome: succeeded"));
        if let Some(server) = server {
            assert_eq!(server.join().expect("join mock server").len(), 1);
        }
    }
}

#[test]
fn caller_directed_cli_reports_the_final_turn_state_after_multiple_turns() {
    let workspace = TempDir::new();
    let args = [
        "--goal",
        "Continue into a failed turn",
        "--max-actions",
        "2",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let (code, output) = in_workspace(workspace.path(), || {
        let mut input = Cursor::new(b"continue\nnot-an-action\n".to_vec());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .expect("run caller-directed multi-turn task");
        (code, output)
    });
    let output = String::from_utf8(output).expect("UTF-8 task output");

    assert_eq!(code, 1);
    assert!(output.contains("Turn results (in order):"));
    assert!(output.contains("Turn 1 actions: 1"));
    assert!(output.contains("Turn 2 actions: 1"));
    assert!(output.contains("Task state: failed"));
    assert!(output.contains("Turn state: failed"));
}

#[test]
fn explicit_ollama_provider_keeps_the_local_model_path() {
    let (endpoint, server) = mock_server(vec![MockResponse {
        status: 200,
        body: tool_response("complete_task", json!({"result":"local complete"})),
    }]);
    let workspace = TempDir::new();
    let mut args = task_args("1");
    args.extend([OsString::from("--provider"), OsString::from("ollama")]);
    let (code, output, error) = in_workspace(workspace.path(), || {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
            .expect("run explicit local Ollama task");
        (code, output, error)
    });

    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&error));
    assert!(String::from_utf8_lossy(&output).contains("Task outcome: succeeded"));
    assert_eq!(server.join().expect("join local Ollama mock").len(), 1);
}

#[test]
fn mistral_credentials_are_loaded_only_for_explicit_selection() {
    let _lock = crate::test_support::lock_environment();
    let key = "local-mistral-test-key";
    let _restore_key = RestoreEnvironmentVariable::set("MISTRAL_API_KEY", key);
    let provider = ModelProvider::parse("mistral").expect("parse Mistral provider");
    let source = prepare_model_source(
        Some("test-model".to_owned()),
        Some(provider),
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    );

    assert!(
        source.is_ok(),
        "explicit Mistral selection should load its key"
    );
}

#[test]
fn saved_credentials_are_used_by_explicit_model_selection_when_enabled() {
    let _lock = crate::test_support::lock_environment();
    let temporary = TempDir::new();
    let config_home = temporary.path().to_string_lossy().into_owned();
    let _restore_api_key = RestoreEnvironmentVariable::remove("OPENAI_API_KEY");
    let _restore_config_home = RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", &config_home);

    let mut credentials = CredentialStore::empty();
    credentials.insert("OPENAI_API_KEY", "saved-openai-test-key".to_owned());
    let credentials_path = temporary.path().join("krypper/credentials.json");
    save_credentials(&credentials_path, &mut credentials).expect("save test credentials");

    let provider = ModelProvider::parse("openai").expect("parse OpenAI provider");
    let endpoint = SocketAddr::from(([127, 0, 0, 1], 0));
    let source = prepare_model_source_with_credentials(
        Some("gpt-4.1".to_owned()),
        Some(provider),
        1,
        endpoint,
        true,
    )
    .expect("load saved credentials for explicit model selection")
    .expect("explicit model selection creates a provider source");
    assert_eq!(source.adapter_id(), AdapterId::RESPONSES);

    assert!(matches!(
        prepare_model_source_with_credentials(
            Some("gpt-4.1".to_owned()),
            Some(provider),
            1,
            endpoint,
            false,
        ),
        Err("OPENAI_API_KEY must be set and nonempty for --provider openai")
    ));
}

#[test]
fn hosted_keys_do_not_change_caller_directed_or_default_ollama_tasks() {
    let _lock = crate::test_support::lock_current_dir();
    let key = "local-mistral-test-key";
    let _restore_key = RestoreEnvironmentVariable::set("MISTRAL_API_KEY", key);
    let xai_key = "local-xai-test-key";
    let _restore_xai_key = RestoreEnvironmentVariable::set("XAI_API_KEY", xai_key);

    let caller_workspace = TempDir::new();
    let caller_args = [
        OsString::from("--goal"),
        OsString::from("Finish a caller-directed task"),
        OsString::from("--max-actions"),
        OsString::from("1"),
    ];
    let (caller_code, caller_output, caller_error) =
        crate::test_support::with_current_dir_under_lock(caller_workspace.path(), || {
            let mut input = Cursor::new(b"complete caller-directed\n".to_vec());
            let mut output = Vec::new();
            let mut error = Vec::new();
            let code = run_task_with_endpoint(
                &caller_args,
                &mut input,
                &mut output,
                &mut error,
                SocketAddr::from(([127, 0, 0, 1], 0)),
            )
            .expect("run caller-directed task without Mistral selection");
            (code, output, error)
        });
    assert_eq!(caller_code, 0, "{}", String::from_utf8_lossy(&caller_error));
    assert!(String::from_utf8_lossy(&caller_output).contains("Task outcome: succeeded"));
    assert!(!String::from_utf8_lossy(&caller_output).contains(key));
    assert!(!String::from_utf8_lossy(&caller_error).contains(key));
    assert!(!String::from_utf8_lossy(&caller_output).contains(xai_key));
    assert!(!String::from_utf8_lossy(&caller_error).contains(xai_key));

    let (endpoint, server) = mock_server(vec![MockResponse {
        status: 200,
        body: tool_response("complete_task", json!({"result":"local Ollama"})),
    }]);
    let ollama_workspace = TempDir::new();
    let (ollama_code, ollama_output, ollama_error) =
        crate::test_support::with_current_dir_under_lock(ollama_workspace.path(), || {
            let mut input = Cursor::new(Vec::<u8>::new());
            let mut output = Vec::new();
            let mut error = Vec::new();
            let code = run_task_with_endpoint(
                &task_args("1"),
                &mut input,
                &mut output,
                &mut error,
                endpoint,
            )
            .expect("run default local Ollama task without Mistral selection");
            (code, output, error)
        });
    assert_eq!(ollama_code, 0, "{}", String::from_utf8_lossy(&ollama_error));
    assert!(String::from_utf8_lossy(&ollama_output).contains("Task outcome: succeeded"));
    assert!(!String::from_utf8_lossy(&ollama_output).contains(key));
    assert!(!String::from_utf8_lossy(&ollama_error).contains(key));
    assert!(!String::from_utf8_lossy(&ollama_output).contains(xai_key));
    assert!(!String::from_utf8_lossy(&ollama_error).contains(xai_key));
    let requests = server.join().expect("join local Ollama mock");
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
    assert!(request.starts_with("post /api/chat "));
    assert!(!request.contains("authorization:"));
    assert!(!request.contains(key));
    assert!(!request.contains(xai_key));
}

#[test]
fn xai_credentials_are_loaded_only_for_explicit_selection() {
    let _lock = crate::test_support::lock_environment();
    let _restore_key = RestoreEnvironmentVariable::set("XAI_API_KEY", "local-xai-test-key");
    let provider = ModelProvider::parse("xai").expect("parse xAI provider");

    let explicit = prepare_model_source(
        Some("grok-local-test".to_owned()),
        Some(provider),
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    );
    let caller_directed =
        prepare_model_source(None, None, 1, SocketAddr::from(([127, 0, 0, 1], 0)));
    let implicit_ollama = prepare_model_source(
        Some("local-model".to_owned()),
        None,
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    );

    assert_eq!(
        explicit
            .expect("explicit xAI provider is prepared")
            .expect("explicit model source exists")
            .adapter_id(),
        AdapterId::RESPONSES
    );
    assert!(matches!(caller_directed, Ok(None)));
    assert_eq!(
        implicit_ollama
            .expect("implicit Ollama provider is prepared")
            .expect("implicit model source exists")
            .adapter_id(),
        AdapterId::OLLAMA
    );
}

#[test]
fn anthropic_workspace_id_environment_is_optional_and_validated() {
    let _lock = crate::test_support::lock_environment();
    let _restore_key =
        RestoreEnvironmentVariable::set("ANTHROPIC_API_KEY", "local-anthropic-test-key");
    let provider = ModelProvider::parse("anthropic").expect("parse Anthropic provider");
    let prepare = || {
        prepare_model_source(
            Some("claude-local-test".to_owned()),
            Some(provider),
            1,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
    };

    {
        let _unset = RestoreEnvironmentVariable::remove("ANTHROPIC_WORKSPACE_ID");
        assert_eq!(
            prepare()
                .expect("Anthropic source is prepared without an optional workspace")
                .expect("Anthropic model source exists")
                .adapter_id(),
            AdapterId::ANTHROPIC
        );
    }
    {
        let _set = RestoreEnvironmentVariable::set(
            "ANTHROPIC_WORKSPACE_ID",
            "wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ",
        );
        assert_eq!(
            prepare()
                .expect("Anthropic source is prepared with a valid workspace")
                .expect("Anthropic model source exists")
                .adapter_id(),
            AdapterId::ANTHROPIC
        );
    }
    for invalid in ["", "a b", "a\nb", "wrk/space"] {
        let _set = RestoreEnvironmentVariable::set("ANTHROPIC_WORKSPACE_ID", invalid);
        assert_eq!(
            prepare().err(),
            Some("ANTHROPIC_WORKSPACE_ID must be 1-128 ASCII letters, digits, '-', '_' or '.'"),
            "{invalid:?}"
        );
    }
}

#[test]
fn hosted_provider_credentials_are_required_before_task_lifecycle_creation() {
    let _lock = crate::test_support::lock_environment();
    let provider_variables = [
        "GEMINI_API_KEY",
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_API_TOKEN",
        "CLOUDFLARE_AI_GATEWAY_ID",
        "GROQ_API_KEY",
        "OPENROUTER_API_KEY",
        "DEEPSEEK_API_KEY",
        "ZAI_API_KEY",
        "MISTRAL_API_KEY",
    ];
    for (provider, missing_name, supplemental) in [
        ("gemini", "GEMINI_API_KEY", None),
        ("openai", "OPENAI_API_KEY", None),
        ("xai", "XAI_API_KEY", None),
        ("anthropic", "ANTHROPIC_API_KEY", None),
        (
            "cloudflare",
            "CLOUDFLARE_ACCOUNT_ID",
            Some(("CLOUDFLARE_API_TOKEN", "test-token")),
        ),
        (
            "cloudflare",
            "CLOUDFLARE_API_TOKEN",
            Some(("CLOUDFLARE_ACCOUNT_ID", "account-123")),
        ),
        (
            "cloudflare-ai-gateway",
            "CLOUDFLARE_ACCOUNT_ID",
            Some(("CLOUDFLARE_API_TOKEN", "test-token")),
        ),
        (
            "cloudflare-ai-gateway",
            "CLOUDFLARE_API_TOKEN",
            Some(("CLOUDFLARE_ACCOUNT_ID", "account-123")),
        ),
        ("groq", "GROQ_API_KEY", None),
        ("openrouter", "OPENROUTER_API_KEY", None),
        ("deepseek", "DEEPSEEK_API_KEY", None),
        ("zai", "ZAI_API_KEY", None),
        ("mistral", "MISTRAL_API_KEY", None),
    ] {
        let _restores = provider_variables
            .iter()
            .map(|name| RestoreEnvironmentVariable::remove(name))
            .collect::<Vec<_>>();
        let _supplemental_override =
            supplemental.map(|(name, value)| RestoreEnvironmentVariable::set(name, value));
        let workspace = TempDir::new();
        let checkpoint = workspace.path().join("new checkpoint.json");
        let mut args = [
            "--goal",
            "This must fail during provider preflight",
            "--max-actions",
            "1",
            "--model",
            "test-model",
            "--provider",
            provider,
            "--workspace",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.push(workspace.path().as_os_str().to_owned());
        args.push(OsString::from("--checkpoint-out"));
        args.push(checkpoint.as_os_str().to_owned());
        let mut input = Cursor::new(b"complete must not run\n".to_vec());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .expect("report missing provider credentials as a CLI error");
        let output = String::from_utf8(output).expect("UTF-8 task output");
        let error = String::from_utf8(error).expect("UTF-8 CLI error");

        assert_eq!(code, 2);
        assert!(error.contains(missing_name));
        assert!(output.is_empty());
        assert_eq!(input.position(), 0);
        assert!(!error.contains("model decision"));
        assert!(!checkpoint.exists());
        assert!(!workspace.path().join("should-not-exist.txt").exists());
    }
}

#[test]
fn cloudflare_ai_gateway_defaults_and_validates_gateway_id_before_task_lifecycle_creation() {
    let _lock = crate::test_support::lock_environment();
    let _restore_account = RestoreEnvironmentVariable::set("CLOUDFLARE_ACCOUNT_ID", "account-123");
    let _restore_token =
        RestoreEnvironmentVariable::set("CLOUDFLARE_API_TOKEN", "local-test-token");
    let provider = ModelProvider::parse("cloudflare-ai-gateway")
        .expect("parse Cloudflare AI Gateway provider");

    {
        let _unset_gateway = RestoreEnvironmentVariable::remove("CLOUDFLARE_AI_GATEWAY_ID");
        assert!(matches!(
            prepare_model_source(
                Some("openai/gpt-4.1".to_owned()),
                Some(provider),
                1,
                SocketAddr::from(([127, 0, 0, 1], 0)),
            ),
            Ok(Some(source)) if source.adapter_id() == AdapterId::OPENAI_COMPATIBLE
        ));
    }

    for invalid in ["", "gateway id", "gateway\nid", &"g".repeat(65)] {
        let _gateway_id = RestoreEnvironmentVariable::set("CLOUDFLARE_AI_GATEWAY_ID", invalid);
        assert_eq!(
            prepare_model_source(
                Some("openai/gpt-4.1".to_owned()),
                Some(provider),
                1,
                SocketAddr::from(([127, 0, 0, 1], 0)),
            )
            .err(),
            Some("CLOUDFLARE_AI_GATEWAY_ID must be 1-64 visible ASCII characters without spaces"),
            "{invalid:?}"
        );
    }
}

#[test]
fn cloudflare_account_id_is_validated_before_task_lifecycle_creation() {
    let _lock = crate::test_support::lock_environment();
    let _restore_account = RestoreEnvironmentVariable::set("CLOUDFLARE_ACCOUNT_ID", "bad/path");
    let _restore_token =
        RestoreEnvironmentVariable::set("CLOUDFLARE_API_TOKEN", "local-test-token");
    let workspace = TempDir::new();
    let checkpoint = workspace.path().join("new checkpoint.json");
    let mut args = [
        "--goal",
        "Reject an invalid Cloudflare account path",
        "--max-actions",
        "1",
        "--model",
        "test-model",
        "--provider",
        "cloudflare",
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(workspace.path().as_os_str().to_owned());
    args.push(OsString::from("--checkpoint-out"));
    args.push(checkpoint.as_os_str().to_owned());
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .expect("report invalid Cloudflare account ID as a CLI error");
    let error = String::from_utf8(error).expect("UTF-8 CLI error");

    assert_eq!(code, 2);
    assert!(error.contains("CLOUDFLARE_ACCOUNT_ID"));
    assert!(output.is_empty());
    assert!(!checkpoint.exists());
}

#[test]
fn gemini_model_path_injection_is_rejected_before_task_lifecycle_creation() {
    let _lock = crate::test_support::lock_environment();
    let _restore_key = RestoreEnvironmentVariable::set("GEMINI_API_KEY", "local-test-key");
    let workspace = TempDir::new();
    let checkpoint = workspace.path().join("new checkpoint.json");
    let mut args = [
        "--goal",
        "Reject a Gemini model path injection",
        "--max-actions",
        "1",
        "--model",
        "gemini/other",
        "--provider",
        "gemini",
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(workspace.path().as_os_str().to_owned());
    args.extend([
        OsString::from("--checkpoint-out"),
        checkpoint.as_os_str().to_owned(),
    ]);
    let mut input = Cursor::new(b"complete must not run\n".to_vec());
    let mut output = Vec::new();
    let mut error = Vec::new();

    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .expect("report invalid Gemini model as a CLI error");
    let error = String::from_utf8(error).expect("UTF-8 CLI error");

    assert_eq!(code, 2);
    assert!(error.contains("safe path component"));
    assert_eq!(input.position(), 0);
    assert!(output.is_empty());
    assert!(!checkpoint.exists());
}

#[test]
fn provider_option_requires_a_model_and_rejects_unknown_names() {
    for (provider, model, expected) in [
        ("gemini", None, "--model is required with --provider"),
        ("openai", None, "--model is required with --provider"),
        ("xai", None, "--model is required with --provider"),
        ("cloudflare", None, "--model is required with --provider"),
        (
            "cloudflare-ai-gateway",
            None,
            "--model is required with --provider",
        ),
        ("groq", None, "--model is required with --provider"),
        ("openrouter", None, "--model is required with --provider"),
        ("deepseek", None, "--model is required with --provider"),
        ("zai", None, "--model is required with --provider"),
        ("mistral", None, "--model is required with --provider"),
        ("Unknown", Some("test-model"), "unsupported provider"),
    ] {
        let mut args = vec![
            OsString::from("--goal"),
            OsString::from("Check provider selection"),
            OsString::from("--max-actions"),
            OsString::from("1"),
            OsString::from("--provider"),
            OsString::from(provider),
        ];
        if let Some(model) = model {
            args.extend([OsString::from("--model"), OsString::from(model)]);
        }
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .expect("report invalid provider selection");

        assert_eq!(code, 2);
        assert!(String::from_utf8_lossy(&error).contains(expected));
        assert!(output.is_empty());
    }
}

#[test]
fn task_help_documents_provider_selection_and_network_disclosure() {
    let args = [OsString::from("--help")];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 11434)),
    )
    .expect("print task help");
    let help = String::from_utf8(output).expect("UTF-8 task help");

    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&error));
    assert!(help.contains("--provider NAME"));
    assert!(help.contains("--model-usage-jsonl"));
    assert!(help.contains(
        "choose ollama, gemini, openai, xai, anthropic, cloudflare, cloudflare-ai-gateway, groq, openrouter, deepseek, zai, or mistral"
    ));
    assert!(help.contains("cf-aig-gateway-id"));
    for credential in [
        "GEMINI_API_KEY",
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_API_TOKEN",
        "CLOUDFLARE_AI_GATEWAY_ID",
        "GROQ_API_KEY",
        "OPENROUTER_API_KEY",
        "DEEPSEEK_API_KEY",
        "ZAI_API_KEY",
        "MISTRAL_API_KEY",
    ] {
        assert!(
            help.contains(credential),
            "missing {credential} in task help"
        );
    }
    assert!(help.contains("trusted per-user profiles use their configured endpoint"));
    assert!(help.contains("optional ANTHROPIC_WORKSPACE_ID"));
    assert!(help.contains(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
    ));
    assert!(help.contains("https://api.x.ai/v1/responses"));
    assert!(help.contains("Gemini model name must be one safe path component"));
    assert!(help.contains("may incur provider charges"));
    assert!(help.contains("Without --model or --provider, task remains caller-directed"));
    assert!(help.contains("Ollama needs no API key"));
    assert!(help.contains("OpenRouter may select an upstream provider for the requested model"));
    assert!(help.contains("https://api.deepseek.com/chat/completions"));
    assert!(help.contains("https://api.z.ai/api/paas/v4/chat/completions"));
    assert!(help.contains("https://api.mistral.ai/v1/chat/completions"));
    for descriptor in crate::brain::provider_descriptors() {
        for field in descriptor.auth_fields {
            assert!(help.contains(descriptor.selector));
            assert!(help.contains(field.name));
        }
        if let Some(endpoint) = descriptor.endpoint.help_url() {
            assert!(
                help.contains(&endpoint),
                "missing endpoint for {}",
                descriptor.selector
            );
        }
    }
}

#[test]
fn model_usage_jsonl_requires_a_compatible_model_provider() {
    let args = [
        OsString::from("--goal"),
        OsString::from("Read the brief"),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--no-config"),
        OsString::from("--model-usage-jsonl"),
    ];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 11434)),
    )
    .expect("reject usage output without a model provider");

    assert_eq!(code, 2);
    assert!(String::from_utf8_lossy(&error)
        .contains("--model-usage-jsonl requires a model-backed compatible provider"));
    assert!(output.is_empty());
}

#[test]
fn model_usage_jsonl_rejects_local_ollama_sources_before_network_access() {
    let args = [
        OsString::from("--goal"),
        OsString::from("Read the brief"),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--no-config"),
        OsString::from("--model"),
        OsString::from("qwen3"),
        OsString::from("--model-usage-jsonl"),
    ];
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_endpoint(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .expect("reject unsupported token usage output");

    assert_eq!(code, 2);
    assert!(String::from_utf8_lossy(&error)
        .contains("--model-usage-jsonl is supported by OpenAI-compatible providers"));
    assert!(output.is_empty());
}
