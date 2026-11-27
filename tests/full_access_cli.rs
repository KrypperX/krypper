#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const FULL_ACCESS_LEADER_PID: &str = "KRYPPER_TEST_FULL_ACCESS_LEADER_PID";
const FULL_ACCESS_DESCENDANT_PID: &str = "KRYPPER_TEST_FULL_ACCESS_DESCENDANT_PID";
const PTY_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
#[ignore = "spawned as a direct-argv Full Access fixture"]
fn full_access_process_group_child_fixture() {
    let (Ok(leader_path), Ok(descendant_path)) = (
        std::env::var(FULL_ACCESS_LEADER_PID),
        std::env::var(FULL_ACCESS_DESCENDANT_PID),
    ) else {
        return;
    };

    fs::write(leader_path, std::process::id().to_string()).expect("write host process pid");
    let mut descendant = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("start same-group descendant");
    fs::write(descendant_path, descendant.id().to_string()).expect("write descendant pid");
    let _ = descendant.wait();
}

#[test]
fn terminal_sigint_cancels_full_access_host_process_group_before_another_model_request() {
    let root = TempDir::new().expect("create fixture root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let config_home = root.path().join("config-home");
    fs::create_dir(&config_home).expect("create configuration home");
    set_private_directory(&config_home);
    let profile_directory = config_home.join("krypper");
    fs::create_dir_all(&profile_directory).expect("create per-user settings directory");
    set_private_directory(&profile_directory);

    let provider_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind provider fixture");
    provider_listener
        .set_nonblocking(true)
        .expect("make provider listener nonblocking");
    let provider_address = provider_listener
        .local_addr()
        .expect("read provider address");
    let profiles = json!({
        "version": 1,
        "profiles": [{
            "id": "fixture",
            "label": "Local Full Access fixture",
            "enabled": true,
            "protocol": "openai-compatible-chat",
            "endpoint": format!("http://{provider_address}/v1/chat/completions"),
            "credential_ref": "profile:fixture:api-key"
        }]
    });
    let profiles_path = profile_directory.join("provider-profiles.json");
    fs::write(
        &profiles_path,
        serde_json::to_vec(&profiles).expect("encode provider profile"),
    )
    .expect("write provider profile");
    set_private_file(&profiles_path);

    let helper_executable = std::env::current_exe().expect("locate process fixture executable");
    let action_arguments = json!({
        "executable": helper_executable,
        "argv": [
            "--ignored",
            "--exact",
            "full_access_process_group_child_fixture",
            "--nocapture"
        ],
        "working_directory": workspace
    });
    let confirmation_submitted = Arc::new(AtomicBool::new(false));
    let provider = start_provider_server(
        provider_listener,
        action_arguments,
        Arc::clone(&confirmation_submitted),
    );
    provider
        .ready
        .recv_timeout(PTY_READ_TIMEOUT)
        .expect("start provider request monitor before the CLI");

    let (master, slave) = open_pty();
    enable_terminal_interrupts(&slave).expect("enable terminal Ctrl-C");
    let leader_pid_path = workspace.join("leader.pid");
    let descendant_pid_path = workspace.join("descendant.pid");
    let mut command = Command::new(env!("CARGO_BIN_EXE_krypper"));
    command
        .args([
            "task",
            "--goal",
            "Run one direct-argv process and cancel it from the terminal",
            "--max-actions",
            "2",
            "--workspace",
        ])
        .arg(&workspace)
        .args([
            "--model",
            "fixture-model",
            "--provider",
            "fixture",
            "--permission-mode",
            "full-access",
            "--no-config",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path().join("home"))
        .env("USERPROFILE", root.path().join("home"))
        .env("XDG_CONFIG_HOME", &config_home)
        .env(
            "KRYPPER_CONFIG",
            root.path().join("unused-model-config.toml"),
        )
        .env("KRYPPER_PROVIDER_FIXTURE_API_KEY", "local-test-key")
        .env(FULL_ACCESS_LEADER_PID, &leader_pid_path)
        .env(FULL_ACCESS_DESCENDANT_PID, &descendant_pid_path)
        .stdin(Stdio::from(
            slave.try_clone().expect("clone PTY slave for stdin"),
        ))
        .stdout(Stdio::from(
            slave.try_clone().expect("clone PTY slave for stdout"),
        ))
        .stderr(Stdio::from(
            slave.try_clone().expect("clone PTY slave for stderr"),
        ));
    // The CLI must own a new session and controlling terminal so a byte-level Ctrl-C
    // sent to the PTY becomes SIGINT for the foreground CLI process group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::tcsetpgrp(0, libc::getpgrp()) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    drop(slave);
    let child = command.spawn().expect("start Krypper in a controlling PTY");
    drop(command);
    let mut child = CliChildGuard::new(child, leader_pid_path.clone());
    let output_receiver = start_terminal_reader(&master);

    let mut terminal_output = Vec::new();
    receive_until(
        &output_receiver,
        &mut terminal_output,
        "Confirm Full Access for this task only?",
        PTY_READ_TIMEOUT,
    );
    assert!(
        String::from_utf8_lossy(&terminal_output).contains("no process sandbox"),
        "the trusted terminal should describe Full Access risk before confirmation"
    );
    let (drained_sender, drained_receiver) = mpsc::channel();
    provider
        .control
        .send(ProviderCommand::DrainBeforeConfirmation(drained_sender))
        .expect("synchronize with the active provider monitor");
    drained_receiver
        .recv_timeout(PTY_READ_TIMEOUT)
        .expect("drain any provider request queued before confirmation");
    let mut terminal = master.try_clone().expect("clone PTY master for input");
    terminal.write_all(b"yes\n").expect("confirm Full Access");
    terminal.flush().expect("flush Full Access confirmation");
    // Keep the monitor in its pre-confirmation phase through the PTY write. A request
    // accepted during that transition is conservatively counted as premature.
    confirmation_submitted.store(true, Ordering::SeqCst);

    wait_for_file(
        &leader_pid_path,
        &mut child.child,
        &output_receiver,
        &mut terminal_output,
    );
    wait_for_file(
        &descendant_pid_path,
        &mut child.child,
        &output_receiver,
        &mut terminal_output,
    );

    terminal.write_all(&[0x03]).expect("send terminal Ctrl-C");
    terminal.flush().expect("flush terminal Ctrl-C");
    let status = wait_for_exit(
        &mut child.child,
        &output_receiver,
        &mut terminal_output,
        Duration::from_secs(8),
    );
    drop(terminal);
    drop(master);
    let reader_output = output_receiver.into_iter().flatten().collect::<Vec<_>>();
    terminal_output.extend(reader_output);

    provider
        .control
        .send(ProviderCommand::Stop)
        .expect("stop provider request monitor");
    let requests = provider.server.join().expect("join local provider fixture");
    let output = String::from_utf8_lossy(&terminal_output);
    assert_eq!(status.code(), Some(130), "{output}");
    assert!(output.contains("Task state: cancelled"), "{output}");
    assert!(output.contains("Turn state: cancelled"), "{output}");
    assert!(
        output.contains("Task outcome: cancelled by caller"),
        "{output}"
    );
    assert!(
        output.contains("process_group_cleanup=true recovery_hold=false"),
        "{output}"
    );
    assert_eq!(
        requests.len(),
        1,
        "cancellation must stop before another model request"
    );
    assert_eq!(
        provider.premature_requests.load(Ordering::SeqCst),
        0,
        "the active provider monitor must see no request before confirmation is submitted"
    );
    assert!(String::from_utf8_lossy(&requests[0]).contains("full_access_host_process"));

    let leader_pid = read_pid(&leader_pid_path);
    let descendant_pid = read_pid(&descendant_pid_path);
    assert!(
        !process_exists(leader_pid),
        "Full Access process leader must be reaped"
    );
    assert!(
        !process_exists(descendant_pid),
        "same-group descendant must be gone when cleanup is reported as confirmed"
    );
}

struct CliChildGuard {
    child: Child,
    host_leader_pid_path: PathBuf,
}

impl CliChildGuard {
    fn new(child: Child, host_leader_pid_path: PathBuf) -> Self {
        Self {
            child,
            host_leader_pid_path,
        }
    }
}

impl Drop for CliChildGuard {
    fn drop(&mut self) {
        if let Ok(contents) = fs::read_to_string(&self.host_leader_pid_path) {
            if let Ok(pid) = contents.trim().parse::<libc::pid_t>() {
                // SAFETY: this fixture makes the host process its private process-group leader.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: the CLI is the leader of the session/process group created in pre_exec.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn open_pty() -> (fs::File, fs::File) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes both descriptors on success and receives no optional buffers.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "open a PTY: {}", io::Error::last_os_error());
    // SAFETY: each descriptor was returned by openpty and is now owned by one File.
    unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) }
}

fn enable_terminal_interrupts(slave: &fs::File) -> io::Result<()> {
    // SAFETY: termios is initialized by tcgetattr before it is read or modified.
    let mut attributes = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: the descriptor is the open PTY slave.
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) } == -1 {
        return Err(io::Error::last_os_error());
    }
    attributes.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
    attributes.c_cc[libc::VINTR] = 0x03;
    // SAFETY: the updated termios structure came from tcgetattr for this descriptor.
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn start_terminal_reader(master: &fs::File) -> Receiver<Vec<u8>> {
    let mut reader = master.try_clone().expect("clone PTY master for output");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    if sender.send(buffer[..length].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    receiver
}

fn receive_until(
    receiver: &Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while !String::from_utf8_lossy(output).contains(expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for PTY output: {expected}"
        );
        let bytes = receiver
            .recv_timeout(remaining)
            .unwrap_or_else(|error| panic!("PTY output ended before {expected}: {error}"));
        output.extend(bytes);
    }
}

fn wait_for_file(
    path: &Path,
    child: &mut Child,
    receiver: &Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
) {
    let deadline = Instant::now() + PTY_READ_TIMEOUT;
    while !path.exists() {
        if let Some(status) = child
            .try_wait()
            .expect("poll CLI while waiting for host process")
        {
            while let Ok(bytes) = receiver.try_recv() {
                output.extend(bytes);
            }
            panic!(
                "Krypper exited with {status} before host fixture file {path:?}; terminal output: {}",
                String::from_utf8_lossy(output)
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for host fixture file {path:?}; terminal output: {}",
            String::from_utf8_lossy(output)
        );
        if let Ok(bytes) = receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            output.extend(bytes);
        }
    }
}

fn wait_for_exit(
    child: &mut Child,
    receiver: &Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    timeout: Duration,
) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll CLI status") {
            while let Ok(bytes) = receiver.try_recv() {
                output.extend(bytes);
            }
            return status;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "Krypper did not stop after terminal Ctrl-C"
        );
        if let Ok(bytes) = receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            output.extend(bytes);
        }
    }
}

enum ProviderCommand {
    DrainBeforeConfirmation(mpsc::Sender<()>),
    Stop,
}

struct ProviderFixture {
    server: thread::JoinHandle<Vec<Vec<u8>>>,
    control: mpsc::Sender<ProviderCommand>,
    ready: Receiver<()>,
    premature_requests: Arc<AtomicUsize>,
}

fn start_provider_server(
    listener: TcpListener,
    action_arguments: Value,
    confirmation_submitted: Arc<AtomicBool>,
) -> ProviderFixture {
    let (control_sender, control_receiver) = mpsc::channel();
    let (ready_sender, ready_receiver) = mpsc::channel();
    let premature_requests = Arc::new(AtomicUsize::new(0));
    let premature_for_server = Arc::clone(&premature_requests);
    let handle = thread::spawn(move || {
        ready_sender
            .send(())
            .expect("report provider monitor readiness");
        let mut requests = Vec::new();
        let mut stop_deadline = None;
        loop {
            match control_receiver.try_recv() {
                Ok(ProviderCommand::DrainBeforeConfirmation(drained)) => {
                    while let Some(stream) = accept_before(&listener, Duration::ZERO) {
                        handle_provider_request(
                            stream,
                            &action_arguments,
                            &confirmation_submitted,
                            &premature_for_server,
                            &mut requests,
                        );
                    }
                    drained
                        .send(())
                        .expect("acknowledge provider request drain");
                }
                Ok(ProviderCommand::Stop) => {
                    stop_deadline
                        .get_or_insert_with(|| Instant::now() + Duration::from_millis(200));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    stop_deadline
                        .get_or_insert_with(|| Instant::now() + Duration::from_millis(200));
                }
            }

            if let Some(stream) = accept_before(&listener, Duration::from_millis(5)) {
                handle_provider_request(
                    stream,
                    &action_arguments,
                    &confirmation_submitted,
                    &premature_for_server,
                    &mut requests,
                );
            }

            if stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
        }
        requests
    });
    ProviderFixture {
        server: handle,
        control: control_sender,
        ready: ready_receiver,
        premature_requests,
    }
}

fn handle_provider_request(
    mut stream: TcpStream,
    action_arguments: &Value,
    confirmation_submitted: &AtomicBool,
    premature_requests: &AtomicUsize,
    requests: &mut Vec<Vec<u8>>,
) {
    if !confirmation_submitted.load(Ordering::SeqCst) {
        premature_requests.fetch_add(1, Ordering::SeqCst);
    }
    let Ok(request) = read_http_request(&mut stream) else {
        return;
    };
    requests.push(request);
    let response = if requests.len() == 1 {
        tool_call_response("full_access_host_process", action_arguments.clone())
    } else {
        tool_call_response(
            "complete_task",
            json!({"result":"unexpected follow-up model request"}),
        )
    };
    write_http_response(&mut stream, response);
}

fn accept_before(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended early",
            ));
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > 2 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
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
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing content length"))?;
        if request.len() >= header_end + 4 + content_length {
            return Ok(request);
        }
    }
}

fn write_http_response(stream: &mut TcpStream, body: String) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write local provider response");
}

fn tool_call_response(name: &str, arguments: Value) -> String {
    json!({
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "full_access_fixture_call",
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }]
            }
        }]
    })
    .to_string()
}

fn read_pid(path: &Path) -> libc::pid_t {
    fs::read_to_string(path)
        .expect("read process fixture pid")
        .trim()
        .parse()
        .expect("parse process fixture pid")
}

fn process_exists(pid: libc::pid_t) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, status)| status.chars().next())
        .is_some_and(|status| status != 'Z' && status != 'X')
}

#[cfg(unix)]
fn set_private_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("protect settings directory");
}

#[cfg(unix)]
fn set_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("protect provider profile");
}
