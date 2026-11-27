//! Direct host execution for a task with explicit Full Access confirmation.
//!
//! This module never parses shell text and does not create a filesystem, network, syscall, or
//! credential sandbox. Linux process-group supervision bounds retained output and lifetime and
//! cleans up descendants that remain in the launched process group.

use crate::contracts::process_execution::{ProcessOutcome, ProcessResult};
use krypper_sandbox::SpawnAuthorizationGate;
#[cfg(target_os = "linux")]
use krypper_sandbox::{SpawnAction, SpawnGateError};
#[cfg(target_os = "linux")]
use std::io::{self, Read};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

pub(crate) const MAX_HOST_EXEC_ARGUMENTS: usize = 32;
pub(crate) const MAX_HOST_EXEC_ARGUMENT_BYTES: usize = 1_024;
pub(crate) const MAX_HOST_EXEC_TOTAL_ARGUMENT_BYTES: usize = 4_096;
pub(crate) const MAX_HOST_EXEC_TEXT_BYTES: usize = 4_096;

const HOST_EXEC_WALL_LIMIT: Duration = Duration::from_secs(300);
const HOST_EXEC_OUTPUT_LIMIT: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(target_os = "linux")]
const CLEANUP_WAIT: Duration = Duration::from_millis(500);

/// One bounded direct-argv request. Its working directory may be anywhere the current OS user
/// can access; it is not resolved through a Krypper workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostProcessRequest {
    executable: String,
    arguments: Vec<String>,
    working_directory: PathBuf,
}

impl HostProcessRequest {
    /// Validates direct argv and the unconfined host working directory.
    pub fn new(
        executable: String,
        arguments: Vec<String>,
        working_directory: String,
    ) -> Result<Self, &'static str> {
        if executable.trim().is_empty()
            || executable.len() > MAX_HOST_EXEC_TEXT_BYTES
            || executable.chars().any(char::is_control)
        {
            return Err("executable must be a nonempty bounded string without control characters");
        }
        if arguments.len() > MAX_HOST_EXEC_ARGUMENTS
            || arguments.iter().any(|argument| {
                argument.len() > MAX_HOST_EXEC_ARGUMENT_BYTES
                    || argument.chars().any(char::is_control)
            })
            || arguments
                .iter()
                .map(String::len)
                .try_fold(0_usize, usize::checked_add)
                .is_none_or(|bytes| bytes > MAX_HOST_EXEC_TOTAL_ARGUMENT_BYTES)
        {
            return Err("argv exceeds the host process argument limits");
        }
        if working_directory.trim().is_empty()
            || working_directory.len() > MAX_HOST_EXEC_TEXT_BYTES
            || working_directory.chars().any(char::is_control)
        {
            return Err(
                "working_directory must be a nonempty bounded host path without control characters",
            );
        }
        Ok(Self {
            executable,
            arguments,
            working_directory: PathBuf::from(working_directory),
        })
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn executable(&self) -> &str {
        &self.executable
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn arguments(&self) -> &[String] {
        &self.arguments
    }

    #[cfg(any(test, target_os = "linux"))]
    pub(crate) fn working_directory(&self) -> &PathBuf {
        &self.working_directory
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    retained: usize,
    limit_exceeded: bool,
}

/// Drains currently available bytes without blocking; returns true only on EOF.
#[cfg(target_os = "linux")]
fn drain_pipe(
    pipe: &mut impl Read,
    stdout: bool,
    output: &mut CapturedOutput,
    output_limit: usize,
) -> io::Result<bool> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let available = output_limit.saturating_sub(output.retained);
                let retained = count.min(available);
                if stdout {
                    output.stdout.extend_from_slice(&buffer[..retained]);
                } else {
                    output.stderr.extend_from_slice(&buffer[..retained]);
                }
                output.retained += retained;
                output.limit_exceeded |= retained < count;
                if output.limit_exceeded {
                    return Ok(false);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn run(
    request: &HostProcessRequest,
    cancelled: Arc<AtomicBool>,
    spawn_gate: SpawnAuthorizationGate,
) -> ProcessResult {
    run_with_limits(
        request,
        cancelled,
        spawn_gate,
        HOST_EXEC_WALL_LIMIT,
        HOST_EXEC_OUTPUT_LIMIT,
    )
}

#[cfg(target_os = "linux")]
fn run_with_limits(
    request: &HostProcessRequest,
    cancelled: Arc<AtomicBool>,
    spawn_gate: SpawnAuthorizationGate,
    wall_limit: Duration,
    output_limit: usize,
) -> ProcessResult {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let started = Instant::now();
    if cancelled.load(Ordering::SeqCst) {
        return host_result(HostResultDetails::empty(
            ProcessOutcome::Cancelled,
            output_limit,
            0,
        ));
    }

    let mut command = Command::new(request.executable());
    command
        .args(request.arguments())
        .current_dir(request.working_directory())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A private process group enables cleanup of ordinary descendants. Full Access still permits
    // a child to deliberately escape the group; incomplete cleanup must never report success.
    command.process_group(0);
    let spawn: SpawnAction = Box::new(|mut command| command.spawn());
    let mut child = match spawn_gate(command, spawn) {
        Ok(child) => child,
        Err(SpawnGateError::Denied) => {
            let outcome = if cancelled.load(Ordering::SeqCst) {
                ProcessOutcome::Cancelled
            } else {
                ProcessOutcome::Denied
            };
            return host_result(HostResultDetails::empty(
                outcome,
                output_limit,
                started.elapsed().as_millis() as u64,
            ));
        }
        Err(SpawnGateError::Spawn(error)) => {
            return host_result(
                HostResultDetails::empty(
                    ProcessOutcome::SpawnFailed,
                    output_limit,
                    started.elapsed().as_millis() as u64,
                )
                .with_diagnostic(error.to_string()),
            )
        }
    };
    let process_group = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        return finish_without_readers(
            &mut child,
            process_group,
            started,
            output_limit,
            "host process stdout pipe was unavailable",
        );
    };
    let Some(mut stderr) = child.stderr.take() else {
        return finish_without_readers(
            &mut child,
            process_group,
            started,
            output_limit,
            "host process stderr pipe was unavailable",
        );
    };
    if set_nonblocking(stdout.as_raw_fd()).is_err() || set_nonblocking(stderr.as_raw_fd()).is_err()
    {
        return finish_without_readers(
            &mut child,
            process_group,
            started,
            output_limit,
            "host process output could not be supervised",
        );
    }

    let mut output = CapturedOutput::default();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut terminal_outcome = None;
    let mut leader_exited = false;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            terminal_outcome = Some(ProcessOutcome::Cancelled);
            break;
        }
        let drain_result =
            drain_pipe(&mut stdout, true, &mut output, output_limit).and_then(|eof| {
                stdout_eof |= eof;
                drain_pipe(&mut stderr, false, &mut output, output_limit)
            });
        match drain_result {
            Ok(eof) => stderr_eof |= eof,
            Err(error) => {
                terminal_outcome = Some(ProcessOutcome::OutcomeUnknown);
                let _ = error;
                break;
            }
        }
        if output.limit_exceeded {
            terminal_outcome = Some(ProcessOutcome::OutputLimitExceeded);
            break;
        }
        if started.elapsed() >= wall_limit {
            terminal_outcome = Some(ProcessOutcome::TimedOut);
            break;
        }
        match child_exited_without_reaping(&child) {
            Ok(true) => {
                leader_exited = true;
                break;
            }
            Ok(false) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => {
                terminal_outcome = Some(ProcessOutcome::OutcomeUnknown);
                break;
            }
        }
    }

    let (status, process_group_cleaned) =
        terminate_process_group(process_group, &mut child, leader_exited);
    let cleanup_deadline = Instant::now() + CLEANUP_WAIT;
    while !(stdout_eof && stderr_eof) && Instant::now() < cleanup_deadline {
        match drain_pipe(&mut stdout, true, &mut output, output_limit) {
            Ok(eof) => stdout_eof |= eof,
            Err(_) => break,
        }
        match drain_pipe(&mut stderr, false, &mut output, output_limit) {
            Ok(eof) => stderr_eof |= eof,
            Err(_) => break,
        }
        if !(stdout_eof && stderr_eof) {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
    // This confirms only the original process group and captured pipes. A direct-argv child
    // can call setsid or otherwise escape that group; Full Access gives us no way to discover it.
    let cleanup_confirmed = process_group_cleaned && stdout_eof && stderr_eof;
    let outcome = terminal_outcome.unwrap_or_else(|| match status {
        Some(status) if status.success() => ProcessOutcome::Exited(status.code().unwrap_or(0)),
        Some(status) => status
            .code()
            .map(ProcessOutcome::Exited)
            .unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                ProcessOutcome::Signaled(status.signal().unwrap_or(0))
            }),
        None => ProcessOutcome::OutcomeUnknown,
    });
    let diagnostic = (!cleanup_confirmed).then(|| {
        "original host process-group cleanup or captured-pipe EOF could not be confirmed; a descendant may remain. Processes that escape the group are outside this check"
            .to_owned()
    });
    host_result(HostResultDetails {
        outcome,
        diagnostic,
        stdout: stdout_bytes_to_string(output.stdout),
        stderr: stdout_bytes_to_string(output.stderr),
        cleanup_confirmed,
        recovery_hold: !cleanup_confirmed,
        output_limit,
        output_bytes: output.retained,
        truncated: output.limit_exceeded,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(not(target_os = "linux"))]
fn run_with_limits(
    _request: &HostProcessRequest,
    _cancelled: Arc<AtomicBool>,
    _spawn_gate: SpawnAuthorizationGate,
    _wall_limit: Duration,
    output_limit: usize,
) -> ProcessResult {
    host_result(
        HostResultDetails::empty(ProcessOutcome::HostExecutionUnavailable, output_limit, 0)
            .with_diagnostic("direct host execution is currently supported only on Linux"),
    )
}

#[cfg(target_os = "linux")]
fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: fcntl is called with a live pipe descriptor owned by the child handle.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor remains live while the owning pipe is held by this function.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn child_exited_without_reaping(child: &Child) -> io::Result<bool> {
    // SAFETY: siginfo_t is a plain C output struct; waitid initializes it before it is read.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // WNOWAIT keeps an exited group leader as a zombie until after the group signal. This pins
    // the numeric PGID and avoids signalling an unrelated group after PID reuse.
    // SAFETY: `info` is writable and `child.id()` names this process's waitable child.
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if waited == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid initialized the siginfo value on success.
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(target_os = "linux")]
fn terminate_process_group(
    process_group: u32,
    child: &mut Child,
    leader_exited: bool,
) -> (Option<ExitStatus>, bool) {
    let group = process_group as libc::pid_t;
    // SAFETY: negative `group` targets the process group created for this child.
    let sent = unsafe { libc::kill(-group, libc::SIGKILL) };
    let signal_error = (sent == -1).then(io::Error::last_os_error);
    let signal_ok = sent == 0
        || signal_error
            .as_ref()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound);
    if !signal_ok && !leader_exited {
        // Do not block forever if a host policy prevented signalling a still-running process.
        return (None, false);
    }
    let deadline = Instant::now() + CLEANUP_WAIT;
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        // SAFETY: signal 0 only checks existence for the private process group.
        let exists = unsafe { libc::kill(-group, 0) } == 0
            || io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied;
        if !exists {
            let waited = status.is_some();
            return (status, signal_ok && waited);
        }
        if Instant::now() >= deadline {
            return (status, false);
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(target_os = "linux")]
fn finish_without_readers(
    child: &mut Child,
    process_group: u32,
    started: Instant,
    output_limit: usize,
    diagnostic: &str,
) -> ProcessResult {
    // The process group can be signalled and reaped here, but without output readers we cannot
    // verify that a detached descendant has closed the captured pipes. Do not let a successful
    // group check hide that uncertainty or release the task's recovery hold.
    let _ = terminate_process_group(process_group, child, false);
    host_result(HostResultDetails {
        outcome: ProcessOutcome::OutcomeUnknown,
        diagnostic: Some(format!(
            "{diagnostic}; captured output pipe EOF could not be checked, so cleanup remains unconfirmed"
        )),
        stdout: String::new(),
        stderr: String::new(),
        cleanup_confirmed: false,
        recovery_hold: true,
        output_limit,
        output_bytes: 0,
        truncated: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(target_os = "linux")]
fn stdout_bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

struct HostResultDetails {
    outcome: ProcessOutcome,
    diagnostic: Option<String>,
    stdout: String,
    stderr: String,
    cleanup_confirmed: bool,
    recovery_hold: bool,
    output_limit: usize,
    output_bytes: usize,
    truncated: bool,
    elapsed_ms: u64,
}

impl HostResultDetails {
    fn empty(outcome: ProcessOutcome, output_limit: usize, elapsed_ms: u64) -> Self {
        Self {
            outcome,
            diagnostic: None,
            stdout: String::new(),
            stderr: String::new(),
            cleanup_confirmed: true,
            recovery_hold: false,
            output_limit,
            output_bytes: 0,
            truncated: false,
            elapsed_ms,
        }
    }

    fn with_diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostic = Some(diagnostic.into());
        self
    }
}

fn host_result(details: HostResultDetails) -> ProcessResult {
    let mut result = ProcessResult::empty_host_process(details.outcome);
    if let Some(diagnostic) = details.diagnostic {
        result.diagnostic = result
            .diagnostic
            .map(|baseline| format!("{baseline}; {diagnostic}"));
    }
    result.stdout = details.stdout;
    result.stderr = details.stderr;
    result.cleanup_confirmed = details.cleanup_confirmed;
    result.recovery_hold = details.recovery_hold;
    result.output_limit = details.output_limit;
    result.output_bytes = details.output_bytes;
    result.truncated = details.truncated;
    result.elapsed_ms = details.elapsed_ms;
    result
}

#[cfg(test)]
mod tests {
    use super::{
        host_result, HostProcessRequest, HostResultDetails, MAX_HOST_EXEC_ARGUMENTS,
        MAX_HOST_EXEC_ARGUMENT_BYTES,
    };
    use crate::contracts::process_execution::ProcessOutcome;

    #[cfg(target_os = "linux")]
    fn unrestricted_test_gate() -> krypper_sandbox::SpawnAuthorizationGate {
        Box::new(|command, spawn| spawn(command).map_err(krypper_sandbox::SpawnGateError::Spawn))
    }

    #[test]
    fn host_request_accepts_direct_argv_and_empty_arguments_without_shell_parsing() {
        let request = HostProcessRequest::new(
            "/usr/bin/printf".into(),
            vec!["%s".into(), "literal; text".into(), String::new()],
            "/tmp".into(),
        )
        .unwrap();

        assert_eq!(request.executable(), "/usr/bin/printf");
        assert_eq!(request.arguments()[1], "literal; text");
    }

    #[test]
    fn host_request_rejects_control_characters_and_oversized_argv() {
        assert!(HostProcessRequest::new("echo\n".into(), vec![], "/tmp".into()).is_err());
        assert!(HostProcessRequest::new(
            "echo".into(),
            vec!["x".repeat(MAX_HOST_EXEC_ARGUMENT_BYTES + 1)],
            "/tmp".into(),
        )
        .is_err());
        assert!(HostProcessRequest::new(
            "echo".into(),
            vec![String::new(); MAX_HOST_EXEC_ARGUMENTS + 1],
            "/tmp".into(),
        )
        .is_err());
    }

    #[test]
    fn unconfirmed_descendant_cleanup_is_never_reported_as_success() {
        let result = host_result(HostResultDetails {
            outcome: ProcessOutcome::Exited(0),
            diagnostic: Some("host process-group cleanup could not be confirmed".into()),
            stdout: String::new(),
            stderr: String::new(),
            cleanup_confirmed: false,
            recovery_hold: true,
            output_limit: 1024,
            output_bytes: 0,
            truncated: false,
            elapsed_ms: 1,
        });

        assert_eq!(
            result.profile,
            crate::ProcessExecutionProfile::FullAccessHostProcessV1
        );
        assert!(!result.cleanup_confirmed);
        assert!(result.recovery_hold);
        assert!(!result.succeeded());
        assert!(result
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("no Krypper sandbox")
                && message.contains("cleanup could not be confirmed")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failure_to_establish_output_readers_always_latches_recovery_hold() {
        use super::finish_without_readers;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().expect("start host process fixture");
        let process_group = child.id();
        let result = finish_without_readers(
            &mut child,
            process_group,
            Instant::now(),
            1024,
            "host process output could not be supervised",
        );

        assert_eq!(result.outcome, ProcessOutcome::OutcomeUnknown);
        assert!(!result.cleanup_confirmed);
        assert!(result.recovery_hold);
        assert!(result
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("output pipe EOF could not be checked")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_at_spawn_gate_is_reported_as_cancelled_without_launch() {
        use super::{run_with_limits, HOST_EXEC_OUTPUT_LIMIT};
        use krypper_sandbox::{SpawnAction, SpawnGateError};
        use std::process::Command;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let request = HostProcessRequest::new("/usr/bin/true".into(), Vec::new(), "/tmp".into())
            .expect("direct host request");
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_at_gate = Arc::clone(&cancelled);
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawn_calls_at_gate = Arc::clone(&spawn_calls);
        let gate = Box::new(move |_command: Command, _spawn: SpawnAction| {
            cancellation_at_gate.store(true, Ordering::SeqCst);
            spawn_calls_at_gate.fetch_add(1, Ordering::SeqCst);
            Err(SpawnGateError::Denied)
        });

        let result = run_with_limits(
            &request,
            cancelled,
            gate,
            Duration::from_secs(1),
            HOST_EXEC_OUTPUT_LIMIT,
        );

        assert_eq!(result.outcome, ProcessOutcome::Cancelled);
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
        assert!(result.cleanup_confirmed);
        assert!(!result.recovery_hold);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_before_spawn_does_not_invoke_host_process() {
        use super::{run_with_limits, HOST_EXEC_OUTPUT_LIMIT};
        use krypper_sandbox::{SpawnAction, SpawnGateError};
        use std::process::Command;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let request =
            HostProcessRequest::new("/usr/bin/true".into(), vec![], "/tmp".into()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&spawn_calls);
        let gate = Box::new(move |_command: Command, _spawn: SpawnAction| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(SpawnGateError::Denied)
        });

        let result = run_with_limits(
            &request,
            cancelled,
            gate,
            Duration::from_secs(1),
            HOST_EXEC_OUTPUT_LIMIT,
        );

        assert_eq!(result.outcome, ProcessOutcome::Cancelled);
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        assert!(result.cleanup_confirmed);
        assert!(!result.recovery_hold);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runner_executes_outside_workspace_in_the_requested_host_directory() {
        use super::{run_with_limits, HOST_EXEC_OUTPUT_LIMIT};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::time::Duration;

        let directory = tempfile::tempdir().expect("host fixture directory");
        let request = HostProcessRequest::new(
            "/bin/pwd".into(),
            Vec::new(),
            directory.path().to_string_lossy().into_owned(),
        )
        .expect("direct host request");
        let result = run_with_limits(
            &request,
            Arc::new(AtomicBool::new(false)),
            unrestricted_test_gate(),
            Duration::from_secs(2),
            HOST_EXEC_OUTPUT_LIMIT,
        );

        assert_eq!(result.outcome, ProcessOutcome::Exited(0));
        assert_eq!(
            result.profile,
            crate::ProcessExecutionProfile::FullAccessHostProcessV1
        );
        assert_eq!(result.stdout.trim(), directory.path().to_string_lossy());
        assert!(result.cleanup_confirmed);
        assert!(result.sandbox_evidence.is_none());
        assert!(!result.recovery_hold);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runner_can_reach_the_unrestricted_host_network() {
        use super::{run_with_limits, HOST_EXEC_OUTPUT_LIMIT};
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback fixture listener");
        let address = listener.local_addr().expect("listener address");
        let accept = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("host process connected");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let mut received = [0_u8; 14];
            stream.read_exact(&mut received).expect("read host payload");
            received
        });
        let request = HostProcessRequest::new(
            "/bin/bash".into(),
            vec![
                "-c".into(),
                format!(
                    "printf 'HOST_EGRESS_OK' >/dev/tcp/127.0.0.1/{}",
                    address.port()
                ),
            ],
            "/tmp".into(),
        )
        .expect("direct host request");
        let result = run_with_limits(
            &request,
            Arc::new(AtomicBool::new(false)),
            unrestricted_test_gate(),
            Duration::from_secs(3),
            HOST_EXEC_OUTPUT_LIMIT,
        );
        let received = accept.join().expect("listener thread");

        assert_eq!(result.outcome, ProcessOutcome::Exited(0));
        assert_eq!(&received, b"HOST_EGRESS_OK");
        assert!(result.cleanup_confirmed);
        assert!(result.sandbox_evidence.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signal_cancellation_kills_the_host_process_group_and_reports_cleanup() {
        use super::{run_with_limits, HOST_EXEC_OUTPUT_LIMIT};
        use std::fs;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::thread;
        use std::time::{Duration, Instant};

        let directory = tempfile::tempdir().expect("host fixture directory");
        let leader_file = directory.path().join("leader.pid");
        let descendant_file = directory.path().join("descendant.pid");
        let script = format!(
            "echo $$ > '{}'; sleep 30 & echo $! > '{}'; wait",
            leader_file.display(),
            descendant_file.display()
        );
        let request = HostProcessRequest::new(
            "/bin/sh".into(),
            vec!["-c".into(), script],
            directory.path().to_string_lossy().into_owned(),
        )
        .expect("direct host request");
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal_registration =
            signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&cancelled))
                .expect("register test signal");
        let trigger_path = descendant_file.clone();
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && !trigger_path.exists() {
                thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: SIGUSR1 is registered above and targets this process's test cancellation flag.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGUSR1);
            }
        });

        let result = run_with_limits(
            &request,
            Arc::clone(&cancelled),
            unrestricted_test_gate(),
            Duration::from_secs(5),
            HOST_EXEC_OUTPUT_LIMIT,
        );
        trigger.join().expect("signal trigger thread");
        signal_hook::low_level::unregister(signal_registration);

        assert_eq!(result.outcome, ProcessOutcome::Cancelled);
        assert!(
            result.cleanup_confirmed || result.recovery_hold,
            "failed group cleanup must be reported as a recovery hold"
        );
        assert!(
            leader_file.exists(),
            "the host process started before cancellation"
        );
        assert!(
            descendant_file.exists(),
            "the descendant started before cancellation"
        );
        let leader_pid = fs::read_to_string(leader_file)
            .expect("leader pid")
            .trim()
            .parse::<libc::pid_t>()
            .expect("valid leader pid");
        let descendant_pid = fs::read_to_string(descendant_file)
            .expect("descendant pid")
            .trim()
            .parse::<libc::pid_t>()
            .expect("valid descendant pid");
        assert!(
            !process_is_running(leader_pid) || result.recovery_hold,
            "leader must be gone or cleanup must remain explicitly uncertain"
        );
        assert!(
            !process_is_running(descendant_pid) || result.recovery_hold,
            "descendant must be gone or cleanup must remain explicitly uncertain"
        );
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: libc::pid_t) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(") ")
            .and_then(|(_, status)| status.chars().next())
            .is_some_and(|status| status != 'Z' && status != 'X')
    }
}
