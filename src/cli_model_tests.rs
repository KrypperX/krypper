use super::run_task_with_endpoint;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-model-cli-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create temporary workspace");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove temporary workspace");
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
    assert_eq!(current_directory, process_directory);
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
    assert_eq!(current_directory, workspace.path());
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

    assert_eq!(current_directory, process_directory);
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
fn model_cli_reports_successful_task_and_turn_outcomes() {
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
    assert!(output.contains("Task outcome: succeeded"));
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
fn model_cli_uses_explicit_workspace_without_changing_current_directory() {
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

    let (code, output, error, current_directory) = in_workspace(&process_directory, || {
        let original_directory = std::env::current_dir().expect("read current directory");
        let mut args = task_args("2");
        args.push(OsString::from("--workspace"));
        args.push(workspace.as_os_str().to_owned());
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_endpoint(&args, &mut input, &mut output, &mut error, endpoint)
            .expect("run mocked model task");
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
    assert!(output.contains("Model selected action: create_file"));
    assert!(output.contains("Task outcome: succeeded"));
    assert_eq!(
        fs::read_to_string(workspace.join("model result.txt")).expect("read model result"),
        "selected workspace"
    );
    assert!(!process_directory.join("model result.txt").exists());
    assert_eq!(current_directory, process_directory);
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
    assert!(output.contains("Task outcome: failed"));
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
        assert!(output.contains("Task outcome: failed"));
        assert!(output.contains("Turn outcome: failure:"));
        assert!(!output.contains("Task outcome: succeeded"));
        if let Some(server) = server {
            assert_eq!(server.join().expect("join mock server").len(), 1);
        }
    }
}
