use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "krypper-mcp-transport-{}-{}-{}",
        tag,
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create temp dir");
    TempDir(path)
}

fn initialize_response(version: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{}","capabilities":{{}},"serverInfo":{{"name":"fake","version":"1"}}}}}}"#,
        version
    )
}

fn responding_server(response: &str) -> String {
    format!(
        "IFS= read -r request\nprintf '%s\\n' \"$request\" > \"$1\"\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nprintf '%s\\n' \"$notification\" >> \"$1\"\nwhile IFS= read -r line; do :; done\n"
    )
}

fn config(script: &str, log: &Path, timeout: Duration) -> McpStdioConfig {
    McpStdioConfig {
        executable: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            script.into(),
            "fake-mcp-server".into(),
            log.display().to_string(),
        ],
        timeout,
    }
}

fn error_of(result: Result<McpSession, McpLifecycleError>) -> McpLifecycleError {
    match result {
        Ok(_) => panic!("connection unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn wait_for_log(log: &Path, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let observed = fs::read_to_string(log).unwrap_or_default();
        if observed.contains(needle) {
            return observed;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "log never contained {needle}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn handshake_succeeds_without_discovering_or_invoking_tools() {
    let dir = temp_dir("success");
    let log = dir.0.join("requests.log");
    let config = config(
        &responding_server(&initialize_response("2024-11-05")),
        &log,
        Duration::from_secs(2),
    );

    let session = connect_stdio(&config, &McpCancellation::new()).expect("connect");
    assert_eq!(session.protocol_version(), "2024-11-05");

    let observed = wait_for_log(&log, "\"method\":\"notifications/initialized\"");
    assert!(observed.contains("\"method\":\"initialize\""));
    assert!(observed.contains("\"protocolVersion\":\"2025-06-18\""));
    assert!(observed.contains("\"method\":\"notifications/initialized\""));
    assert!(!observed.contains("tools/list"));
    assert!(!observed.contains("tools/call"));
    session.shutdown().expect("shutdown");
}

#[test]
fn server_notifications_before_response_are_ignored() {
    let dir = temp_dir("notification");
    let response = initialize_response("2025-06-18");
    let script = format!(
        "IFS= read -r request\nprintf '%s\\n' \"$request\" > \"$1\"\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}'\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nprintf '%s\\n' \"$notification\" >> \"$1\"\nwhile IFS= read -r line; do :; done\n"
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_secs(2));

    let session = connect_stdio(&config, &McpCancellation::new()).expect("connect");
    assert_eq!(session.protocol_version(), "2025-06-18");
    session.shutdown().expect("shutdown");
}

#[test]
fn invalid_configuration_is_rejected_before_spawn() {
    let dir = temp_dir("invalid");
    let mut config = config("exit 0\n", &dir.0.join("log"), Duration::from_secs(1));
    config.executable = PathBuf::from("mcp-server");

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(
        error,
        McpLifecycleError::InvalidConfig(McpConfigError::RelativeExecutable)
    );
}

#[test]
fn spawn_failure_is_mapped_without_hanging() {
    let config = McpStdioConfig {
        executable: PathBuf::from("/definitely/not/a/krypper-mcp-server"),
        args: Vec::new(),
        timeout: Duration::from_secs(1),
    };

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::SpawnFailed);
}

#[test]
fn unsupported_protocol_version_fails_closed() {
    let dir = temp_dir("version");
    let config = config(
        &responding_server(&initialize_response("9999-01-01")),
        &dir.0.join("log"),
        Duration::from_secs(2),
    );

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::UnsupportedProtocolVersion);
}

#[test]
fn malformed_or_mismatched_responses_are_protocol_violations() {
    let dir = temp_dir("protocol");
    for response in [
        "not json",
        r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
        r#"{"jsonrpc":"1.0","id":1,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
    ] {
        let config = config(
            &responding_server(response),
            &dir.0.join("log"),
            Duration::from_secs(2),
        );
        let error = error_of(connect_stdio(&config, &McpCancellation::new()));
        assert_eq!(error, McpLifecycleError::ProtocolViolation);
    }
}

#[test]
fn server_error_response_is_mapped() {
    let dir = temp_dir("server-error");
    let config = config(
        &responding_server(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"denied"}}"#,
        ),
        &dir.0.join("log"),
        Duration::from_secs(2),
    );

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::ServerError);
}

#[test]
fn disconnect_before_response_is_mapped() {
    let dir = temp_dir("disconnect");
    let config = config("exit 0\n", &dir.0.join("log"), Duration::from_secs(2));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::Disconnected);
}

#[test]
fn handshake_timeout_is_bounded() {
    let dir = temp_dir("timeout");
    let script = format!(
        "sleep 2\nprintf '%s\\n' '{}'\n",
        initialize_response("2024-11-05")
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_millis(100));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::Timeout);
}

#[test]
fn cancellation_stops_handshake_and_cleans_up() {
    let dir = temp_dir("cancel");
    let script = format!(
        "sleep 2\nprintf '%s\\n' '{}'\n",
        initialize_response("2024-11-05")
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_secs(5));
    let cancel = McpCancellation::new();
    let thread_cancel = cancel.clone();
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        thread_cancel.cancel();
    });

    let error = error_of(connect_stdio(&config, &cancel));
    handle.join().expect("join canceller");
    assert_eq!(error, McpLifecycleError::Cancelled);
}

#[test]
fn oversized_response_is_rejected_without_buffering_without_bound() {
    let dir = temp_dir("oversized");
    let config = config(
        "printf '%070000d\\n' 0\n",
        &dir.0.join("log"),
        Duration::from_secs(2),
    );

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::OutputLimitExceeded);
}
