//! Shared state types and errors for the workspace containment boundary.

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use super::syscalls::fstat;

pub(super) const MAX_FILE_SIZE: usize = 1024 * 1024;
pub(super) const MAX_ROOT_AGENTS_BYTES: usize = 16 * 1024;
pub(super) const MAX_SYMLINKS: usize = 40;
pub(super) const MAX_TASK_DIRECTORY_ENTRIES: usize = 1024;

#[derive(Debug)]
pub(crate) struct CliError(String);

#[derive(Debug)]
pub(crate) struct WorkspaceRoot {
    pub(super) descriptor: OwnedFd,
    pub(super) path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootGuidanceReadError {
    Symlink,
    NotRegularFile,
    TooLarge,
    InvalidUtf8,
    ChangedDuringRead,
    ReadFailed,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PatchEvidence {
    pub(crate) bytes_written: usize,
    pub(crate) before_diff: String,
    pub(crate) after_diff: String,
}

pub(super) struct OriginalFileState<'a> {
    pub(super) status: &'a libc::stat,
    pub(super) contents: &'a [u8],
}

impl CliError {
    pub(super) fn new(message: &'static str) -> Self {
        Self(message.to_owned())
    }

    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RootGuidanceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => formatter.write_str("root AGENTS.md is a symlink"),
            Self::NotRegularFile => formatter.write_str("root AGENTS.md is not a regular file"),
            Self::TooLarge => formatter.write_str("root AGENTS.md exceeds 16 KiB"),
            Self::InvalidUtf8 => formatter.write_str("root AGENTS.md is not valid UTF-8"),
            Self::ChangedDuringRead => {
                formatter.write_str("root AGENTS.md changed during the safe read")
            }
            Self::ReadFailed => formatter.write_str("root AGENTS.md could not be read"),
        }
    }
}

impl std::error::Error for RootGuidanceReadError {}

impl WorkspaceRoot {
    pub(crate) fn process_descriptor(&self) -> io::Result<OwnedFd> {
        self.descriptor.try_clone()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let metadata = fstat(self.descriptor.as_raw_fd())?;
        Ok((metadata.st_dev as u64, metadata.st_ino as u64))
    }

    pub(crate) fn duplicate(&self) -> io::Result<Self> {
        Ok(Self {
            descriptor: self.descriptor.try_clone()?,
            path: self.path.clone(),
        })
    }
}
