use super::lifecycle::{cleanup_on_error, connect_stdio};
use super::stdio::spawn;
use super::*;
use std::fs;
use std::io;
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

fn assert_process_group_gone(pgid: i32) {
    // SAFETY: signal 0 only probes whether the process group exists; `kill` takes only integers.
    assert_eq!(unsafe { libc::kill(-pgid, 0) }, -1);
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "cleanup returned before the process group was empty"
    );
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
        r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"fake","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake"}}}"#,
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

#[test]
fn cumulative_stderr_limit_is_enforced_across_notifications() {
    let dir = temp_dir("stderr");
    let script = format!(
        "IFS= read -r request\nprintf '%05000d\\n' 0 >&2\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}'\nprintf '%05000d\\n' 0 >&2\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nwhile IFS= read -r line; do :; done\n",
        response = initialize_response("2024-11-05")
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_secs(2));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::OutputLimitExceeded);
}

#[test]
fn shutdown_kills_the_child_process_tree() {
    let dir = temp_dir("process-tree");
    let marker = dir.0.join("grandchild");
    let script = format!(
        "printf '%s\\n' $$ > \"$1\"\n(sleep 1; printf alive > \"$2\") &\nIFS= read -r request\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nwhile IFS= read -r line; do :; done\n",
        response = initialize_response("2024-11-05")
    );
    let config = McpStdioConfig {
        executable: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            script,
            "fake-mcp-server".into(),
            dir.0.join("log").display().to_string(),
            marker.display().to_string(),
        ],
        timeout: Duration::from_secs(2),
    };

    let session = connect_stdio(&config, &McpCancellation::new()).expect("connect");
    let pid_log = fs::read_to_string(dir.0.join("log")).expect("pid log");
    let pgid: i32 = pid_log
        .lines()
        .next()
        .expect("pid line")
        .parse()
        .expect("pgid");
    session.shutdown().expect("shutdown");
    assert_process_group_gone(pgid);
    assert!(!marker.exists(), "grandchild survived shutdown");
}

#[cfg(target_os = "macos")]
#[test]
fn eperm_with_exited_leader_and_live_descendant_fails_closed() {
    let dir = temp_dir("eperm-live-descendant");
    let descendant_pid_file = dir.0.join("descendant.pid");
    let config = config(
        "sleep 10 &\nprintf '%s\\n' \"$!\" > \"$1\"\nexit 0\n",
        &dir.0.join("log"),
        Duration::from_secs(3),
    );
    let mut stdio = spawn(
        &config,
        Instant::now() + Duration::from_secs(3),
        &McpCancellation::new(),
    )
    .expect("spawn MCP process");

    let start = Instant::now();
    let descendant_pid = loop {
        if let Some(pid) = fs::read_to_string(&descendant_pid_file)
            .ok()
            .and_then(|contents| contents.trim().parse::<libc::pid_t>().ok())
        {
            break pid;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "descendant PID was not recorded"
        );
        thread::sleep(Duration::from_millis(5));
    };

    let start = Instant::now();
    loop {
        if stdio.child.has_exited().expect("check leader state") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "process-group leader did not exit"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: signal 0 probes the known descendant PID without delivering a signal.
    assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, 0);

    let cleanup_started = Instant::now();
    let cleanup = stdio
        .child
        .terminate_with_signal_error(io::Error::from_raw_os_error(libc::EPERM));
    let cleanup_elapsed = cleanup_started.elapsed();
    // SAFETY: this is the known live descendant created by the test, not a process-group ID.
    let kill_descendant = unsafe { libc::kill(descendant_pid, libc::SIGKILL) };
    assert_eq!(kill_descendant, 0, "test descendant was not still alive");
    assert!(
        cleanup_elapsed >= Duration::from_secs(1),
        "cleanup did not wait to verify the still-populated group"
    );
    assert_eq!(cleanup, Err(McpLifecycleError::CleanupFailed));
}

#[test]
fn spawn_time_is_included_in_the_handshake_timeout() {
    let dir = temp_dir("spawn-timeout");
    let config = config("exit 0\n", &dir.0.join("log"), Duration::from_nanos(1));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::Timeout);
}

#[test]
fn spawn_timeout_waits_for_worker_cleanup() {
    let dir = temp_dir("spawn-timeout-cleanup");
    let marker = dir.0.join("marker");
    let config = config(
        "sleep 0.5\n# krypper-spawn-delay\nprintf alive > \"$1\"\n",
        &marker,
        Duration::from_nanos(1),
    );
    let start = Instant::now();

    let error = match spawn(
        &config,
        Instant::now() + Duration::from_nanos(1),
        &McpCancellation::new(),
    ) {
        Ok(_) => panic!("spawn unexpectedly succeeded"),
        Err(error) => error,
    };

    assert_eq!(error, McpLifecycleError::Timeout);
    assert!(
        start.elapsed() >= Duration::from_millis(40),
        "spawn returned before the worker finished"
    );
    thread::sleep(Duration::from_millis(600));
    assert!(!marker.exists(), "spawned child survived timeout cleanup");
}

#[test]
fn cancellation_during_spawn_stops_the_worker() {
    let dir = temp_dir("spawn-cancel");
    let marker = dir.0.join("marker");
    let config = config(
        "sleep 0.5\n# krypper-spawn-delay\nprintf alive > \"$1\"\n",
        &marker,
        Duration::from_secs(2),
    );
    let cancel = McpCancellation::new();
    let worker_cancel = cancel.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        worker_cancel.cancel();
    });

    let error = error_of(connect_stdio(&config, &cancel));
    canceller.join().expect("canceller");
    assert_eq!(error, McpLifecycleError::Cancelled);
    thread::sleep(Duration::from_millis(600));
    assert!(
        !marker.exists(),
        "spawn worker created a child after cancellation"
    );
}

#[test]
fn excessive_notifications_before_response_are_rejected() {
    let dir = temp_dir("notification-flood");
    let script = format!(
        "i=0\nwhile [ $i -lt 17 ]; do printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}'\ni=$((i + 1))\ndone\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nwhile IFS= read -r line; do :; done\n",
        response = initialize_response("2025-06-18")
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_secs(2));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::ProtocolViolation);
}

#[test]
fn handshake_error_terminates_server_before_returning() {
    let dir = temp_dir("cleanup-error-path");
    let pid_log = dir.0.join("pid.log");
    let script = format!(
        "printf '%s\\n' $$ > \"$1\"\nprintf '%s\\n' '{response}'\nwhile IFS= read -r line; do :; done\n",
        response = initialize_response("9999-01-01")
    );
    let config = config(&script, &pid_log, Duration::from_secs(2));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::UnsupportedProtocolVersion);
    let pid_text = fs::read_to_string(&pid_log).expect("pid log");
    let pid: i32 = pid_text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad pid log: {pid_text:?}"));
    // SAFETY: signal 0 only probes whether the process exists; `kill` takes only integers.
    let probe = unsafe { libc::kill(pid, 0) };
    assert_ne!(probe, 0, "server survived failed handshake");
}

#[test]
fn confirmed_cleanup_preserves_the_primary_handshake_error() {
    for primary in [
        McpLifecycleError::Cancelled,
        McpLifecycleError::Timeout,
        McpLifecycleError::Disconnected,
        McpLifecycleError::ProtocolViolation,
    ] {
        assert_eq!(cleanup_on_error(primary, || Ok(())), primary);
    }
}

#[test]
fn unconfirmed_cleanup_fails_closed_over_the_primary_handshake_error() {
    for primary in [
        McpLifecycleError::Cancelled,
        McpLifecycleError::Timeout,
        McpLifecycleError::Disconnected,
        McpLifecycleError::ProtocolViolation,
    ] {
        assert_eq!(
            cleanup_on_error(primary, || Err(McpLifecycleError::CleanupFailed)),
            McpLifecycleError::CleanupFailed
        );
    }
}

#[test]
fn response_limit_includes_already_buffered_notification_bytes() {
    let dir = temp_dir("response-limit-pending");
    let script = format!(
        "printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}'\nhead -c 70000 /dev/zero | tr '\\000' 'A'\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nwhile IFS= read -r line; do :; done\n",
        response = initialize_response("2024-11-05")
    );
    let config = config(&script, &dir.0.join("log"), Duration::from_secs(2));

    let error = error_of(connect_stdio(&config, &McpCancellation::new()));
    assert_eq!(error, McpLifecycleError::OutputLimitExceeded);
}

#[test]
fn shutdown_kills_descendants_after_parent_exits() {
    let dir = temp_dir("orphan-tree");
    let marker = dir.0.join("grandchild");
    let script = format!(
        "(sleep 1; printf alive > \"$2\") &\nIFS= read -r request\nprintf '%s\\n' '{response}'\nIFS= read -r notification\nsleep 0.2\nexit 0\n",
        response = initialize_response("2024-11-05")
    );
    let config = McpStdioConfig {
        executable: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            script,
            "fake-mcp-server".into(),
            dir.0.join("log").display().to_string(),
            marker.display().to_string(),
        ],
        timeout: Duration::from_secs(2),
    };

    let session = connect_stdio(&config, &McpCancellation::new()).expect("connect");
    thread::sleep(Duration::from_millis(400));
    session.shutdown().expect("shutdown");
    thread::sleep(Duration::from_millis(800));
    assert!(!marker.exists(), "descendant survived parent exit");
}
