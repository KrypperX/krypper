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
