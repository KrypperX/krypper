use crate::tools::{
    apply_patch_file_at, capture_workspace_root, capture_workspace_root_at, create_directory_at,
    create_text_file_at, list_task_directory_names_at, read_root_agents_md_at, read_text_file_at,
    replace_text_file_at, search_text_files_at, search_tree_text_files_at, PatchEvidence,
    RootGuidanceReadError, WorkspaceRoot,
};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

mod checkpoint;
pub use checkpoint::{TaskSessionCheckpointError, MAX_TASK_SESSION_CHECKPOINT_BYTES};

pub use super::task::{
    Task, TaskActivityEvent, TaskEvent, TaskHistoryEvent, TaskHistoryTransition, TaskState,
    TransitionError, Turn, TurnActivityAction, TurnActivitySummary, TurnCompletionError,
    TurnCreationError, TurnEvent, TurnOutcome, TurnState, TurnTransitionError,
};

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

/// A creation-ordered collection of tasks with a stable session identifier.
#[derive(Debug)]
pub struct TaskSession {
    id: Uuid,
    workspace_root: Option<Arc<WorkspaceRoot>>,
    tasks: Vec<Task>,
    process_policies: std::collections::HashMap<Uuid, Arc<crate::OfflineCargoPolicy>>,
}

/// An immutable view of the current identifiers and states in a task session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSessionSnapshot {
    id: Uuid,
    tasks: Vec<TaskSnapshot>,
}

impl TaskSessionSnapshot {
    /// Returns the stable identifier of the task session.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns task snapshots in their session creation order.
    pub fn tasks(&self) -> &[TaskSnapshot] {
        &self.tasks
    }
}

/// An immutable view of one task's state, turns, and history sequence positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshot {
    id: Uuid,
    state: TaskState,
    lifecycle_sequence_high_water: u64,
    activity_sequence_high_water: u64,
    turns: Vec<TurnSnapshot>,
}

impl TaskSnapshot {
    /// Returns the stable identifier of the task.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the current lifecycle state of the task.
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// Returns the last sequence number in this task's combined lifecycle history.
    ///
    /// This one-based sequence is independent of the activity history sequence and is zero when
    /// the combined lifecycle history is empty.
    pub fn lifecycle_sequence_high_water(&self) -> u64 {
        self.lifecycle_sequence_high_water
    }

    /// Returns the last sequence number in this task's metadata-only activity history.
    ///
    /// This one-based sequence is independent of the lifecycle history sequence and is zero when
    /// the activity history is empty.
    pub fn activity_sequence_high_water(&self) -> u64 {
        self.activity_sequence_high_water
    }

    /// Returns turn snapshots in their task creation order.
    pub fn turns(&self) -> &[TurnSnapshot] {
        &self.turns
    }
}

/// An immutable view of one task-owned turn's identifier and current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnSnapshot {
    id: Uuid,
    state: TurnState,
}

impl TurnSnapshot {
    /// Returns the stable identifier of the turn.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the current lifecycle state of the turn.
    pub fn state(&self) -> TurnState {
        self.state
    }
}

/// A requested TaskSession workspace could not be captured safely.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSessionWorkspaceError(String);

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

impl TaskSession {
    /// Creates an empty task session.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            workspace_root: None,
            tasks: Vec::new(),
            process_policies: Default::default(),
        }
    }

    /// Creates an empty task session bound to an explicitly selected workspace directory.
    ///
    /// The directory is canonicalized and captured with the same filesystem identity and path
    /// containment protections used by task workspace actions. Tasks created by this session keep
    /// using that captured directory even if the process current directory later changes. This
    /// opt-in constructor does not change the process current directory. [`Self::new`] retains its
    /// existing behavior of capturing the current directory when each task is created.
    ///
    /// # Errors
    ///
    /// Returns a clear error if the path cannot be resolved, opened, or verified as a directory.
    ///
    /// # Example
    ///
    /// ```
    /// use krypper::task_lifecycle::TaskSession;
    ///
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let mut session = TaskSession::new_in_workspace(".")?;
    ///     let task_id = session.create_task("Summarize the selected workspace").id();
    ///     assert!(session.get_task(task_id).is_some());
    ///     Ok(())
    /// }
    /// ```
    pub fn new_in_workspace(
        workspace_path: impl AsRef<Path>,
    ) -> Result<Self, TaskSessionWorkspaceError> {
        let workspace_root = capture_workspace_root_at(workspace_path.as_ref())
            .map_err(TaskSessionWorkspaceError)?;
        Ok(Self {
            id: Uuid::new_v4(),
            workspace_root: Some(Arc::new(workspace_root)),
            tasks: Vec::new(),
            process_policies: Default::default(),
        })
    }

    /// Returns the stable session identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns an immutable, on-demand view of the session's current task and turn state.
    ///
    /// The snapshot contains only the session, task, and turn identifiers and states, plus each
    /// task's independent combined-lifecycle and metadata-only activity sequence high-water
    /// values. A high-water value is zero when its history is empty. Creating a snapshot does not
    /// consume history or change session state, and the snapshot exposes no task descriptions,
    /// outcomes, prompts, provider data, workspace information, credentials, or secrets.
    ///
    /// ```
    /// use krypper::task_lifecycle::{TaskSession, TaskState};
    ///
    /// let mut session = TaskSession::new();
    /// let task_id = session.create_task("Summarize the selected workspace").id();
    /// let snapshot = session.snapshot();
    /// let task = &snapshot.tasks()[0];
    /// assert_eq!(task.id(), task_id);
    /// assert_eq!(task.state(), TaskState::Pending);
    /// assert_eq!(task.lifecycle_sequence_high_water(), 0);
    /// ```
    pub fn snapshot(&self) -> TaskSessionSnapshot {
        TaskSessionSnapshot {
            id: self.id,
            tasks: self
                .tasks
                .iter()
                .map(|task| TaskSnapshot {
                    id: task.id,
                    state: task.state,
                    lifecycle_sequence_high_water: task
                        .history
                        .last()
                        .map_or(0, TaskHistoryEvent::sequence),
                    activity_sequence_high_water: task
                        .activity_history
                        .last()
                        .map_or(0, TaskActivityEvent::sequence),
                    turns: task
                        .turns
                        .iter()
                        .map(|turn| TurnSnapshot {
                            id: turn.id,
                            state: turn.state,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Creates and stores a pending task, returning it for immediate use.
    pub fn create_task(&mut self, description: impl Into<String>) -> &mut Task {
        let workspace_root = match &self.workspace_root {
            Some(root) => Ok(Arc::clone(root)),
            None => capture_workspace_root().map(Arc::new),
        };
        self.tasks
            .push(Task::new_with_workspace_root(description, workspace_root));
        self.tasks
            .last_mut()
            .expect("invariant: a task was just appended")
    }

    /// Returns a task in this session by identifier.
    pub fn get_task(&self, task_id: Uuid) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == task_id)
    }

    /// Returns a mutable task in this session by identifier.
    pub fn get_task_mut(&mut self, task_id: Uuid) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == task_id)
    }

    pub(crate) fn read_root_project_guidance(
        &self,
        task_id: Uuid,
    ) -> Result<Option<String>, WorkspaceGuidanceReadError> {
        let root = self
            .get_task(task_id)
            .ok_or(WorkspaceGuidanceReadError::WorkspaceUnavailable)?
            .workspace_root
            .as_ref()
            .map_err(|_| WorkspaceGuidanceReadError::WorkspaceUnavailable)?;
        read_root_agents_md_at(root).map_err(Into::into)
    }

    /// Completes a running turn owned by the requested task in this session.
    ///
    /// Returns a typed error without changing any task, turn, lifecycle history, or existing
    /// outcome when the task or turn cannot be completed.
    pub fn complete_turn(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        outcome: TurnOutcome,
    ) -> Result<(), TurnCompletionError> {
        self.complete_turn_with_activity(task_id, turn_id, outcome, &[])
    }

    pub(crate) fn complete_turn_with_activity(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        outcome: TurnOutcome,
        activity: &[TurnActivitySummary],
    ) -> Result<(), TurnCompletionError> {
        let task = self
            .get_task(task_id)
            .ok_or(TurnCompletionError::TaskNotFound { task_id })?;
        if task.get_turn(turn_id).is_none() {
            let owner_task_id = self
                .tasks
                .iter()
                .find(|candidate| candidate.get_turn(turn_id).is_some())
                .map(Task::id);
            return match owner_task_id {
                Some(owner_task_id) => Err(TurnCompletionError::ForeignTurn {
                    task_id,
                    turn_id,
                    owner_task_id,
                }),
                None => Err(TurnCompletionError::TurnNotFound { task_id, turn_id }),
            };
        }

        self.get_task_mut(task_id)
            .ok_or(TurnCompletionError::TaskNotFound { task_id })?
            .complete_turn_with_activity(turn_id, outcome, activity)
    }

    /// Records metadata-only activity and cancels a running task-owned turn and its task.
    ///
    /// Activity summaries are appended in order before the existing lifecycle cancellation
    /// transitions. Cancellation does not create a success or failure outcome.
    pub fn cancel_turn_with_activity(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        activity: &[TurnActivitySummary],
    ) -> Result<(), TurnCompletionError> {
        let Some(task_index) = self.tasks.iter().position(|task| task.id == task_id) else {
            return Err(TurnCompletionError::TaskNotFound { task_id });
        };

        let Some(turn_index) = self.tasks[task_index]
            .turns
            .iter()
            .position(|turn| turn.id == turn_id)
        else {
            let owner_task_id = self
                .tasks
                .iter()
                .find(|task| task.get_turn(turn_id).is_some())
                .map(Task::id);
            return match owner_task_id {
                Some(owner_task_id) => Err(TurnCompletionError::ForeignTurn {
                    task_id,
                    turn_id,
                    owner_task_id,
                }),
                None => Err(TurnCompletionError::TurnNotFound { task_id, turn_id }),
            };
        };

        let task = &self.tasks[task_index];
        let turn_state = task.turns[turn_index].state;
        if turn_state != TurnState::Running {
            return Err(TurnCompletionError::TurnNotRunning {
                turn_id,
                state: turn_state,
            });
        }
        if !task.state.can_transition_to(TaskState::Cancelled) {
            return Err(TurnCompletionError::TaskTransition(TransitionError {
                from: task.state,
                to: TaskState::Cancelled,
                blocking_turn: None,
            }));
        }

        let task = &mut self.tasks[task_index];
        task.record_activity(turn_index, activity);
        task.cancel().map_err(TurnCompletionError::TaskTransition)
    }

    fn running_workspace_task(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<&Task, WorkspaceReadError> {
        let task = self
            .get_task(task_id)
            .ok_or(WorkspaceReadError::TaskNotFound { task_id })?;
        let turn = match task.get_turn(turn_id) {
            Some(turn) => turn,
            None => {
                let owner_task_id = self
                    .tasks
                    .iter()
                    .find(|candidate| candidate.get_turn(turn_id).is_some())
                    .map(Task::id);
                return match owner_task_id {
                    Some(owner_task_id) => Err(WorkspaceReadError::ForeignTurn {
                        task_id,
                        turn_id,
                        owner_task_id,
                    }),
                    None => Err(WorkspaceReadError::TurnNotFound { task_id, turn_id }),
                };
            }
        };
        if turn.state() != TurnState::Running {
            return Err(WorkspaceReadError::TurnNotRunning {
                turn_id,
                state: turn.state(),
            });
        }
        Ok(task)
    }

    /// Checks task ownership and running-turn state without performing a workspace operation.
    pub(crate) fn validate_running_turn(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<(), WorkspaceReadError> {
        self.running_workspace_task(task_id, turn_id).map(|_| ())
    }

    /// Grants this task the fixed offline Cargo profile. Only trusted embedding code may
    /// grant it; model arguments and restored checkpoints never grant process authority.
    pub fn authorize_offline_cargo_tests(
        &mut self,
        task_id: Uuid,
        policy: crate::OfflineCargoPolicy,
    ) -> Result<(), &'static str> {
        if self.get_task(task_id).is_none() {
            return Err("task not found");
        }
        self.process_policies.insert(task_id, Arc::new(policy));
        Ok(())
    }

    /// Executes the one fixed profile after task ownership and running-turn checks.
    pub fn run_offline_cargo_tests(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<crate::ProcessResult, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let Some(policy) = self.process_policies.get(&task_id) else {
            return Ok(crate::ProcessResult::denied());
        };
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|message| WorkspaceReadError::File {
                message: message.clone(),
            })?;
        Ok(crate::tools::process::run(root, policy))
    }

    /// Reads one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, regular-file, and 1 MiB policies as the `show` command.
    /// This operation borrows the session immutably and does not change lifecycle state or history.
    pub fn read_workspace_file(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<String, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|message| WorkspaceReadError::File {
                message: format!("task workspace could not be resolved: {message}"),
            })?;
        read_text_file_at(root, path.as_os_str()).map_err(|error| WorkspaceReadError::File {
            message: error.message().to_owned(),
        })
    }

    /// Lists immediate filesystem-native entry names for a currently running owned turn.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, missing-path, and directory policies as the `list` command.
    /// Names use the same deterministic order as `list` and can be used directly as path
    /// components. At most 1,024 immediate names are returned; larger directories fail without a
    /// partial result. This operation borrows the session immutably and does not change lifecycle
    /// state or history.
    pub fn list_workspace_directory(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<Vec<OsString>, WorkspaceListError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceListError::from)?;
        let root =
            task.workspace_root
                .as_ref()
                .map_err(|message| WorkspaceListError::Directory {
                    message: format!("task workspace could not be resolved: {message}"),
                })?;
        list_task_directory_names_at(root, path.as_os_str()).map_err(|error| {
            WorkspaceListError::Directory {
                message: error.message().to_owned(),
            }
        })
    }

    /// Searches immediate regular UTF-8 files in one workspace directory for a literal query.
    ///
    /// Paths are relative to the task's captured workspace. Entry symlinks and non-regular files
    /// are skipped; directory traversal is not recursive. Queries, total bytes scanned, and
    /// returned file-line locations are bounded to 4 KiB, 16 MiB, and 10,000 locations. Each
    /// file remains limited to 1 MiB, and directories use the existing 1,024-entry limit. Query
    /// and file contents are not retained in task activity history.
    pub fn search_workspace_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        directory: &Path,
        query: &str,
    ) -> Result<Vec<WorkspaceSearchLocation>, WorkspaceSearchError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceSearchError::from)?;
        let root =
            task.workspace_root
                .as_ref()
                .map_err(|message| WorkspaceSearchError::Search {
                    message: format!("task workspace could not be resolved: {message}"),
                })?;
        search_text_files_at(root, directory.as_os_str(), query)
            .map(|matches| {
                matches
                    .into_iter()
                    .map(|(path, line)| WorkspaceSearchLocation { path, line })
                    .collect()
            })
            .map_err(|error| WorkspaceSearchError::Search {
                message: error.message().to_owned(),
            })
    }

    /// Creates exactly one directory for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's captured workspace. Its parent must already
    /// exist; symlink components, escaping paths, and existing targets are rejected. Creation uses
    /// the process umask and does not change lifecycle state or history.
    pub fn create_workspace_directory(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<(), WorkspaceCreateDirectoryError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceCreateDirectoryError::from)?;
        let root = task.workspace_root.as_ref().map_err(|message| {
            WorkspaceCreateDirectoryError::Directory {
                message: format!("task workspace could not be resolved: {message}"),
            }
        })?;
        create_directory_at(root, path.as_os_str()).map_err(|error| {
            WorkspaceCreateDirectoryError::Directory {
                message: error.message().to_owned(),
            }
        })
    }

    /// Searches regular UTF-8 files recursively under one workspace directory for a literal query.
    ///
    /// Paths are relative to the task's captured workspace. Entry symlinks and non-regular files
    /// are skipped; hidden entries are included. Queries, files, cumulative bytes, locations,
    /// traversal depth, and directory entries are bounded to 4 KiB, 1 MiB, 16 MiB, 10,000,
    /// 16 levels, and 4,096 entries. Query and file contents are not retained in activity history.
    pub fn search_workspace_tree_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        directory: &Path,
        query: &str,
    ) -> Result<Vec<WorkspaceSearchLocation>, WorkspaceSearchError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceSearchError::from)?;
        let root =
            task.workspace_root
                .as_ref()
                .map_err(|message| WorkspaceSearchError::Search {
                    message: format!("task workspace could not be resolved: {message}"),
                })?;
        search_tree_text_files_at(root, directory.as_os_str(), query)
            .map(|matches| {
                matches
                    .into_iter()
                    .map(|(path, line)| WorkspaceSearchLocation { path, line })
                    .collect()
            })
            .map_err(|error| WorkspaceSearchError::Search {
                message: error.message().to_owned(),
            })
    }

    /// Creates one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, parent-directory, create-only, and 1 MiB policies as the
    /// `write` command. This operation borrows the session immutably and does not change lifecycle
    /// state or history.
    pub fn create_workspace_file(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        text: &str,
    ) -> Result<(), WorkspaceWriteError> {
        let task = self
            .get_task(task_id)
            .ok_or(WorkspaceWriteError::TaskNotFound { task_id })?;
        let turn = match task.get_turn(turn_id) {
            Some(turn) => turn,
            None => {
                let owner_task_id = self
                    .tasks
                    .iter()
                    .find(|candidate| candidate.get_turn(turn_id).is_some())
                    .map(Task::id);
                return match owner_task_id {
                    Some(owner_task_id) => Err(WorkspaceWriteError::ForeignTurn {
                        task_id,
                        turn_id,
                        owner_task_id,
                    }),
                    None => Err(WorkspaceWriteError::TurnNotFound { task_id, turn_id }),
                };
            }
        };
        if turn.state() != TurnState::Running {
            return Err(WorkspaceWriteError::TurnNotRunning {
                turn_id,
                state: turn.state(),
            });
        }

        let root = task
            .workspace_root
            .as_ref()
            .map_err(|message| WorkspaceWriteError::File {
                message: format!("task workspace could not be resolved: {message}"),
            })?;
        create_text_file_at(root, path.as_os_str(), text).map_err(|error| {
            WorkspaceWriteError::File {
                message: error.message().to_owned(),
            }
        })
    }

    /// Replaces one unique exact text match in an existing UTF-8 file for a running task turn.
    ///
    /// The path is resolved beneath the task's captured workspace. Symlink path components,
    /// non-regular files, missing parents, and content over 1 MiB are rejected. The replacement
    /// is staged beside the original and committed with an atomic rename; this operation does not
    /// change lifecycle state or retain either text value in task history. Returns the resulting
    /// file size in bytes.
    pub fn replace_workspace_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        match_text: &str,
        replacement_text: &str,
    ) -> Result<usize, WorkspaceReplaceError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceReplaceError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|message| WorkspaceReplaceError::File {
                message: format!("task workspace could not be resolved: {message}"),
            })?;
        replace_text_file_at(root, path.as_os_str(), match_text, replacement_text).map_err(
            |error| WorkspaceReplaceError::File {
                message: error.message().to_owned(),
            },
        )
    }

    /// Applies one context-checked unified diff to an existing UTF-8 file in a running task.
    ///
    /// The patch is resolved beneath the task's captured workspace. It preserves the file's line
    /// endings and supported permission bits, and succeeds only after atomic replacement and
    /// exact readback verification.
    pub(crate) fn apply_workspace_patch(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        patch: &str,
    ) -> Result<PatchEvidence, WorkspaceReplaceError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceReplaceError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|message| WorkspaceReplaceError::File {
                message: format!("task workspace could not be resolved: {message}"),
            })?;
        apply_patch_file_at(root, path.as_os_str(), patch).map_err(|error| {
            WorkspaceReplaceError::File {
                message: error.message().to_owned(),
            }
        })
    }

    /// Returns the tasks in creation order as a read-only slice.
    pub fn list_tasks(&self) -> &[Task] {
        &self.tasks
    }
}

impl Default for TaskSession {
    fn default() -> Self {
        Self::new()
    }
}
