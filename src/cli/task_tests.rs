use super::{read_repository_context_json, run_task_with_endpoint, run_task_with_selection_loader};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use uuid::Uuid;

struct TempWorkspace {
    root: PathBuf,
    workspace: PathBuf,
}

impl TempWorkspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("krypper-verified-cli-{}", Uuid::new_v4()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("create fixture workspace");
        Self { root, workspace }
    }

    fn path(&self) -> &Path {
        &self.workspace
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove fixture workspace");
    }
}

fn model_tool(name: &str, arguments: Value) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{"function": {"name": name, "arguments": arguments}}]
        }
    })
    .to_string()
}

fn model_tools(calls: Vec<(&str, Value)>) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": calls
                .into_iter()
                .map(|(name, arguments)| json!({"function": {"name": name, "arguments": arguments}}))
                .collect::<Vec<_>>()
        }
    })
    .to_string()
}

fn model_text(content: &str) -> String {
    json!({"message":{"role":"assistant","content":content}}).to_string()
}

fn mock_server(responses: Vec<String>) -> (SocketAddr, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local model fixture");
    let address = listener.local_addr().expect("read model fixture address");
    let handle = thread::spawn(move || {
        responses
            .into_iter()
            .map(|body| {
                let (mut stream, _) = listener.accept().expect("accept model request");
                let request = read_request(&mut stream);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write model fixture response");
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
            .expect("model request content length");
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn verified_args(workspace: &Path, max_actions: &str, file: &str) -> Vec<OsString> {
    let mut args = vec![
        "--goal",
        "Create the requested output",
        "--max-actions",
        max_actions,
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(workspace.as_os_str().to_owned());
    args.extend(
        [
            "--model",
            "fixture-model",
            "--verified",
            "--verify-file",
            file,
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

fn run(args: &[OsString], input: &str, address: SocketAddr) -> (i32, String, String) {
    let mut input = Cursor::new(input.as_bytes().to_vec());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_endpoint(args, &mut input, &mut output, &mut error, address)
        .expect("run verified CLI fixture");
    (
        code,
        String::from_utf8(output).expect("CLI output UTF-8"),
        String::from_utf8(error).expect("CLI error UTF-8"),
    )
}

#[test]
fn repository_context_json_loader_accepts_context_and_rejects_unbounded_input() {
    let fixture = TempWorkspace::new();
    let path = fixture.root.join("context.json");
    let entries = json!([{"name":"slug","declared_in":"src/lib.rs"}]);
    let entry_bytes = serde_json::to_vec(&entries).expect("encode entries").len();
    let valid = json!({"bytes":entry_bytes,"modules":entries});
    fs::write(&path, serde_json::to_vec(&valid).expect("encode context"))
        .expect("write context fixture");
    assert_eq!(
        read_repository_context_json(&path).expect("read context"),
        valid
    );

    fs::write(&path, json!({"bytes":4097,"modules":[]}).to_string())
        .expect("write over-budget context");
    assert!(read_repository_context_json(&path)
        .expect_err("reject over-budget context")
        .contains("byte count must not exceed"));

    let oversized_entries = json!([{"excerpt":"x".repeat(
        crate::harness::context::MAX_REPOSITORY_CONTEXT_BYTES
    )}]);
    let oversized_context = json!({"bytes":0,"snippets":oversized_entries});
    fs::write(
        &path,
        serde_json::to_vec(&oversized_context).expect("encode oversized entries"),
    )
    .expect("write oversized entries");
    assert!(read_repository_context_json(&path)
        .expect_err("reject entries over the actual byte budget")
        .contains("entries must not exceed"));

    fs::write(&path, r#"{"bytes":1,"modules":[]}"#).expect("write mismatched byte count");
    assert!(read_repository_context_json(&path)
        .expect_err("reject mismatched byte count")
        .contains("does not match its entries"));

    fs::write(
        &path,
        vec![b'x'; crate::harness::context::MAX_REPOSITORY_CONTEXT_JSON_BYTES + 1],
    )
    .expect("write oversized context file");
    assert!(read_repository_context_json(&path)
        .expect_err("reject oversized file")
        .contains("must not exceed"));
}

#[test]
fn repository_context_json_rejects_local_provider_before_network_access() {
    let fixture = TempWorkspace::new();
    let context_path = fixture.root.join("context.json");
    fs::write(&context_path, r#"{"bytes":2,"modules":[]}"#).expect("write context fixture");
    let args = [
        OsString::from("--goal"),
        OsString::from("Read the brief"),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--workspace"),
        fixture.workspace.as_os_str().to_owned(),
        OsString::from("--model"),
        OsString::from("qwen3"),
        OsString::from("--no-config"),
        OsString::from("--repository-context-json"),
        context_path.into_os_string(),
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
    .expect("reject unsupported context provider");

    assert_eq!(code, 2);
    assert!(String::from_utf8_lossy(&error)
        .contains("--repository-context-json is supported by OpenAI-compatible providers"));
    assert!(output.is_empty());
}

#[test]
fn verified_cli_passes_task_session_file_evidence_without_correction() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![model_tool(
        "create_file",
        json!({"path":"expected.txt","contents":"verified"}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete artifact ready\n",
        address,
    );
    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Verification: Passed"));
    assert!(output.contains("Correction: not needed; checks passed"));
    assert!(output.contains("Actions: 2/2"));
    assert_eq!(
        fs::read(fixture.path().join("expected.txt")).unwrap(),
        b"verified"
    );
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_accepts_registered_patch_evidence_for_the_exact_file() {
    let fixture = TempWorkspace::new();
    fs::write(fixture.path().join("expected.txt"), "old\n").expect("write initial file");
    let patch = "--- a/expected.txt\n+++ b/expected.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let (address, server) = mock_server(vec![model_tool(
        "apply_patch",
        json!({"path":"expected.txt", "patch":patch}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete patched file\n",
        address,
    );

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Verification: Passed"));
    assert_eq!(
        fs::read(fixture.path().join("expected.txt")).unwrap(),
        b"new\n"
    );
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_accepts_successful_read_evidence_for_the_exact_file() {
    let fixture = TempWorkspace::new();
    fs::write(fixture.path().join("expected.txt"), "readable").expect("write file");
    let (address, server) = mock_server(vec![model_tool(
        "read_file",
        json!({"path":"expected.txt"}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete read verified\n",
        address,
    );

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Verification: Passed"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_accepts_successful_range_read_evidence_for_the_exact_file() {
    let fixture = TempWorkspace::new();
    fs::write(fixture.path().join("expected.txt"), "first\nsecond\n").expect("write file");
    let (address, server) = mock_server(vec![model_tool(
        "read_range",
        json!({"path":"expected.txt","start_line":1,"max_lines":2}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete range read verified\n",
        address,
    );

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Verification: Passed"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_accepts_successful_replace_evidence_for_the_exact_file() {
    let fixture = TempWorkspace::new();
    let path = fixture.path().join("expected.txt");
    fs::write(&path, "old text").expect("write file");
    let (address, server) = mock_server(vec![model_tool(
        "replace_text",
        json!({
            "path":"expected.txt",
            "match_text":"old text",
            "replacement_text":"new text"
        }),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete replacement verified\n",
        address,
    );

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Verification: Passed"));
    assert_eq!(fs::read(path).unwrap(), b"new text");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_corrects_unknown_file_evidence_once_within_the_original_budget() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![
        model_tool(
            "create_file",
            json!({"path":"wrong.txt","contents":"not the requested output"}),
        ),
        model_text("The expected output file is missing; create it."),
        model_tool(
            "create_file",
            json!({"path":"expected.txt","contents":"corrected"}),
        ),
    ]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "complete initial plan\ncomplete corrected plan\n",
        address,
    );
    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Verification: Passed"));
    assert!(output.contains("Correction: attempted once"));
    assert!(output.contains("Actions: 4/5"));
    assert_eq!(
        fs::read(fixture.path().join("expected.txt")).unwrap(),
        b"corrected"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
}

#[test]
fn verified_cli_reports_unknown_when_budget_cannot_fit_a_correction() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![model_tool(
        "create_file",
        json!({"path":"wrong.txt","contents":"not the requested output"}),
    )]);

    let (code, output, _error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete initial plan\n",
        address,
    );
    assert_eq!(code, 1);
    assert!(output.contains("Task outcome: failed"));
    assert!(output.contains("Verification: Unknown"));
    assert!(output.contains("Correction: skipped; insufficient remaining action budget"));
    assert!(output.contains("Actions remaining: 0"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_reports_failed_file_evidence_without_retrying_the_failed_action() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![model_tool(
        "read_file",
        json!({"path":"expected.txt"}),
    )]);

    let (code, output, _) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "",
        address,
    );
    assert_eq!(code, 1);
    assert!(output.contains("Task outcome: failed"));
    assert!(output.contains("Verification: Failed"));
    assert!(output.contains("Correction: skipped; initial turn did not complete normally"));
    assert!(output.contains("Stop reason: ActionFailed"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_prefers_failed_file_evidence_over_success_in_the_same_turn() {
    let fixture = TempWorkspace::new();
    let path = fixture.path().join("expected.txt");
    fs::write(&path, "existing").expect("write existing file");
    let (address, server) = mock_server(vec![model_tools(vec![
        ("read_file", json!({"path":"expected.txt"})),
        (
            "create_file",
            json!({"path":"expected.txt","contents":"replace is not allowed"}),
        ),
    ])]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "",
        address,
    );

    assert_eq!(code, 1, "{error}");
    assert!(output.contains("Verification: Failed"));
    assert!(output.contains("Stop reason: ActionFailed"));
    assert_eq!(fs::read(path).unwrap(), b"existing");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_does_not_treat_directory_creation_failure_as_file_evidence() {
    let fixture = TempWorkspace::new();
    fs::write(fixture.path().join("expected.txt"), "existing").expect("write file");
    let (address, server) = mock_server(vec![model_tool(
        "create_directory",
        json!({"path":"expected.txt"}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "",
        address,
    );

    assert_eq!(code, 1, "{error}");
    assert!(output.contains("Verification: Unknown"));
    assert!(output.contains("Stop reason: ActionFailed"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_does_not_treat_successful_directory_creation_as_file_evidence() {
    let fixture = TempWorkspace::new();
    let path = fixture.path().join("expected.txt");
    let (address, server) = mock_server(vec![model_tool(
        "create_directory",
        json!({"path":"expected.txt"}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "2", "expected.txt"),
        "complete directory created\n",
        address,
    );

    assert_eq!(code, 1, "{error}");
    assert!(output.contains("Verification: Unknown"));
    assert!(path.is_dir());
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_does_not_retry_denied_workspace_operations() {
    let fixture = TempWorkspace::new();
    let outside = fixture.root.join("outside.txt");
    fs::write(&outside, "unchanged").expect("write outside sentinel");
    let (address, server) = mock_server(vec![model_tool(
        "create_file",
        json!({"path":"../outside.txt","contents":"must be denied"}),
    )]);

    let (code, output, _) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "",
        address,
    );
    assert_eq!(code, 1);
    assert!(output.contains("Verification: Unknown"));
    assert!(output.contains("Correction: skipped; initial turn did not complete normally"));
    assert!(output.contains("Stop reason: ActionFailed"));
    assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_preserves_cancellation_and_unknown_evidence() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![model_tool(
        "create_file",
        json!({"path":"wrong.txt","contents":"not the requested output"}),
    )]);

    let (code, output, _) = run(
        &verified_args(fixture.path(), "5", "expected.txt"),
        "cancel\n",
        address,
    );
    assert_eq!(code, 130);
    assert!(output.contains("Task outcome: cancelled by caller"));
    assert!(output.contains("Verification: Unknown"));
    assert!(output.contains("Correction: skipped; initial turn did not complete normally"));
    assert!(output.contains("Stop reason: Cancelled"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn verified_cli_resume_requires_fresh_file_observation_and_repeated_options() {
    let fixture = TempWorkspace::new();
    let checkpoint = fixture.root.join("pause.json");
    let (address, server) = mock_server(vec![
        model_tool(
            "create_file",
            json!({"path":"expected.txt","contents":"saved output"}),
        ),
        model_tool("read_file", json!({"path":"expected.txt"})),
    ]);

    let mut first_args = verified_args(fixture.path(), "3", "expected.txt");
    first_args.extend([
        OsString::from("--checkpoint-out"),
        checkpoint.as_os_str().to_owned(),
    ]);
    let (first_code, first_output, first_error) = run(&first_args, "continue\n", address);
    assert_eq!(first_code, 0, "{first_error}\n{first_output}");
    assert!(first_output.contains("Task outcome: paused (still running)"));
    assert!(first_output.contains("Correction: deferred until checkpoint resume"));

    let mut resume_args = vec![
        "--resume",
        checkpoint.to_str().unwrap(),
        "--max-actions",
        "2",
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    resume_args.push(fixture.path().as_os_str().to_owned());
    resume_args.extend(
        [
            "--model",
            "fixture-model",
            "--verified",
            "--verify-file",
            "expected.txt",
        ]
        .into_iter()
        .map(OsString::from),
    );
    let (resume_code, resume_output, resume_error) =
        run(&resume_args, "complete resumed task\n", address);
    assert_eq!(resume_code, 0, "{resume_error}");
    assert!(resume_output.contains("Task outcome: succeeded"));
    assert!(resume_output.contains("Verification: Passed"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn verified_cli_checkpoint_after_correction_keeps_the_invocation_allowance_used() {
    let fixture = TempWorkspace::new();
    let checkpoint = fixture.root.join("pause-after-correction.json");
    let (address, server) = mock_server(vec![
        model_tool(
            "create_file",
            json!({"path":"wrong.txt","contents":"not the requested output"}),
        ),
        model_text("The required file is missing; create it."),
        model_tool(
            "create_file",
            json!({"path":"expected.txt","contents":"corrected"}),
        ),
        model_tool("read_file", json!({"path":"expected.txt"})),
    ]);

    let mut first_args = verified_args(fixture.path(), "5", "expected.txt");
    first_args.extend([
        OsString::from("--checkpoint-out"),
        checkpoint.as_os_str().to_owned(),
    ]);
    let (first_code, first_output, first_error) =
        run(&first_args, "complete initial turn\ncontinue\n", address);
    assert_eq!(first_code, 0, "{first_error}\n{first_output}");
    assert!(first_output.contains("Task outcome: paused (still running)"));
    assert!(first_output.contains("Correction: attempted once"));
    assert!(first_output.contains("Checkpoint saved:"));
    assert_eq!(
        fs::read(fixture.path().join("expected.txt")).unwrap(),
        b"corrected"
    );

    let mut resume_args = vec![
        "--resume",
        checkpoint.to_str().unwrap(),
        "--max-actions",
        "2",
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    resume_args.push(fixture.path().as_os_str().to_owned());
    resume_args.extend(
        [
            "--model",
            "fixture-model",
            "--verified",
            "--verify-file",
            "expected.txt",
        ]
        .into_iter()
        .map(OsString::from),
    );
    let (resume_code, resume_output, resume_error) =
        run(&resume_args, "complete resumed turn\n", address);
    assert_eq!(resume_code, 0, "{resume_error}");
    assert!(resume_output.contains("Task outcome: succeeded"));
    assert!(resume_output.contains("Verification: Passed"));
    assert_eq!(server.join().unwrap().len(), 4);
}

#[test]
fn verified_cli_rejects_hosted_provider_before_credential_lookup() {
    let fixture = TempWorkspace::new();
    let args = [
        "--goal",
        "Create the requested output",
        "--max-actions",
        "2",
        "--workspace",
        fixture.path().to_str().unwrap(),
        "--model",
        "fixture-model",
        "--provider",
        "cloudflare",
        "--verified",
        "--verify-file",
        "expected.txt",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let (code, output, error) = run(&args, "", SocketAddr::from(([127, 0, 0, 1], 0)));
    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(error.contains("--verified requires the local Ollama provider"));
}

#[test]
fn verified_cli_rejects_configured_hosted_provider_before_credential_lookup() {
    let fixture = TempWorkspace::new();
    let config_path = fixture.root.join("model-config.toml");
    fs::write(
        &config_path,
        "model = \"fixture-model\"\nprovider = \"cloudflare\"\n",
    )
    .expect("write configured model defaults");
    let args = [
        "--goal",
        "Create the requested output",
        "--max-actions",
        "2",
        "--workspace",
        fixture.path().to_str().unwrap(),
        "--verified",
        "--verify-file",
        "expected.txt",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let mut input = Cursor::new(Vec::new());
    let mut output = Vec::new();
    let mut error = Vec::new();
    let code = run_task_with_selection_loader(
        &args,
        &mut input,
        &mut output,
        &mut error,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        true,
        |stderr| {
            let Some(defaults) = super::super::config::load_model_defaults(&config_path)? else {
                return Ok(None);
            };
            writeln!(
                stderr,
                "using model defaults from {}",
                defaults.path.display()
            )
            .map_err(|cause| cause.to_string())?;
            Ok(Some(defaults))
        },
    )
    .expect("run configured provider preflight fixture");

    let error = String::from_utf8(error).unwrap();
    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(error.contains("--verified requires the local Ollama provider"));
    assert!(!error.contains("CLOUDFLARE_ACCOUNT_ID"));
}

#[test]
fn task_without_verified_mode_keeps_the_existing_caller_directed_flow() {
    let fixture = TempWorkspace::new();
    let mut args = vec![
        "--goal",
        "Create one output file",
        "--max-actions",
        "2",
        "--workspace",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(fixture.path().as_os_str().to_owned());

    let (code, output, error) = run(
        &args,
        "create output.txt original behavior\ncomplete output created\n",
        SocketAddr::from(([127, 0, 0, 1], 0)),
    );

    assert_eq!(code, 0, "{error}");
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Actions: 2/2"));
    assert!(!output.contains("Verification:"));
    assert_eq!(
        fs::read(fixture.path().join("output.txt")).unwrap(),
        b"original behavior"
    );
}

#[test]
fn verified_cli_skips_correction_when_only_two_action_slots_remain() {
    let fixture = TempWorkspace::new();
    let (address, server) = mock_server(vec![model_tool(
        "create_file",
        json!({"path":"wrong.txt", "contents":"not the requested output"}),
    )]);

    let (code, output, error) = run(
        &verified_args(fixture.path(), "4", "expected.txt"),
        "complete initial plan\n",
        address,
    );

    assert_eq!(code, 1, "{error}");
    assert!(output.contains("Verification: Unknown"));
    assert!(output.contains("Correction: skipped; insufficient remaining action budget"));
    assert!(output.contains("Actions: 2/4"));
    assert!(output.contains("Actions remaining: 2"));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn task_help_documents_verified_mode_and_file_checks() {
    let (code, output, error) = run(
        &[OsString::from("--help")],
        "",
        SocketAddr::from(([127, 0, 0, 1], 0)),
    );
    assert_eq!(code, 0, "{error}");
    assert!(output.contains("--verified"));
    assert!(output.contains("--verify-file PATH"));
    assert!(output.contains("--stream-events-jsonl"));
    assert!(output.contains("enter complete, continue, or cancel"));
    assert!(output.contains("AGENTS.md guidance"));
}

#[test]
fn verified_cli_rejects_workspace_escape_and_duplicate_check_paths() {
    let fixture = TempWorkspace::new();
    for (files, expected) in [
        (vec!["../outside.txt"], "must not escape the task workspace"),
        (
            vec!["expected.txt", "./expected.txt"],
            "each --verify-file path may be provided only once",
        ),
    ] {
        let mut args = verified_args(fixture.path(), "2", files[0]);
        if files.len() == 2 {
            args.extend(["--verify-file", files[1]].into_iter().map(OsString::from));
        }
        let (code, _, error) = run(&args, "", SocketAddr::from(([127, 0, 0, 1], 0)));
        assert_eq!(code, 2);
        assert!(error.contains(expected), "{error}");
    }

    let long_path = "a".repeat(193);
    for (path, expected) in [
        ("", "must not be empty"),
        ("bad\0path", "must not contain control characters"),
        ("/absolute/path", "must be relative to the task workspace"),
        (long_path.as_str(), "must not exceed 192 UTF-8 bytes"),
    ] {
        let args = verified_args(fixture.path(), "2", path);
        let (code, _, error) = run(&args, "", SocketAddr::from(([127, 0, 0, 1], 0)));
        assert_eq!(code, 2);
        assert!(error.contains(expected), "{error}");
    }

    let mut too_many_paths = verified_args(fixture.path(), "40", "file-0.txt");
    for index in 1..33 {
        too_many_paths.push(OsString::from("--verify-file"));
        too_many_paths.push(OsString::from(format!("file-{index}.txt")));
    }
    let (code, _, error) = run(&too_many_paths, "", SocketAddr::from(([127, 0, 0, 1], 0)));
    assert_eq!(code, 2);
    assert!(error.contains("at most 32 --verify-file paths may be provided"));
}

#[cfg(unix)]
#[test]
fn verified_cli_rejects_non_utf8_verification_paths_before_model_request() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = TempWorkspace::new();
    let mut args = verified_args(fixture.path(), "2", "placeholder.txt");
    args.pop();
    args.push(OsString::from_vec(vec![b'f', b'i', b'l', b'e', 0xff]));

    let (code, _, error) = run(&args, "", SocketAddr::from(([127, 0, 0, 1], 0)));
    assert_eq!(code, 2);
    assert!(error.contains("verification file paths must be valid UTF-8"));
}
