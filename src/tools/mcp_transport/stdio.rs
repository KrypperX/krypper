//! Bounded stdio launch, nonblocking I/O and process cleanup for MCP clients.
//!
//! The child starts with a cleared, minimal environment and its own process
//! group. Those controls support predictable launch and cleanup; they do not
//! sandbox the child. It runs with the Krypper process's OS-user permissions
//! and has no filesystem or network confinement from this transport.

use super::config::McpStdioConfig;
use super::lifecycle::{McpCancellation, McpLifecycleError};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const MAX_PROCESS_GROUP_MEMBERS: usize = 65_536;

pub(super) struct StdioChild {
    pub child: ManagedChild,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    stderr_capture: StderrCapture,
    pub pending: Vec<u8>,
}

#[derive(Default)]
struct StderrCaptureState {
    retained: Vec<u8>,
}

struct StderrCapture {
    state: Arc<Mutex<StderrCaptureState>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl StderrCapture {
    fn start(mut stderr: ChildStderr) -> Result<Self, McpLifecycleError> {
        let state = Arc::new(Mutex::new(StderrCaptureState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_state = Arc::clone(&state);
        let reader_stop = Arc::clone(&stop);
        let reader = thread::Builder::new()
            .name("krypper-mcp-stderr-drain".into())
            .spawn(move || {
                let mut buffer = [0; 8192];
                while !reader_stop.load(Ordering::Acquire) {
                    match stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut state = reader_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let remaining = MAX_STDERR_BYTES.saturating_sub(state.retained.len());
                            let retained = read.min(remaining);
                            state.retained.extend_from_slice(&buffer[..retained]);
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) =>
                        {
                            thread::sleep(POLL_INTERVAL);
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|_| McpLifecycleError::SpawnFailed)?;
        Ok(Self {
            state,
            stop,
            reader: Some(reader),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<u8> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retained.clone()
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        let _ = self.child.terminate();
        self.stderr_capture.stop();
    }
}

pub(super) struct ManagedChild(Option<Child>);

impl std::ops::Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl ManagedChild {
    pub(super) fn has_exited(&mut self) -> io::Result<bool> {
        let Some(child) = self.0.as_ref() else {
            return Ok(false);
        };
        child_has_exited(child)
    }

    pub(super) fn terminate(&mut self) -> Result<(), McpLifecycleError> {
        self.terminate_with_group_signal(|pgid| {
            // SAFETY: `kill` takes only integers, and `pgid` is the process group established
            // for this child by `process_group(0)` at spawn. This runs before reaping the child,
            // so the group ID cannot have been recycled.
            if unsafe { libc::kill(-pgid, libc::SIGKILL) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn terminate_with_signal_error(
        &mut self,
        error: io::Error,
    ) -> Result<(), McpLifecycleError> {
        // Tests use this seam to reproduce macOS permission errors without relying on setuid.
        self.terminate_with_group_signal(|_| Err(error))
    }

    fn terminate_with_group_signal(
        &mut self,
        send_signal: impl FnOnce(libc::pid_t) -> io::Result<()>,
    ) -> Result<(), McpLifecycleError> {
        let Some(mut child) = self.0.take() else {
            return Ok(());
        };
        // spawn_inner uses process_group(0), so the child's PID is also its PGID.
        // Send SIGKILL before reaping: while the child remains unreaped, its PID
        // cannot be recycled into an unrelated process group.
        let pgid = child.id() as libc::pid_t;
        if let Err(error) = send_signal(pgid) {
            #[cfg(target_os = "macos")]
            let macos_eperm_with_exited_leader = error.raw_os_error() == Some(libc::EPERM)
                && child_has_exited(&child).unwrap_or(false);
            #[cfg(not(target_os = "macos"))]
            let macos_eperm_with_exited_leader = false;
            if error.raw_os_error() != Some(libc::ESRCH) && !macos_eperm_with_exited_leader {
                let _ = child.kill();
                if !reap(&mut child) {
                    thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                return Err(McpLifecycleError::CleanupFailed);
            }
            // On macOS, EPERM can mean either that the group contains only zombies or that it
            // contains a live process this caller cannot signal. Reap the leader, then verify
            // group emptiness below before reporting success.
        }
        if !reap(&mut child) {
            thread::spawn(move || {
                let _ = child.wait();
            });
            return Err(McpLifecycleError::CleanupFailed);
        }
        if !wait_for_process_group_empty(pgid) {
            return Err(McpLifecycleError::CleanupFailed);
        }
        Ok(())
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn child_has_exited(child: &Child) -> io::Result<bool> {
    let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `child` is still owned and unreaped. `status` points to writable storage,
    // and WNOWAIT observes its exit without releasing the PID used as its process-group ID.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            status.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded and initialized the siginfo_t output.
    Ok(unsafe { status.assume_init() }.si_signo != 0)
}

fn reap(child: &mut Child) -> bool {
    let start = Instant::now();
    while start.elapsed() < REAP_TIMEOUT {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            _ => {}
        }
        thread::sleep(POLL_INTERVAL);
    }
    false
}

fn wait_for_process_group_empty(pgid: libc::pid_t) -> bool {
    let start = Instant::now();
    while start.elapsed() < REAP_TIMEOUT {
        match process_group_is_empty(pgid) {
            Ok(true) => return true,
            Ok(false) | Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
    matches!(process_group_is_empty(pgid), Ok(true))
}

#[cfg(target_os = "macos")]
fn process_group_is_empty(pgid: libc::pid_t) -> io::Result<bool> {
    let mut pids = vec![0; 64];
    loop {
        let buffer_size = std::mem::size_of_val(pids.as_slice()) as libc::c_int;
        // Clear errno so a zero result can be distinguished from libproc's error return.
        // SAFETY: `__error` returns this thread's writable errno slot.
        unsafe { *libc::__error() = 0 };
        // SAFETY: `pids` is writable storage of `buffer_size` bytes and `pgid` was captured
        // from the child created for this process group.
        let count = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), buffer_size) };
        // SAFETY: `__error` returns this thread's errno slot.
        let errno = unsafe { *libc::__error() };
        if errno != 0 {
            return Err(io::Error::from_raw_os_error(errno));
        }
        if count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "libproc returned a negative process count",
            ));
        }

        let count = count as usize;
        if count == 0 {
            return Ok(true);
        }
        if count < pids.len() {
            return Ok(false);
        }
        if pids.len() >= MAX_PROCESS_GROUP_MEMBERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process group is too large to verify completely",
            ));
        }
        pids.resize((pids.len() * 2).min(MAX_PROCESS_GROUP_MEMBERS), 0);
    }
}

#[cfg(not(target_os = "macos"))]
fn process_group_is_empty(pgid: libc::pid_t) -> io::Result<bool> {
    // SAFETY: signal 0 probes the process group and does not deliver a signal.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(true),
        Some(libc::EPERM) => Ok(false),
        _ => Err(error),
    }
}

pub(super) fn spawn(
    config: &McpStdioConfig,
    deadline: Instant,
    cancel: &McpCancellation,
) -> Result<StdioChild, McpLifecycleError> {
    let config = config.clone();
    let worker_cancel = cancel.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = spawn_inner(&config, &worker_cancel);
        let _ = sender.send(result);
        let _ = done_sender.send(());
    });
    let cleanup_aborted = || -> Result<(), McpLifecycleError> {
        done_receiver
            .recv_timeout(REAP_TIMEOUT)
            .map_err(|_| McpLifecycleError::CleanupFailed)
            .and_then(|_| match receiver.try_recv() {
                Ok(Ok(mut stdio)) => stdio.child.terminate(),
                Ok(Err(_)) => Ok(()),
                Err(_) => Err(McpLifecycleError::CleanupFailed),
            })
    };
    loop {
        if cancel.is_cancelled() {
            cleanup_aborted()?;
            return Err(McpLifecycleError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cleanup_aborted()?;
            return Err(McpLifecycleError::Timeout);
        }
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(Ok(mut stdio)) if cancel.is_cancelled() => {
                stdio.child.terminate()?;
                return Err(McpLifecycleError::Cancelled);
            }
            Ok(Err(_)) if cancel.is_cancelled() => return Err(McpLifecycleError::Cancelled),
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(McpLifecycleError::SpawnFailed),
        }
    }
}

fn spawn_inner(
    config: &McpStdioConfig,
    cancel: &McpCancellation,
) -> Result<StdioChild, McpLifecycleError> {
    #[cfg(test)]
    if config
        .args
        .iter()
        .any(|arg| arg.contains("krypper-spawn-delay"))
    {
        thread::sleep(Duration::from_millis(50));
    }
    if cancel.is_cancelled() {
        return Err(McpLifecycleError::Cancelled);
    }
    let mut command = Command::new(&config.executable);
    command
        .args(&config.args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ManagedChild(Some(
        command
            .spawn()
            .map_err(|_| McpLifecycleError::SpawnFailed)?,
    ));
    let stdin = child.stdin.take().ok_or(McpLifecycleError::SpawnFailed)?;
    let stdout = child.stdout.take().ok_or(McpLifecycleError::SpawnFailed)?;
    let stderr = child.stderr.take().ok_or(McpLifecycleError::SpawnFailed)?;
    set_nonblocking(stdin.as_raw_fd())?;
    set_nonblocking(stdout.as_raw_fd())?;
    set_nonblocking(stderr.as_raw_fd())?;
    let stderr_capture = StderrCapture::start(stderr)?;
    Ok(StdioChild {
        child,
        stdin,
        stdout,
        stderr_capture,
        pending: Vec::new(),
    })
}

pub(super) fn write_all(
    stdin: &mut ChildStdin,
    bytes: &[u8],
    deadline: Instant,
    cancel: &McpCancellation,
) -> Result<(), McpLifecycleError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        check(deadline, cancel)?;
        match stdin.write(remaining) {
            Ok(0) => return Err(McpLifecycleError::Disconnected),
            Ok(count) => remaining = &remaining[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Err(McpLifecycleError::Disconnected)
            }
            Err(error) => return Err(McpLifecycleError::Io(error.kind())),
        }
    }
    Ok(())
}

pub(super) fn read_line(
    child: &mut ManagedChild,
    stdout: &mut ChildStdout,
    pending: &mut Vec<u8>,
    deadline: Instant,
    cancel: &McpCancellation,
) -> Result<Vec<u8>, McpLifecycleError> {
    let mut response = std::mem::take(pending);
    loop {
        check(deadline, cancel)?;
        let before = response.len();
        let (stdout_eof, _, _) = drain(stdout, &mut response, MAX_RESPONSE_BYTES)?;
        let progressed = response.len() > before;
        if let Some(position) = response.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = response.drain(..=position).collect();
            *pending = response;
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            return Ok(line);
        }
        if response.len() >= MAX_RESPONSE_BYTES {
            return Err(McpLifecycleError::OutputLimitExceeded);
        }
        if stdout_eof && !progressed && matches!(child.has_exited(), Ok(true)) {
            return Err(McpLifecycleError::Disconnected);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn check(deadline: Instant, cancel: &McpCancellation) -> Result<(), McpLifecycleError> {
    if cancel.is_cancelled() {
        Err(McpLifecycleError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(McpLifecycleError::Timeout)
    } else {
        Ok(())
    }
}

fn set_nonblocking(fd: i32) -> Result<(), McpLifecycleError> {
    // SAFETY: `F_GETFL`/`F_SETFL` take and return plain integers and touch no memory. The
    // caller passes a descriptor it owns and keeps open for this call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: same descriptor and same reasoning as the `F_GETFL` call above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(McpLifecycleError::Io(io::Error::last_os_error().kind()))
    } else {
        Ok(())
    }
}

fn drain(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    limit: usize,
) -> Result<(bool, usize, usize), McpLifecycleError> {
    let mut buffer = [0; 8192];
    let writable = limit.saturating_sub(bytes.len()).min(buffer.len());
    match pipe.read(&mut buffer[..writable.max(1)]) {
        Ok(0) => Ok((true, 0, 0)),
        Ok(read) => {
            let stored = read.min(writable);
            if stored > 0 {
                bytes.extend_from_slice(&buffer[..stored]);
            }
            Ok((false, stored, read))
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok((false, 0, 0))
        }
        Err(error) => Err(McpLifecycleError::Io(error.kind())),
    }
}

#[cfg(test)]
mod stderr_capture_tests {
    use super::*;

    #[test]
    fn stderr_capture_retains_only_its_byte_budget() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "head -c 1048576 /dev/zero >&2"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stderr fixture");
        let stderr = child.stderr.take().expect("stderr pipe");
        set_nonblocking(stderr.as_raw_fd()).expect("make stderr nonblocking");
        let mut capture = StderrCapture::start(stderr).expect("start stderr reader");

        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll stderr fixture") {
                break status;
            }
            if start.elapsed() >= Duration::from_secs(2) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("stderr reader did not drain the fixture");
            }
            thread::sleep(POLL_INTERVAL);
        };
        assert!(status.success());

        let retained = loop {
            let snapshot = capture.snapshot();
            if snapshot.len() == MAX_STDERR_BYTES || start.elapsed() >= Duration::from_secs(2) {
                break snapshot;
            }
            thread::sleep(POLL_INTERVAL);
        };
        assert_eq!(retained.len(), MAX_STDERR_BYTES);
        capture.stop();
    }
}
