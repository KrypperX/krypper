//! Task lifecycle: the task session, its snapshots and the workspace error taxonomy.
//!
//! `errors` owns the workspace error enums, `session` owns `TaskSession` and its snapshots,
//! and `checkpoint` owns the checkpoint codec. This file is a facade so every existing
//! `crate::core::lifecycle::*` path keeps working.

// `checkpoint` glob-imports this module, so the shared vocabulary it relies on stays here.
use crate::tools::{capture_workspace_root, capture_workspace_root_at, WorkspaceRoot};
use std::fmt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use uuid::Uuid;

mod checkpoint;
mod errors;
mod session;

pub use checkpoint::{TaskSessionCheckpointError, MAX_TASK_SESSION_CHECKPOINT_BYTES};
pub use errors::{
    TaskSessionWorkspaceError, WorkspaceCreateDirectoryError, WorkspaceGuidanceReadError,
    WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError, WorkspaceSearchError,
    WorkspaceSearchLocation, WorkspaceWriteError,
};
pub use session::{TaskSession, TaskSessionSnapshot, TaskSnapshot, TurnSnapshot};

pub use super::task::{
    Task, TaskActivityEvent, TaskEvent, TaskHistoryEvent, TaskHistoryTransition, TaskState,
    TransitionError, Turn, TurnActivityAction, TurnActivitySummary, TurnCompletionError,
    TurnCreationError, TurnEvent, TurnOutcome, TurnState, TurnTransitionError,
};
