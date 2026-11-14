//! Compatibility facade for the relocated Core lifecycle implementation.

pub use crate::core::lifecycle::{
    Task, TaskActivityEvent, TaskEvent, TaskHistoryEvent, TaskHistoryTransition, TaskSession,
    TaskSessionCheckpointError, TaskSessionSnapshot, TaskSessionWorkspaceError, TaskSnapshot,
    TaskState, TransitionError, Turn, TurnActivityAction, TurnActivitySummary, TurnCompletionError,
    TurnCreationError, TurnEvent, TurnOutcome, TurnSnapshot, TurnState, TurnTransitionError,
    WorkspaceCreateDirectoryError, WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError,
    WorkspaceSearchError, WorkspaceSearchLocation, WorkspaceWriteError,
    MAX_TASK_SESSION_CHECKPOINT_BYTES,
};
