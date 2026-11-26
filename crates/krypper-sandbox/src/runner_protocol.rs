//! Shared fixed status and acknowledgement protocol for the in-sandbox runner.

use std::io;

pub const STATUS_FD: i32 = 204;
pub const ACK_FD: i32 = 205;
pub const ACK_FRAME: &[u8; 8] = b"KRYACK1\n";
pub const STATUS_FRAME_LEN: usize = 20;
pub const MIN_LANDLOCK_ABI: i32 = 3;
pub const MAX_LANDLOCK_ABI: i32 = 12;
const STATUS_MAGIC: &[u8; 8] = b"KRYLLND1";
const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerStatus {
    Applied { abi: i32 },
    Unavailable { abi: Option<i32> },
    Failed { errno: i32 },
}

pub fn encode_status(status: RunnerStatus) -> [u8; STATUS_FRAME_LEN] {
    let (kind, abi, errno) = match status {
        RunnerStatus::Applied { abi } => (1, abi as u32, 0),
        RunnerStatus::Unavailable { abi } => (2, abi.unwrap_or(0) as u32, 0),
        RunnerStatus::Failed { errno } => (3, 0, errno),
    };
    let mut frame = [0u8; STATUS_FRAME_LEN];
    frame[..8].copy_from_slice(STATUS_MAGIC);
    frame[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    frame[10] = kind;
    frame[12..16].copy_from_slice(&abi.to_be_bytes());
    frame[16..20].copy_from_slice(&errno.to_be_bytes());
    frame
}

pub fn decode_status(frame: &[u8]) -> io::Result<RunnerStatus> {
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
        1 if (MIN_LANDLOCK_ABI as u32..=MAX_LANDLOCK_ABI as u32).contains(&abi_raw)
            && errno == 0 =>
        {
            Ok(RunnerStatus::Applied {
                abi: abi_raw as i32,
            })
        }
        2 if errno == 0 => Ok(RunnerStatus::Unavailable {
            abi: i32::try_from(abi_raw).ok().filter(|abi| *abi > 0),
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
    use super::{decode_status, encode_status, RunnerStatus};

    #[test]
    fn status_codec_accepts_only_known_versioned_states() {
        assert_eq!(
            decode_status(&encode_status(RunnerStatus::Applied { abi: 12 })).unwrap(),
            RunnerStatus::Applied { abi: 12 }
        );
        assert_eq!(
            decode_status(&encode_status(RunnerStatus::Unavailable { abi: None })).unwrap(),
            RunnerStatus::Unavailable { abi: None }
        );
        assert_eq!(
            decode_status(&encode_status(RunnerStatus::Failed { errno: libc::EPERM })).unwrap(),
            RunnerStatus::Failed { errno: libc::EPERM }
        );
        assert!(decode_status(&encode_status(RunnerStatus::Applied { abi: 2 })).is_err());
        assert!(decode_status(&encode_status(RunnerStatus::Failed { errno: 0 })).is_err());

        let mut unknown = encode_status(RunnerStatus::Applied { abi: 12 });
        unknown[9] = 2;
        assert!(decode_status(&unknown).is_err());
    }
}
