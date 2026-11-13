use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

/// Maximum encoded size accepted for a TaskSession checkpoint.
pub const MAX_TASK_SESSION_CHECKPOINT_BYTES: usize = 1024 * 1024;

const TASK_SESSION_CHECKPOINT_VERSION: u32 = 1;

/// A checkpoint could not be exported or did not describe a valid restorable session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskSessionCheckpointError {
    /// Checkpoints are only safe when no task-owned turn is currently running.
    RunningTurn { task_id: Uuid, turn_id: Uuid },
    /// A workspace root could not be resolved or duplicated.
    WorkspaceUnavailable { task_id: Option<Uuid> },
    /// A task workspace does not match the checkpoint's workspace binding.
    WorkspaceMismatch { task_id: Uuid },
    /// The checkpoint exceeds [`MAX_TASK_SESSION_CHECKPOINT_BYTES`].
    TooLarge,
    /// The checkpoint uses an unsupported wire format version.
    UnsupportedVersion { version: u32 },
    /// The checkpoint is malformed, has duplicate identifiers, or has inconsistent history.
    InvalidCheckpoint,
}

impl fmt::Display for TaskSessionCheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunningTurn { task_id, turn_id } => write!(
                formatter,
                "cannot checkpoint task '{task_id}' while turn '{turn_id}' is running"
            ),
            Self::WorkspaceUnavailable {
                task_id: Some(task_id),
            } => write!(
                formatter,
                "cannot checkpoint task '{task_id}' because its workspace is unavailable"
            ),
            Self::WorkspaceUnavailable { task_id: None } => {
                formatter.write_str("cannot resolve the current workspace for checkpoint restore")
            }
            Self::WorkspaceMismatch { task_id } => write!(
                formatter,
                "workspace binding for task '{task_id}' does not match the checkpoint"
            ),
            Self::TooLarge => write!(
                formatter,
                "TaskSession checkpoint exceeds {MAX_TASK_SESSION_CHECKPOINT_BYTES} bytes"
            ),
            Self::UnsupportedVersion { version } => {
                write!(
                    formatter,
                    "unsupported TaskSession checkpoint version {version}"
                )
            }
            Self::InvalidCheckpoint => formatter.write_str("invalid TaskSession checkpoint"),
        }
    }
}

impl std::error::Error for TaskSessionCheckpointError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointSession {
    version: u32,
    session_id: String,
    tasks: Vec<CheckpointTask>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTask {
    id: String,
    description: String,
    workspace_path: Vec<u8>,
    workspace_device: u64,
    workspace_inode: u64,
    state: CheckpointTaskState,
    events: Vec<CheckpointTaskEvent>,
    history: Vec<CheckpointTaskHistoryEvent>,
    activity_history: Vec<CheckpointTaskActivityEvent>,
    turns: Vec<CheckpointTurn>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointTaskState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<TaskState> for CheckpointTaskState {
    fn from(state: TaskState) -> Self {
        match state {
            TaskState::Pending => Self::Pending,
            TaskState::Running => Self::Running,
            TaskState::Succeeded => Self::Succeeded,
            TaskState::Failed => Self::Failed,
            TaskState::Cancelled => Self::Cancelled,
        }
    }
}

impl From<CheckpointTaskState> for TaskState {
    fn from(state: CheckpointTaskState) -> Self {
        match state {
            CheckpointTaskState::Pending => Self::Pending,
            CheckpointTaskState::Running => Self::Running,
            CheckpointTaskState::Succeeded => Self::Succeeded,
            CheckpointTaskState::Failed => Self::Failed,
            CheckpointTaskState::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTaskEvent {
    task_id: String,
    sequence: u64,
    previous_state: CheckpointTaskState,
    new_state: CheckpointTaskState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTaskHistoryEvent {
    task_id: String,
    sequence: u64,
    transition: CheckpointTaskHistoryTransition,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CheckpointTaskHistoryTransition {
    Task {
        previous_state: CheckpointTaskState,
        new_state: CheckpointTaskState,
    },
    TurnCreated {
        turn_id: String,
        initial_state: CheckpointTurnState,
    },
    Turn {
        turn_id: String,
        previous_state: CheckpointTurnState,
        new_state: CheckpointTurnState,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTaskActivityEvent {
    task_id: String,
    turn_id: String,
    sequence: u64,
    summary: CheckpointActivitySummary,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTurn {
    id: String,
    task_id: String,
    state: CheckpointTurnState,
    outcome: Option<CheckpointTurnOutcome>,
    events: Vec<CheckpointTurnEvent>,
    activity_history: Vec<CheckpointActivitySummary>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointTurnState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<TurnState> for CheckpointTurnState {
    fn from(state: TurnState) -> Self {
        match state {
            TurnState::Pending => Self::Pending,
            TurnState::Running => Self::Running,
            TurnState::Succeeded => Self::Succeeded,
            TurnState::Failed => Self::Failed,
            TurnState::Cancelled => Self::Cancelled,
        }
    }
}

impl From<CheckpointTurnState> for TurnState {
    fn from(state: CheckpointTurnState) -> Self {
        match state {
            CheckpointTurnState::Pending => Self::Pending,
            CheckpointTurnState::Running => Self::Running,
            CheckpointTurnState::Succeeded => Self::Succeeded,
            CheckpointTurnState::Failed => Self::Failed,
            CheckpointTurnState::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointTurnEvent {
    task_id: String,
    turn_id: String,
    sequence: u64,
    previous_state: CheckpointTurnState,
    new_state: CheckpointTurnState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CheckpointTurnOutcome {
    Success { result: String },
    Failure { detail: String },
}

impl From<&TurnOutcome> for CheckpointTurnOutcome {
    fn from(outcome: &TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Success { result } => Self::Success {
                result: result.clone(),
            },
            TurnOutcome::Failure { detail } => Self::Failure {
                detail: detail.clone(),
            },
        }
    }
}

impl From<CheckpointTurnOutcome> for TurnOutcome {
    fn from(outcome: CheckpointTurnOutcome) -> Self {
        match outcome {
            CheckpointTurnOutcome::Success { result } => Self::Success { result },
            CheckpointTurnOutcome::Failure { detail } => Self::Failure { detail },
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointActivityAction {
    ReadFile,
    ReadRange,
    ListDirectory,
    CreateDirectory,
    CreateFile,
    ReplaceText,
    SearchText,
    SearchTreeText,
    Unsupported,
}

impl From<TurnActivityAction> for CheckpointActivityAction {
    fn from(action: TurnActivityAction) -> Self {
        match action {
            TurnActivityAction::ReadFile => Self::ReadFile,
            TurnActivityAction::ReadRange => Self::ReadRange,
            TurnActivityAction::ListDirectory => Self::ListDirectory,
            TurnActivityAction::CreateDirectory => Self::CreateDirectory,
            TurnActivityAction::CreateFile => Self::CreateFile,
            TurnActivityAction::ReplaceText => Self::ReplaceText,
            TurnActivityAction::SearchText => Self::SearchText,
            TurnActivityAction::SearchTreeText => Self::SearchTreeText,
            TurnActivityAction::Unsupported => Self::Unsupported,
        }
    }
}

impl From<CheckpointActivityAction> for TurnActivityAction {
    fn from(action: CheckpointActivityAction) -> Self {
        match action {
            CheckpointActivityAction::ReadFile => Self::ReadFile,
            CheckpointActivityAction::ReadRange => Self::ReadRange,
            CheckpointActivityAction::ListDirectory => Self::ListDirectory,
            CheckpointActivityAction::CreateDirectory => Self::CreateDirectory,
            CheckpointActivityAction::CreateFile => Self::CreateFile,
            CheckpointActivityAction::ReplaceText => Self::ReplaceText,
            CheckpointActivityAction::SearchText => Self::SearchText,
            CheckpointActivityAction::SearchTreeText => Self::SearchTreeText,
            CheckpointActivityAction::Unsupported => Self::Unsupported,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CheckpointActivitySummary {
    FileRead {
        bytes_read: usize,
    },
    FileRangeRead {
        start_line: usize,
        end_line: usize,
        bytes_returned: usize,
    },
    DirectoryCreated,
    FileCreated {
        bytes_written: usize,
    },
    TextReplaced {
        bytes_written: usize,
    },
    DirectoryListed {
        entry_count: usize,
    },
    TextSearched {
        location_count: usize,
    },
    TreeTextSearched {
        location_count: usize,
    },
    Completed,
    Continued,
    BudgetExhausted {
        actions: usize,
    },
    Cancelled,
    ActionFailed {
        action: CheckpointActivityAction,
    },
}

impl From<TurnActivitySummary> for CheckpointActivitySummary {
    fn from(summary: TurnActivitySummary) -> Self {
        match summary {
            TurnActivitySummary::FileRead { bytes_read } => Self::FileRead { bytes_read },
            TurnActivitySummary::FileRangeRead {
                start_line,
                end_line,
                bytes_returned,
            } => Self::FileRangeRead {
                start_line,
                end_line,
                bytes_returned,
            },
            TurnActivitySummary::DirectoryCreated => Self::DirectoryCreated,
            TurnActivitySummary::FileCreated { bytes_written } => {
                Self::FileCreated { bytes_written }
            }
            TurnActivitySummary::TextReplaced { bytes_written } => {
                Self::TextReplaced { bytes_written }
            }
            TurnActivitySummary::DirectoryListed { entry_count } => {
                Self::DirectoryListed { entry_count }
            }
            TurnActivitySummary::TextSearched { location_count } => {
                Self::TextSearched { location_count }
            }
            TurnActivitySummary::TreeTextSearched { location_count } => {
                Self::TreeTextSearched { location_count }
            }
            TurnActivitySummary::Completed => Self::Completed,
            TurnActivitySummary::Continued => Self::Continued,
            TurnActivitySummary::BudgetExhausted { actions } => Self::BudgetExhausted { actions },
            TurnActivitySummary::Cancelled => Self::Cancelled,
            TurnActivitySummary::ActionFailed { action } => Self::ActionFailed {
                action: action.into(),
            },
        }
    }
}

impl From<CheckpointActivitySummary> for TurnActivitySummary {
    fn from(summary: CheckpointActivitySummary) -> Self {
        match summary {
            CheckpointActivitySummary::FileRead { bytes_read } => Self::FileRead { bytes_read },
            CheckpointActivitySummary::FileRangeRead {
                start_line,
                end_line,
                bytes_returned,
            } => Self::FileRangeRead {
                start_line,
                end_line,
                bytes_returned,
            },
            CheckpointActivitySummary::DirectoryCreated => Self::DirectoryCreated,
            CheckpointActivitySummary::FileCreated { bytes_written } => {
                Self::FileCreated { bytes_written }
            }
            CheckpointActivitySummary::TextReplaced { bytes_written } => {
                Self::TextReplaced { bytes_written }
            }
            CheckpointActivitySummary::DirectoryListed { entry_count } => {
                Self::DirectoryListed { entry_count }
            }
            CheckpointActivitySummary::TextSearched { location_count } => {
                Self::TextSearched { location_count }
            }
            CheckpointActivitySummary::TreeTextSearched { location_count } => {
                Self::TreeTextSearched { location_count }
            }
            CheckpointActivitySummary::Completed => Self::Completed,
            CheckpointActivitySummary::Continued => Self::Continued,
            CheckpointActivitySummary::BudgetExhausted { actions } => {
                Self::BudgetExhausted { actions }
            }
            CheckpointActivitySummary::Cancelled => Self::Cancelled,
            CheckpointActivitySummary::ActionFailed { action } => Self::ActionFailed {
                action: action.into(),
            },
        }
    }
}

impl TaskSession {
    /// Exports a versioned checkpoint at a safe boundary.
    ///
    /// The returned JSON bytes contain task and turn lifecycle data, outcomes, workspace
    /// bindings, and metadata-only activity summaries. They do not include workspace action
    /// inputs or observations. The caller decides whether and where to store the bytes. Export
    /// fails while any turn is running, when tasks are bound to different workspaces, or when the
    /// encoded checkpoint exceeds 1 MiB. Restore the bytes from the same current directory, or
    /// use [`Self::restore_checkpoint_in_workspace`] to supply the captured workspace explicitly.
    pub fn export_checkpoint(&self) -> Result<Vec<u8>, TaskSessionCheckpointError> {
        if checkpoint_text_size(self).is_none_or(|size| size > MAX_TASK_SESSION_CHECKPOINT_BYTES) {
            return Err(TaskSessionCheckpointError::TooLarge);
        }
        let mut tasks = Vec::with_capacity(self.tasks.len());
        let mut workspace_path: Option<Vec<u8>> = None;
        let mut workspace_identity: Option<(u64, u64)> = None;
        for task in &self.tasks {
            let workspace_root = task.workspace_root.as_ref().map_err(|_| {
                TaskSessionCheckpointError::WorkspaceUnavailable {
                    task_id: Some(task.id),
                }
            })?;
            let task_workspace_identity = workspace_root.identity().map_err(|_| {
                TaskSessionCheckpointError::WorkspaceUnavailable {
                    task_id: Some(task.id),
                }
            })?;
            let task_workspace_path = workspace_root.path().as_os_str().as_bytes();
            if workspace_path
                .as_deref()
                .is_some_and(|path| path != task_workspace_path)
                || workspace_identity.is_some_and(|identity| identity != task_workspace_identity)
            {
                return Err(TaskSessionCheckpointError::WorkspaceMismatch { task_id: task.id });
            }
            workspace_path.get_or_insert_with(|| task_workspace_path.to_vec());
            workspace_identity.get_or_insert(task_workspace_identity);
            tasks.push(checkpoint_task(task, task_workspace_identity)?);
        }
        let checkpoint = CheckpointSession {
            version: TASK_SESSION_CHECKPOINT_VERSION,
            session_id: self.id.to_string(),
            tasks,
        };
        let bytes = serde_json::to_vec(&checkpoint)
            .map_err(|_| TaskSessionCheckpointError::InvalidCheckpoint)?;
        if bytes.len() > MAX_TASK_SESSION_CHECKPOINT_BYTES {
            return Err(TaskSessionCheckpointError::TooLarge);
        }
        Ok(bytes)
    }

    /// Restores a checkpoint only when every task remains bound to the current directory.
    ///
    /// The version, encoded size, identifiers, workspace bindings, lifecycle histories, outcomes,
    /// and metadata-only activity are validated before the session is returned. Restore only
    /// rebuilds in-memory state; it never repeats a workspace action. A running task with no
    /// running turn can be restored and continued with a newly created turn.
    pub fn restore_checkpoint(bytes: &[u8]) -> Result<Self, TaskSessionCheckpointError> {
        Self::restore_checkpoint_for_workspace(bytes, None)
    }

    /// Restores a checkpoint against an explicitly supplied workspace path.
    ///
    /// The supplied path must resolve to the checkpoint's recorded canonical path and device/inode
    /// identity. This method does not change the process current directory. It rebuilds in-memory
    /// state only and never repeats a workspace action.
    pub fn restore_checkpoint_in_workspace(
        bytes: &[u8],
        workspace_path: impl AsRef<Path>,
    ) -> Result<Self, TaskSessionCheckpointError> {
        Self::restore_checkpoint_for_workspace(bytes, Some(workspace_path.as_ref()))
    }

    fn restore_checkpoint_for_workspace(
        bytes: &[u8],
        workspace_path: Option<&Path>,
    ) -> Result<Self, TaskSessionCheckpointError> {
        if bytes.len() > MAX_TASK_SESSION_CHECKPOINT_BYTES {
            return Err(TaskSessionCheckpointError::TooLarge);
        }
        let checkpoint: CheckpointSession = serde_json::from_slice(bytes)
            .map_err(|_| TaskSessionCheckpointError::InvalidCheckpoint)?;
        if checkpoint.version != TASK_SESSION_CHECKPOINT_VERSION {
            return Err(TaskSessionCheckpointError::UnsupportedVersion {
                version: checkpoint.version,
            });
        }

        let mut identifiers = HashSet::with_capacity(checkpoint.tasks.len() + 1);
        let session_id = claim_checkpoint_id(&checkpoint.session_id, &mut identifiers)?;
        let current_workspace =
            match workspace_path {
                Some(workspace_path) => {
                    Some(capture_workspace_root_at(workspace_path).map_err(|_| {
                        TaskSessionCheckpointError::WorkspaceUnavailable { task_id: None }
                    })?)
                }
                None if checkpoint.tasks.is_empty() => None,
                None => Some(capture_workspace_root().map_err(|_| {
                    TaskSessionCheckpointError::WorkspaceUnavailable { task_id: None }
                })?),
            };
        let mut tasks = Vec::with_capacity(checkpoint.tasks.len());
        for checkpoint_task in checkpoint.tasks {
            let task_id = claim_checkpoint_id(&checkpoint_task.id, &mut identifiers)?;
            let Some(current_workspace) = current_workspace.as_ref() else {
                return Err(TaskSessionCheckpointError::WorkspaceUnavailable {
                    task_id: Some(task_id),
                });
            };
            if checkpoint_task.workspace_path != current_workspace.path().as_os_str().as_bytes() {
                return Err(TaskSessionCheckpointError::WorkspaceMismatch { task_id });
            }
            let (workspace_device, workspace_inode) =
                current_workspace.identity().map_err(|_| {
                    TaskSessionCheckpointError::WorkspaceUnavailable {
                        task_id: Some(task_id),
                    }
                })?;
            if checkpoint_task.workspace_device != workspace_device
                || checkpoint_task.workspace_inode != workspace_inode
            {
                return Err(TaskSessionCheckpointError::WorkspaceMismatch { task_id });
            }
            let workspace_root = current_workspace.duplicate().map_err(|_| {
                TaskSessionCheckpointError::WorkspaceUnavailable {
                    task_id: Some(task_id),
                }
            })?;
            let task = checkpoint_task.restore(task_id, workspace_root, &mut identifiers)?;
            validate_restored_task(&task)?;
            tasks.push(task);
        }

        Ok(Self {
            id: session_id,
            workspace_root: None,
            tasks,
        })
    }
}

fn checkpoint_task(
    task: &Task,
    workspace_identity: (u64, u64),
) -> Result<CheckpointTask, TaskSessionCheckpointError> {
    validate_restored_task(task)?;
    let workspace_root = task.workspace_root.as_ref().map_err(|_| {
        TaskSessionCheckpointError::WorkspaceUnavailable {
            task_id: Some(task.id),
        }
    })?;
    let (workspace_device, workspace_inode) = workspace_identity;

    Ok(CheckpointTask {
        id: task.id.to_string(),
        description: task.description.clone(),
        workspace_path: workspace_root.path().as_os_str().as_bytes().to_vec(),
        workspace_device,
        workspace_inode,
        state: task.state.into(),
        events: task
            .events
            .iter()
            .map(|event| CheckpointTaskEvent {
                task_id: event.task_id.to_string(),
                sequence: event.sequence,
                previous_state: event.previous_state.into(),
                new_state: event.new_state.into(),
            })
            .collect(),
        history: task
            .history
            .iter()
            .map(|event| CheckpointTaskHistoryEvent {
                task_id: event.task_id.to_string(),
                sequence: event.sequence,
                transition: event.transition.into(),
            })
            .collect(),
        activity_history: task
            .activity_history
            .iter()
            .map(|event| CheckpointTaskActivityEvent {
                task_id: event.task_id.to_string(),
                turn_id: event.turn_id.to_string(),
                sequence: event.sequence,
                summary: event.summary.into(),
            })
            .collect(),
        turns: task
            .turns
            .iter()
            .map(|turn| CheckpointTurn {
                id: turn.id.to_string(),
                task_id: turn.task_id.to_string(),
                state: turn.state.into(),
                outcome: turn.outcome.as_ref().map(CheckpointTurnOutcome::from),
                events: turn
                    .events
                    .iter()
                    .map(|event| CheckpointTurnEvent {
                        task_id: event.task_id.to_string(),
                        turn_id: event.turn_id.to_string(),
                        sequence: event.sequence,
                        previous_state: event.previous_state.into(),
                        new_state: event.new_state.into(),
                    })
                    .collect(),
                activity_history: turn
                    .activity_history
                    .iter()
                    .copied()
                    .map(CheckpointActivitySummary::from)
                    .collect(),
            })
            .collect(),
    })
}

impl From<TaskHistoryTransition> for CheckpointTaskHistoryTransition {
    fn from(transition: TaskHistoryTransition) -> Self {
        match transition {
            TaskHistoryTransition::Task {
                previous_state,
                new_state,
            } => Self::Task {
                previous_state: previous_state.into(),
                new_state: new_state.into(),
            },
            TaskHistoryTransition::TurnCreated {
                turn_id,
                initial_state,
            } => Self::TurnCreated {
                turn_id: turn_id.to_string(),
                initial_state: initial_state.into(),
            },
            TaskHistoryTransition::Turn {
                turn_id,
                previous_state,
                new_state,
            } => Self::Turn {
                turn_id: turn_id.to_string(),
                previous_state: previous_state.into(),
                new_state: new_state.into(),
            },
        }
    }
}

impl CheckpointTask {
    fn restore(
        self,
        task_id: Uuid,
        workspace_root: WorkspaceRoot,
        identifiers: &mut HashSet<Uuid>,
    ) -> Result<Task, TaskSessionCheckpointError> {
        let events = self
            .events
            .into_iter()
            .map(|event| {
                Ok(TaskEvent {
                    task_id: parse_checkpoint_id(&event.task_id)?,
                    sequence: event.sequence,
                    previous_state: event.previous_state.into(),
                    new_state: event.new_state.into(),
                })
            })
            .collect::<Result<Vec<_>, TaskSessionCheckpointError>>()?;
        let history = self
            .history
            .into_iter()
            .map(|event| {
                Ok(TaskHistoryEvent {
                    task_id: parse_checkpoint_id(&event.task_id)?,
                    sequence: event.sequence,
                    transition: restore_history_transition(event.transition)?,
                })
            })
            .collect::<Result<Vec<_>, TaskSessionCheckpointError>>()?;
        let activity_history = self
            .activity_history
            .into_iter()
            .map(|event| {
                Ok(TaskActivityEvent {
                    task_id: parse_checkpoint_id(&event.task_id)?,
                    turn_id: parse_checkpoint_id(&event.turn_id)?,
                    sequence: event.sequence,
                    summary: event.summary.into(),
                })
            })
            .collect::<Result<Vec<_>, TaskSessionCheckpointError>>()?;
        let turns = self
            .turns
            .into_iter()
            .map(|turn| turn.restore(task_id, identifiers))
            .collect::<Result<Vec<_>, TaskSessionCheckpointError>>()?;

        Ok(Task {
            id: task_id,
            description: self.description,
            workspace_root: Ok(Arc::new(workspace_root)),
            state: self.state.into(),
            events,
            history,
            activity_history,
            turns,
        })
    }
}

impl CheckpointTurn {
    fn restore(
        self,
        expected_task_id: Uuid,
        identifiers: &mut HashSet<Uuid>,
    ) -> Result<Turn, TaskSessionCheckpointError> {
        let id = claim_checkpoint_id(&self.id, identifiers)?;
        let task_id = parse_checkpoint_id(&self.task_id)?;
        if task_id != expected_task_id {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
        let events = self
            .events
            .into_iter()
            .map(|event| {
                Ok(TurnEvent {
                    task_id: parse_checkpoint_id(&event.task_id)?,
                    turn_id: parse_checkpoint_id(&event.turn_id)?,
                    sequence: event.sequence,
                    previous_state: event.previous_state.into(),
                    new_state: event.new_state.into(),
                })
            })
            .collect::<Result<Vec<_>, TaskSessionCheckpointError>>()?;

        Ok(Turn {
            id,
            task_id,
            state: self.state.into(),
            outcome: self.outcome.map(TurnOutcome::from),
            events,
            activity_history: self
                .activity_history
                .into_iter()
                .map(TurnActivitySummary::from)
                .collect(),
        })
    }
}

fn restore_history_transition(
    transition: CheckpointTaskHistoryTransition,
) -> Result<TaskHistoryTransition, TaskSessionCheckpointError> {
    match transition {
        CheckpointTaskHistoryTransition::Task {
            previous_state,
            new_state,
        } => Ok(TaskHistoryTransition::Task {
            previous_state: previous_state.into(),
            new_state: new_state.into(),
        }),
        CheckpointTaskHistoryTransition::TurnCreated {
            turn_id,
            initial_state,
        } => Ok(TaskHistoryTransition::TurnCreated {
            turn_id: parse_checkpoint_id(&turn_id)?,
            initial_state: initial_state.into(),
        }),
        CheckpointTaskHistoryTransition::Turn {
            turn_id,
            previous_state,
            new_state,
        } => Ok(TaskHistoryTransition::Turn {
            turn_id: parse_checkpoint_id(&turn_id)?,
            previous_state: previous_state.into(),
            new_state: new_state.into(),
        }),
    }
}

fn parse_checkpoint_id(value: &str) -> Result<Uuid, TaskSessionCheckpointError> {
    let id = Uuid::parse_str(value).map_err(|_| TaskSessionCheckpointError::InvalidCheckpoint)?;
    if id.is_nil() || id.to_string() != value {
        return Err(TaskSessionCheckpointError::InvalidCheckpoint);
    }
    Ok(id)
}

fn claim_checkpoint_id(
    value: &str,
    identifiers: &mut HashSet<Uuid>,
) -> Result<Uuid, TaskSessionCheckpointError> {
    let id = parse_checkpoint_id(value)?;
    if !identifiers.insert(id) {
        return Err(TaskSessionCheckpointError::InvalidCheckpoint);
    }
    Ok(id)
}

fn checkpoint_sequence(index: usize) -> Result<u64, TaskSessionCheckpointError> {
    u64::try_from(index)
        .ok()
        .and_then(|sequence| sequence.checked_add(1))
        .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)
}

fn checkpoint_text_size(session: &TaskSession) -> Option<usize> {
    session.tasks.iter().try_fold(0_usize, |size, task| {
        let size = size.checked_add(task.description.len())?;
        task.turns.iter().try_fold(size, |size, turn| {
            let outcome_size = match turn.outcome.as_ref() {
                Some(TurnOutcome::Success { result }) => result.len(),
                Some(TurnOutcome::Failure { detail }) => detail.len(),
                None => 0,
            };
            size.checked_add(outcome_size)
        })
    })
}

fn validate_restored_task(task: &Task) -> Result<(), TaskSessionCheckpointError> {
    let turns_by_id: HashMap<Uuid, &Turn> = task.turns.iter().map(|turn| (turn.id, turn)).collect();
    if turns_by_id.len() != task.turns.len() {
        return Err(TaskSessionCheckpointError::InvalidCheckpoint);
    }

    let mut task_state = TaskState::Pending;
    let mut task_event_index = 0;
    let mut created_turn_index = 0;
    let mut seen_turns = HashSet::with_capacity(task.turns.len());
    let mut turn_states = HashMap::with_capacity(task.turns.len());
    let mut turn_event_indices = HashMap::with_capacity(task.turns.len());
    for (index, event) in task.history.iter().enumerate() {
        if event.task_id != task.id || event.sequence != checkpoint_sequence(index)? {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
        match event.transition {
            TaskHistoryTransition::Task {
                previous_state,
                new_state,
            } => {
                let task_event = task
                    .events
                    .get(task_event_index)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                if task_event.task_id != task.id
                    || task_event.sequence != checkpoint_sequence(task_event_index)?
                    || task_event.previous_state != previous_state
                    || task_event.new_state != new_state
                    || task_state != previous_state
                    || !task_state.can_transition_to(new_state)
                    || (new_state.is_terminal()
                        && turn_states
                            .values()
                            .any(|state| matches!(state, TurnState::Pending | TurnState::Running)))
                {
                    return Err(TaskSessionCheckpointError::InvalidCheckpoint);
                }
                task_state = new_state;
                task_event_index += 1;
            }
            TaskHistoryTransition::TurnCreated {
                turn_id,
                initial_state,
            } => {
                let expected_turn = task
                    .turns
                    .get(created_turn_index)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                if task_state.is_terminal()
                    || initial_state != TurnState::Pending
                    || expected_turn.id != turn_id
                    || !seen_turns.insert(turn_id)
                    || !turns_by_id.contains_key(&turn_id)
                {
                    return Err(TaskSessionCheckpointError::InvalidCheckpoint);
                }
                turn_states.insert(turn_id, initial_state);
                turn_event_indices.insert(turn_id, 0_usize);
                created_turn_index += 1;
            }
            TaskHistoryTransition::Turn {
                turn_id,
                previous_state,
                new_state,
            } => {
                let current_state = *turn_states
                    .get(&turn_id)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                if current_state != previous_state
                    || !current_state.can_transition_to(new_state)
                    || (current_state == TurnState::Pending
                        && new_state == TurnState::Running
                        && task_state != TaskState::Running)
                {
                    return Err(TaskSessionCheckpointError::InvalidCheckpoint);
                }
                let turn = turns_by_id
                    .get(&turn_id)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                let turn_event_index = *turn_event_indices
                    .get(&turn_id)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                let turn_event = turn
                    .events
                    .get(turn_event_index)
                    .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?;
                if turn_event.task_id != task.id
                    || turn_event.turn_id != turn_id
                    || turn_event.sequence != checkpoint_sequence(turn_event_index)?
                    || turn_event.previous_state != previous_state
                    || turn_event.new_state != new_state
                {
                    return Err(TaskSessionCheckpointError::InvalidCheckpoint);
                }
                turn_states.insert(turn_id, new_state);
                turn_event_indices.insert(
                    turn_id,
                    turn_event_index
                        .checked_add(1)
                        .ok_or(TaskSessionCheckpointError::InvalidCheckpoint)?,
                );
            }
        }
    }

    if task_event_index != task.events.len()
        || created_turn_index != task.turns.len()
        || task_state != task.state
        || turn_states.len() != task.turns.len()
    {
        return Err(TaskSessionCheckpointError::InvalidCheckpoint);
    }

    let mut activity_by_turn: HashMap<Uuid, Vec<TurnActivitySummary>> = HashMap::new();
    for (index, event) in task.activity_history.iter().enumerate() {
        if event.task_id != task.id
            || event.sequence != checkpoint_sequence(index)?
            || !turns_by_id.contains_key(&event.turn_id)
        {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
        activity_by_turn
            .entry(event.turn_id)
            .or_default()
            .push(event.summary);
    }

    for turn in &task.turns {
        if turn.task_id != task.id
            || turn_states.get(&turn.id) != Some(&turn.state)
            || turn_event_indices.get(&turn.id) != Some(&turn.events.len())
        {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
        if turn.state == TurnState::Running {
            return Err(TaskSessionCheckpointError::RunningTurn {
                task_id: task.id,
                turn_id: turn.id,
            });
        }
        let outcome_is_consistent = matches!(
            (turn.state, turn.outcome.as_ref()),
            (
                TurnState::Succeeded,
                None | Some(TurnOutcome::Success { .. })
            ) | (TurnState::Failed, None | Some(TurnOutcome::Failure { .. }))
                | (TurnState::Pending | TurnState::Cancelled, None)
        );
        if !outcome_is_consistent {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
        let expected_activity = activity_by_turn
            .get(&turn.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if turn.activity_history.as_slice() != expected_activity
            || (!turn.activity_history.is_empty() && turn.state == TurnState::Pending)
        {
            return Err(TaskSessionCheckpointError::InvalidCheckpoint);
        }
    }

    if task.state.is_terminal()
        && turn_states
            .values()
            .any(|state| matches!(state, TurnState::Pending | TurnState::Running))
    {
        return Err(TaskSessionCheckpointError::InvalidCheckpoint);
    }
    Ok(())
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;
