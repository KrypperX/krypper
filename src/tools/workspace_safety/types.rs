//! Platform-neutral state types for the workspace containment boundary.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::syscalls::fstat_directory;

pub(super) const MAX_FILE_SIZE: usize = 1024 * 1024;
pub(super) const MAX_ROOT_AGENTS_BYTES: usize = 16 * 1024;
pub(super) const MAX_SYMLINKS: usize = 40;
pub(super) const MAX_TASK_DIRECTORY_ENTRIES: usize = 1024;

#[derive(Debug)]
pub(crate) enum CliError {
    Message(String),
    PlatformUnavailable(&'static str),
}

/// Stable, portable identity for a file-system object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FileIdentity {
    pub(crate) volume: u64,
    pub(crate) file: u64,
}

/// File type returned by the selected platform backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FileKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// Permission bits accepted by Unix backends and reserved for later backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CreateMode(u32);

impl CreateMode {
    pub(super) const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub(super) const fn bits(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileStatus {
    pub(super) identity: FileIdentity,
    pub(super) kind: FileKind,
    pub(super) mode: CreateMode,
    pub(super) size: u64,
    pub(super) modified_seconds: i64,
    pub(super) modified_nanos: i64,
    pub(super) changed_seconds: i64,
    pub(super) changed_nanos: i64,
}

/// Owned directory handle. Its platform representation stays behind `syscalls`.
#[derive(Debug)]
pub(crate) struct DirHandle(File);

#[derive(Debug)]
pub(crate) struct WorkspaceRoot {
    pub(super) descriptor: DirHandle,
    pub(super) path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootGuidanceReadError {
    InvalidTargetPath,
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
    pub(super) status: FileStatus,
    pub(super) contents: &'a [u8],
}

impl DirHandle {
    pub(super) fn from_file(file: File) -> Self {
        Self(file)
    }

    pub(super) fn file(&self) -> &File {
        &self.0
    }

    pub(super) fn into_file(self) -> File {
        self.0
    }
}

impl CliError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub(super) fn from_io(error: &io::Error, fallback: &'static str) -> Self {
        if error.kind() == io::ErrorKind::Unsupported {
            Self::PlatformUnavailable("workspace operations")
        } else {
            Self::new(fallback)
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Message(message) => message,
            Self::PlatformUnavailable("workspace operations") => {
                "workspace operations are unavailable on this platform"
            }
            Self::PlatformUnavailable("process sandbox") => {
                "process sandbox is unavailable on this platform"
            }
            Self::PlatformUnavailable(_) => "feature is unavailable on this platform",
        }
    }
}

impl fmt::Display for RootGuidanceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTargetPath => {
                formatter.write_str("edit target path is invalid or outside the task workspace")
            }
            Self::Symlink => formatter.write_str("AGENTS.md guidance or edit target is a symlink"),
            Self::NotRegularFile => formatter.write_str("AGENTS.md guidance is not a regular file"),
            Self::TooLarge => formatter.write_str("AGENTS.md guidance exceeds 16 KiB"),
            Self::InvalidUtf8 => formatter.write_str("AGENTS.md guidance is not valid UTF-8"),
            Self::ChangedDuringRead => {
                formatter.write_str("AGENTS.md guidance changed during the safe read")
            }
            Self::ReadFailed => formatter.write_str("AGENTS.md guidance could not be read"),
        }
    }
}

impl std::error::Error for RootGuidanceReadError {}

impl WorkspaceRoot {
    pub(crate) fn process_descriptor(&self) -> io::Result<File> {
        super::syscalls::duplicate_directory_file(&self.descriptor)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> io::Result<FileIdentity> {
        Ok(fstat_directory(&self.descriptor)?.identity)
    }

    pub(crate) fn duplicate(&self) -> io::Result<Self> {
        Ok(Self {
            descriptor: super::syscalls::duplicate_directory(&self.descriptor)?,
            path: self.path.clone(),
        })
    }
}
