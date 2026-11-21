//! Bounded stdio launch, nonblocking I/O and process cleanup for MCP clients.

use super::config::McpStdioConfig;
use super::lifecycle::{McpCancellation, McpLifecycleError};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

pub(super) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct StdioChild {
    pub child: ManagedChild,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub pending: Vec<u8>,
    pub stderr_used: usize,
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
    pub(super) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self.0.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    pub(super) fn terminate(&mut self) -> Result<(), McpLifecycleError> {
        let Some(mut child) = self.0.take() else {
            return Ok(());
        };
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.kill();
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        if reap(&mut child) {
            Ok(())
        } else {
            thread::spawn(move || {
                let _ = child.wait();
            });
            Err(McpLifecycleError::CleanupFailed)
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
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

pub(super) fn spawn(
    config: &McpStdioConfig,
    deadline: Instant,
) -> Result<StdioChild, McpLifecycleError> {
    let config = config.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = spawn_inner(&config);
        let _ = sender.send(result);
        let _ = done_sender.send(());
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            done_receiver
                .recv_timeout(REAP_TIMEOUT)
                .map_err(|_| McpLifecycleError::CleanupFailed)
                .and_then(|_| match receiver.try_recv() {
                    Ok(Ok(mut stdio)) => stdio.child.terminate(),
                    Ok(Err(_)) => Ok(()),
                    Err(_) => Err(McpLifecycleError::CleanupFailed),
                })
                .map_err(|_| McpLifecycleError::CleanupFailed)?;
            Err(McpLifecycleError::Timeout)
        }
        Err(RecvTimeoutError::Disconnected) => Err(McpLifecycleError::SpawnFailed),
    }
}

fn spawn_inner(config: &McpStdioConfig) -> Result<StdioChild, McpLifecycleError> {
    #[cfg(test)]
    if config
        .args
        .iter()
        .any(|arg| arg.contains("krypper-spawn-delay"))
    {
        thread::sleep(Duration::from_millis(50));
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
    Ok(StdioChild {
        child,
        stdin,
        stdout,
        stderr,
        pending: Vec::new(),
        stderr_used: 0,
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
    stderr: &mut ChildStderr,
    stderr_used: &mut usize,
    pending: &mut Vec<u8>,
    deadline: Instant,
    cancel: &McpCancellation,
) -> Result<Vec<u8>, McpLifecycleError> {
    let mut response = std::mem::take(pending);
    let mut errors = Vec::new();
    loop {
        check(deadline, cancel)?;
        let before = response.len();
        let (stdout_eof, _, _) = drain(stdout, &mut response, MAX_RESPONSE_BYTES)?;
        let progressed = response.len() > before;
        let stderr_budget = MAX_STDERR_BYTES.saturating_sub(*stderr_used);
        let (_, stored, read) = drain(stderr, &mut errors, stderr_budget)?;
        *stderr_used += stored;
        if read > stderr_budget {
            return Err(McpLifecycleError::OutputLimitExceeded);
        }
        if let Some(position) = response.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = response.drain(..=position).collect();
            *pending = response;
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            return Ok(line);
        }
        if response.len() >= MAX_RESPONSE_BYTES {
            return Err(McpLifecycleError::OutputLimitExceeded);
        }
        if stdout_eof && !progressed && matches!(child.try_wait(), Ok(Some(_))) {
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
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
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
