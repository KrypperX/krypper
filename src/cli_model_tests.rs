use super::{prepare_model_source, run_task_with_endpoint};
use crate::model::{ModelDecisionSource, ModelProvider};
use crate::task_lifecycle::{TaskSession, TaskState};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use uuid::Uuid;

static PROVIDER_ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

struct RestoreEnvironmentVariable {
    name: &'static str,
    value: Option<OsString>,
}

impl RestoreEnvironmentVariable {
    fn remove(name: &'static str) -> Self {
        let value = std::env::var_os(name);
        std::env::remove_var(name);
        Self { name, value }
    }

    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self {
            name,
            value: previous,
        }
    }
}

impl Drop for RestoreEnvironmentVariable {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

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
fn model_cli_uses_explicit_workspace_without_changing_current_directory() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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
fn hosted_keys_do_not_change_caller_directed_or_default_ollama_tasks() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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
    let (caller_code, caller_output, caller_error) = in_workspace(caller_workspace.path(), || {
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
    let (ollama_code, ollama_output, ollama_error) = in_workspace(ollama_workspace.path(), || {
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
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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

    assert!(matches!(
        explicit,
        Ok(Some(ModelDecisionSource::Responses(_)))
    ));
    assert!(matches!(caller_directed, Ok(None)));
    assert!(matches!(
        implicit_ollama,
        Ok(Some(ModelDecisionSource::Ollama(_)))
    ));
}

#[test]
fn hosted_provider_credentials_are_required_before_task_lifecycle_creation() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
    let provider_variables = [
        "GEMINI_API_KEY",
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_API_TOKEN",
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
        if let Some((name, value)) = supplemental {
            std::env::set_var(name, value);
        }
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
fn cloudflare_account_id_is_validated_before_task_lifecycle_creation() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
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
        ("groq", None, "--model is required with --provider"),
        ("openrouter", None, "--model is required with --provider"),
        ("deepseek", None, "--model is required with --provider"),
        ("zai", None, "--model is required with --provider"),
        ("mistral", None, "--model is required with --provider"),
        ("unknown", Some("test-model"), "unsupported provider"),
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
    assert!(help.contains(
        "choose ollama, gemini, openai, xai, anthropic, cloudflare, groq, openrouter, deepseek, zai, or mistral"
    ));
    for credential in [
        "GEMINI_API_KEY",
        "OPENAI_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_API_TOKEN",
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
    assert!(help.contains("no custom API URL is accepted"));
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
}
