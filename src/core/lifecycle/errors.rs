//! Workspace and task-session error taxonomy.
//!
//! Every variant here is produced while reading, listing, searching, creating or replacing
//! workspace files for a task turn. The enums only carry data and `Display`/`From` plumbing.

use crate::core::task::TurnState;
use crate::tools::RootGuidanceReadError;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// An error while requesting a workspace file for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceReadError {
    /// The task identifier is not present in the session.
    TaskNotFound { task_id: Uuid },
    /// The turn identifier is not present in the requested task.
    TurnNotFound { task_id: Uuid, turn_id: Uuid },
    /// The turn identifier belongs to a different task in this session.
    ForeignTurn {
        task_id: Uuid,
        turn_id: Uuid,
        owner_task_id: Uuid,
    },
    /// The requested turn exists but is not currently running.
    TurnNotRunning { turn_id: Uuid, state: TurnState },
    /// The workspace file could not be read under the existing file-safety policy.
    File { message: String },
}

impl fmt::Display for WorkspaceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskNotFound { task_id } => {
                write!(formatter, "task '{task_id}' is not in this session")
            }
            Self::TurnNotFound { task_id, turn_id } => {
                write!(formatter, "turn '{turn_id}' is not in task '{task_id}'")
            }
            Self::ForeignTurn {
                task_id,
                turn_id,
                owner_task_id,
            } => write!(
                formatter,
                "turn '{turn_id}' belongs to task '{owner_task_id}', not requested task '{task_id}'"
            ),
            Self::TurnNotRunning { turn_id, state } => write!(
                formatter,
                "turn '{turn_id}' is '{}', not running",
                state.as_str()
            ),
            Self::File { message } => write!(formatter, "workspace file read failed: {message}"),
        }
    }
}

impl std::error::Error for WorkspaceReadError {}

/// A bounded root project-guidance read could not be completed safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceGuidanceReadError {
    /// The task has no captured workspace root.
    WorkspaceUnavailable,
    /// Root `AGENTS.md` is a symbolic link.
    Symlink,
    /// Root `AGENTS.md` is not a regular file.
    NotRegularFile,
    /// Root `AGENTS.md` exceeds 16 KiB.
    TooLarge,
    /// Root `AGENTS.md` is not valid UTF-8.
    InvalidUtf8,
    /// Root `AGENTS.md` changed while it was being read.
    ChangedDuringRead,
    /// Root `AGENTS.md` could not be read.
    ReadFailed,
}

impl fmt::Display for WorkspaceGuidanceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceUnavailable => formatter.write_str("task workspace is unavailable"),
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

impl std::error::Error for WorkspaceGuidanceReadError {}

impl From<RootGuidanceReadError> for WorkspaceGuidanceReadError {
    fn from(error: RootGuidanceReadError) -> Self {
        match error {
            RootGuidanceReadError::Symlink => Self::Symlink,
            RootGuidanceReadError::NotRegularFile => Self::NotRegularFile,
            RootGuidanceReadError::TooLarge => Self::TooLarge,
            RootGuidanceReadError::InvalidUtf8 => Self::InvalidUtf8,
            RootGuidanceReadError::ChangedDuringRead => Self::ChangedDuringRead,
            RootGuidanceReadError::ReadFailed => Self::ReadFailed,
        }
    }
}

/// An error while listing a workspace directory for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceListError {
    /// The task identifier is not present in the session.
    TaskNotFound { task_id: Uuid },
    /// The turn identifier is not present in the requested task.
    TurnNotFound { task_id: Uuid, turn_id: Uuid },
    /// The turn identifier belongs to a different task in this session.
    ForeignTurn {
        task_id: Uuid,
        turn_id: Uuid,
        owner_task_id: Uuid,
    },
    /// The requested turn exists but is not currently running.
    TurnNotRunning { turn_id: Uuid, state: TurnState },
    /// The directory could not be listed under the existing file-safety policy.
    Directory { message: String },
}

impl fmt::Display for WorkspaceListError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskNotFound { task_id } => {
                write!(formatter, "task '{task_id}' is not in this session")
            }
            Self::TurnNotFound { task_id, turn_id } => {
                write!(formatter, "turn '{turn_id}' is not in task '{task_id}'")
            }
            Self::ForeignTurn {
                task_id,
                turn_id,
                owner_task_id,
            } => write!(
                formatter,
                "turn '{turn_id}' belongs to task '{owner_task_id}', not requested task '{task_id}'"
            ),
            Self::TurnNotRunning { turn_id, state } => write!(
                formatter,
                "turn '{turn_id}' is '{}', not running",
                state.as_str()
            ),
            Self::Directory { message } => {
                write!(formatter, "workspace directory listing failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceListError {}

impl From<WorkspaceReadError> for WorkspaceListError {
    fn from(error: WorkspaceReadError) -> Self {
        match error {
            WorkspaceReadError::TaskNotFound { task_id } => Self::TaskNotFound { task_id },
            WorkspaceReadError::TurnNotFound { task_id, turn_id } => {
                Self::TurnNotFound { task_id, turn_id }
            }
            WorkspaceReadError::ForeignTurn {
                task_id,
                turn_id,
                owner_task_id,
            } => Self::ForeignTurn {
                task_id,
                turn_id,
                owner_task_id,
            },
            WorkspaceReadError::TurnNotRunning { turn_id, state } => {
                Self::TurnNotRunning { turn_id, state }
            }
            WorkspaceReadError::File { message } => Self::Directory { message },
        }
    }
}

/// A matching file location returned by a bounded workspace text search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSearchLocation {
    /// Relative path to the matching regular file.
    pub path: PathBuf,
    /// One-based line number where the literal query begins.
    pub line: usize,
}

/// An error while searching a workspace directory for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceSearchError {
    /// The task turn is missing, foreign, or not running.
    TaskTurn(WorkspaceReadError),
    /// Search could not be completed under the workspace safety policy.
    Search { message: String },
}

impl fmt::Display for WorkspaceSearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskTurn(error) => error.fmt(formatter),
            Self::Search { message } => {
                write!(formatter, "workspace text search failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TaskTurn(error) => Some(error),
            Self::Search { .. } => None,
        }
    }
}

impl From<WorkspaceReadError> for WorkspaceSearchError {
    fn from(error: WorkspaceReadError) -> Self {
        Self::TaskTurn(error)
    }
}

/// An error while creating a workspace directory for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceCreateDirectoryError {
    /// The task or turn is missing, foreign, or not currently running.
    TaskTurn(WorkspaceReadError),
    /// The directory could not be created under the workspace safety policy.
    Directory { message: String },
}

impl fmt::Display for WorkspaceCreateDirectoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskTurn(error) => error.fmt(formatter),
            Self::Directory { message } => {
                write!(formatter, "workspace directory creation failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceCreateDirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TaskTurn(error) => Some(error),
            Self::Directory { .. } => None,
        }
    }
}

impl From<WorkspaceReadError> for WorkspaceCreateDirectoryError {
    fn from(error: WorkspaceReadError) -> Self {
        Self::TaskTurn(error)
    }
}

/// An error while creating a workspace file for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceWriteError {
    /// The task identifier is not present in the session.
    TaskNotFound { task_id: Uuid },
    /// The turn identifier is not present in the requested task.
    TurnNotFound { task_id: Uuid, turn_id: Uuid },
    /// The turn identifier belongs to a different task in this session.
    ForeignTurn {
        task_id: Uuid,
        turn_id: Uuid,
        owner_task_id: Uuid,
    },
    /// The requested turn exists but is not currently running.
    TurnNotRunning { turn_id: Uuid, state: TurnState },
    /// The workspace file could not be created under the existing file-safety policy.
    File { message: String },
}

impl fmt::Display for WorkspaceWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskNotFound { task_id } => {
                write!(formatter, "task '{task_id}' is not in this session")
            }
            Self::TurnNotFound { task_id, turn_id } => {
                write!(formatter, "turn '{turn_id}' is not in task '{task_id}'")
            }
            Self::ForeignTurn {
                task_id,
                turn_id,
                owner_task_id,
            } => write!(
                formatter,
                "turn '{turn_id}' belongs to task '{owner_task_id}', not requested task '{task_id}'"
            ),
            Self::TurnNotRunning { turn_id, state } => write!(
                formatter,
                "turn '{turn_id}' is '{}', not running",
                state.as_str()
            ),
            Self::File { message } => {
                write!(formatter, "workspace file creation failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceWriteError {}

/// An error while replacing text in a workspace file for a task turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceReplaceError {
    /// The requested task turn is missing, foreign, or not running.
    TaskTurn(WorkspaceReadError),
    /// The requested replacement could not be completed under the workspace policy.
    File { message: String },
}

impl fmt::Display for WorkspaceReplaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskTurn(error) => error.fmt(formatter),
            Self::File { message } => {
                write!(formatter, "workspace text replacement failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TaskTurn(error) => Some(error),
            Self::File { .. } => None,
        }
    }
}

impl From<WorkspaceReadError> for WorkspaceReplaceError {
    fn from(error: WorkspaceReadError) -> Self {
        Self::TaskTurn(error)
    }
}

/// A requested TaskSession workspace could not be captured safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSessionWorkspaceError(pub(super) String);

impl fmt::Display for TaskSessionWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unable to capture task session workspace: {}",
            self.0
        )
    }
}

impl std::error::Error for TaskSessionWorkspaceError {}
