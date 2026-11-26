//! Bounded host and payload process supervision primitives.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct ManagedChild(Option<std::process::Child>);
impl ManagedChild {
    pub(super) fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    pub(super) fn child_mut(&mut self) -> io::Result<&mut std::process::Child> {
        self.0
            .as_mut()
            .ok_or_else(|| io::Error::other("host child process was already detached"))
    }
}
pub(super) fn reap(child: &mut std::process::Child) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            _ => {}
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = child.kill();
            if !reap(&mut child) {
                // Keep the wait handle until the kernel permits reaping; never claim cleanup.
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

pub(super) fn bounded_host(command: &mut Command) -> io::Result<String> {
    let mut child = ManagedChild::new(command.spawn()?);
    let mut stdout = child
        .child_mut()?
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing pipe"))?;
    nonblocking(&stdout)?;
    let mut stderr = child.child_mut()?.stderr.take();
    if let Some(pipe) = &stderr {
        nonblocking(pipe)?;
    }
    let mut errors = Vec::new();
    let start = Instant::now();
    let mut output = Vec::new();
    loop {
        let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
        drain(&mut stdout, &mut output, remaining)?;
        if let Some(pipe) = &mut stderr {
            let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
            drain(pipe, &mut errors, remaining)?;
        }
        if output.len() + errors.len() >= 16 * 1024 || start.elapsed() > Duration::from_secs(5) {
            let child = child.child_mut()?;
            let _ = child.kill();
            let _ = reap(child);
            return Err(io::Error::other("host command exceeded bound"));
        }
        if let Some(status) = child.child_mut()?.try_wait()? {
            let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
            drain(&mut stdout, &mut output, remaining)?;
            if !status.success() {
                if let Some(pipe) = &mut stderr {
                    let remaining = (16 * 1024usize).saturating_sub(output.len() + errors.len());
                    drain(pipe, &mut errors, remaining)?;
                }
                return Err(io::Error::other(format!(
                    "host command failed: {}",
                    String::from_utf8_lossy(&errors[..errors.len().min(512)])
                )));
            }
            return String::from_utf8(output).map_err(io::Error::other);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: `pipe` is borrowed for the whole function, so its descriptor stays open;
    // `F_GETFL`/`F_SETFL` take and return plain integers and touch no memory.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        // SAFETY: same descriptor and same reasoning as the `F_GETFL` call above.
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Return true only at EOF. Read at most one bounded chunk per stream per poll.
pub(super) fn drain(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    remaining: usize,
) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    let count = remaining.min(buffer.len());
    if count == 0 {
        return Ok(false);
    }
    match pipe.read(&mut buffer[..count]) {
        Ok(0) => Ok(true),
        Ok(n) => {
            bytes.extend_from_slice(&buffer[..n]);
            Ok(false)
        }
        Err(e)
            if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PostExitDrain {
    Complete,
    OutputLimit,
    Cancelled,
    DeadlineExpired,
}

pub(super) fn drain_after_child_exit(
    stdout_pipe: &mut impl Read,
    stderr_pipe: &mut impl Read,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    output_limit: usize,
    timeout: Duration,
    mut cancelled: impl FnMut() -> bool,
) -> io::Result<PostExitDrain> {
    let started = Instant::now();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        stdout_eof |= drain(stdout_pipe, stdout, remaining)?;
        let remaining = output_limit.saturating_sub(stdout.len() + stderr.len());
        stderr_eof |= drain(stderr_pipe, stderr, remaining)?;
        if stdout.len() + stderr.len() >= output_limit {
            return Ok(PostExitDrain::OutputLimit);
        }
        if cancelled() {
            return Ok(PostExitDrain::Cancelled);
        }
        if stdout_eof && stderr_eof {
            return Ok(PostExitDrain::Complete);
        }
        if started.elapsed() >= timeout {
            return Ok(PostExitDrain::DeadlineExpired);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod post_exit_drain_tests {
    use super::{drain_after_child_exit, nonblocking, PostExitDrain};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct ProcessGroup(libc::pid_t);

    impl Drop for ProcessGroup {
        fn drop(&mut self) {
            // SAFETY: this test created the process group with the child PID as its ID.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }

    #[test]
    fn inherited_pipe_writer_does_not_extend_the_post_exit_deadline() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "(sleep 30) & printf 'leader output\\n'; exit 0"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let _process_group = ProcessGroup(child.id() as libc::pid_t);
        let mut stdout_pipe = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();
        nonblocking(&stdout_pipe).unwrap();
        nonblocking(&stderr_pipe).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(0));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let started = Instant::now();
        let drain = drain_after_child_exit(
            &mut stdout_pipe,
            &mut stderr_pipe,
            &mut stdout,
            &mut stderr,
            1024,
            Duration::from_millis(100),
            || false,
        )
        .unwrap();

        assert_eq!(drain, PostExitDrain::DeadlineExpired);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(stdout, b"leader output\n");

        // SAFETY: this test created the isolated group and its sleeping descendant.
        let killed = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        assert_eq!(killed, 0);
        assert_eq!(
            drain_after_child_exit(
                &mut stdout_pipe,
                &mut stderr_pipe,
                &mut stdout,
                &mut stderr,
                1024,
                Duration::from_secs(1),
                || false,
            )
            .unwrap(),
            PostExitDrain::Complete
        );
    }
}
