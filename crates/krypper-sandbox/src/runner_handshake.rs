//! Bounded status and acknowledgement protocol for the fixed in-sandbox runner.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) const STATUS_FD: i32 = 204;
pub(crate) const ACK_FD: i32 = 205;
const STATUS_FRAME_LEN: usize = 20;
const STATUS_MAGIC: &[u8; 8] = b"KRYLLND1";
const ACK_FRAME: &[u8; 8] = b"KRYACK1\n";
const PROTOCOL_VERSION: u16 = 1;
const POLL_INTERVAL_MILLIS: i32 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunnerStatus {
    Applied { abi: i32 },
    Unavailable { abi: Option<i32> },
    Failed { errno: i32 },
}

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
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` returned two fresh valid descriptors; each is transferred to exactly one
    // owning File and then removed from the local raw descriptor array.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((read, write))
}

fn decode_status(frame: &[u8]) -> io::Result<RunnerStatus> {
    if frame.len() != STATUS_FRAME_LEN
        || &frame[..8] != STATUS_MAGIC
        || u16::from_be_bytes([frame[8], frame[9]]) != PROTOCOL_VERSION
        || frame[11] != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runner status frame is malformed or unsupported",
        ));
    }
    let abi_raw = u32::from_be_bytes(frame[12..16].try_into().expect("fixed ABI field"));
    let errno = i32::from_be_bytes(frame[16..20].try_into().expect("fixed errno field"));
    match frame[10] {
        1 if (3..=12).contains(&abi_raw) && errno == 0 => Ok(RunnerStatus::Applied {
            abi: abi_raw as i32,
        }),
        2 if errno == 0 => Ok(RunnerStatus::Unavailable {
            abi: i32::try_from(abi_raw).ok().filter(|abi| *abi != 0),
        }),
        3 if abi_raw == 0 && errno > 0 => Ok(RunnerStatus::Failed { errno }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runner status frame contains an invalid status",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_status, RunnerStatus, STATUS_FRAME_LEN, STATUS_MAGIC};

    fn frame(kind: u8, abi: u32, errno: i32) -> [u8; STATUS_FRAME_LEN] {
        let mut frame = [0; STATUS_FRAME_LEN];
        frame[..8].copy_from_slice(STATUS_MAGIC);
        frame[8..10].copy_from_slice(&1u16.to_be_bytes());
        frame[10] = kind;
        frame[12..16].copy_from_slice(&abi.to_be_bytes());
        frame[16..20].copy_from_slice(&errno.to_be_bytes());
        frame
    }

    #[test]
    fn status_frame_accepts_only_known_versioned_states() {
        assert_eq!(
            decode_status(&frame(1, 12, 0)).unwrap(),
            RunnerStatus::Applied { abi: 12 }
        );
        assert_eq!(
            decode_status(&frame(2, 0, 0)).unwrap(),
            RunnerStatus::Unavailable { abi: None }
        );
        assert_eq!(
            decode_status(&frame(3, 0, libc::EPERM)).unwrap(),
            RunnerStatus::Failed { errno: libc::EPERM }
        );
        assert!(decode_status(&frame(1, 2, 0)).is_err());
        assert!(decode_status(&frame(2, 0, libc::EPERM)).is_err());
        assert!(decode_status(&frame(9, 0, 0)).is_err());

        let mut unknown = frame(1, 12, 0);
        unknown[9] = 2;
        assert!(decode_status(&unknown).is_err());
    }
}
