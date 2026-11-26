//! Bounded status and acknowledgement protocol for the fixed in-sandbox runner.

use crate::runner_protocol::{
    decode_status, RunnerStatus, ACK_FD, ACK_FRAME, STATUS_FD, STATUS_FRAME_LEN,
};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
const POLL_INTERVAL_MILLIS: i32 = 100;

/// The parent owns one read/write endpoint per pipe. Child endpoints are duplicated to the
/// protocol's fixed descriptors in Bubblewrap's pre-exec phase, then closed in the parent.
pub(crate) struct RunnerHandshakePipes {
    status_read: File,
    status_write: Option<File>,
    ack_read: Option<File>,
    ack_write: Option<File>,
}

impl RunnerHandshakePipes {
    pub(crate) fn new() -> io::Result<Self> {
        let (status_read, status_write) = pipe_cloexec()?;
        let (ack_read, ack_write) = pipe_cloexec()?;
        Ok(Self {
            status_read,
            status_write: Some(status_write),
            ack_read: Some(ack_read),
            ack_write: Some(ack_write),
        })
    }

    pub(crate) fn status_writer(&self) -> io::Result<&File> {
        self.status_write
            .as_ref()
            .ok_or_else(|| io::Error::other("runner status writer was already closed"))
    }

    pub(crate) fn acknowledgement_reader(&self) -> io::Result<&File> {
        self.ack_read
            .as_ref()
            .ok_or_else(|| io::Error::other("runner acknowledgement reader was already closed"))
    }

    pub(crate) fn close_child_ends(&mut self) {
        self.status_write.take();
        self.ack_read.take();
    }

    /// Polls in short bounded intervals, checking cancellation and an overall deadline. The
    /// single status frame is read once into a fixed buffer, so a partial or oversized frame is
    /// rejected without waiting for more bytes.
    pub(crate) fn receive_status(
        &mut self,
        cancelled: &AtomicBool,
        timeout: Duration,
    ) -> io::Result<RunnerStatus> {
        let started = Instant::now();
        loop {
            if cancelled.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "runner handshake cancelled",
                ));
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "runner handshake status timed out",
                ));
            }
            let mut descriptor = libc::pollfd {
                fd: self.status_read.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            };
            // SAFETY: `descriptor` is a live, initialized pollfd; poll reads one entry and
            // writes only its `revents` field for this call.
            let ready = unsafe { libc::poll(&mut descriptor, 1, POLL_INTERVAL_MILLIS) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }

            let mut frame = [0u8; STATUS_FRAME_LEN + 1];
            let read = self.status_read.read(&mut frame)?;
            if read != STATUS_FRAME_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runner status frame has an invalid length",
                ));
            }
            return decode_status(&frame[..STATUS_FRAME_LEN]);
        }
    }

    pub(crate) fn acknowledge(&mut self) -> io::Result<()> {
        let mut writer = self
            .ack_write
            .take()
            .ok_or_else(|| io::Error::other("runner was already acknowledged"))?;
        writer.write_all(ACK_FRAME)?;
        writer.flush()
    }
}

fn pipe_cloexec() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: the array has space for the two descriptors required by pipe2 and remains live
    // for the call; on success the kernel initializes both entries.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` returned two fresh valid descriptors; each is transferred to exactly one
    // owning File and then removed from the local raw descriptor array.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: the second distinct descriptor returned by `pipe2` is still open and transfers to
    // this File as its sole owner.
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((read, write))
}
