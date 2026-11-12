use crate::file_safety::{
    capture_workspace_root, create_text_file_at, list_task_directory_names_at, read_text_file_at,
    replace_text_file_at, search_text_files_at, WorkspaceRoot,
};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The lifecycle states supported by a task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    /// Returns the stable lowercase name used in lifecycle records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled
                )
        )
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An attempted transition that is not valid for the task's current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionError {
    from: TaskState,
    to: TaskState,
    blocking_turn: Option<(Uuid, TurnState)>,
}

impl TransitionError {
    /// Returns the task state before the rejected transition.
    pub fn from(self) -> TaskState {
        self.from
    }

    /// Returns the requested state of the rejected transition.
    pub fn to(self) -> TaskState {
        self.to
    }

    /// Returns the active turn that prevented a terminal task transition.
    pub fn blocking_turn(self) -> Option<(Uuid, TurnState)> {
        self.blocking_turn
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.blocking_turn {
            Some((turn_id, turn_state)) => write!(
                formatter,
                "cannot transition task from '{}' to '{}' while owned turn '{}' is '{}'",
                self.from.as_str(),
                self.to.as_str(),
                turn_id,
                turn_state.as_str()
            ),
            None => write!(
                formatter,
                "cannot transition task from '{}' to '{}'",
                self.from.as_str(),
                self.to.as_str()
            ),
        }
    }
}

impl std::error::Error for TransitionError {}

/// An attempt to create a turn for a terminal task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnCreationError {
    task_state: TaskState,
}

impl TurnCreationError {
    /// Returns the terminal state that prevented turn creation.
    pub fn task_state(&self) -> TaskState {
        self.task_state
    }
}

impl fmt::Display for TurnCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot create a turn for task in terminal state '{}'",
            self.task_state
        )
    }
}

impl std::error::Error for TurnCreationError {}

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

/// An immutable record of one successful state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvent {
    task_id: Uuid,
    sequence: u64,
    previous_state: TaskState,
    new_state: TaskState,
}

impl TaskEvent {
    /// Returns the task whose history contains this event.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the one-based position of this transition in the task history.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the state before this transition.
    pub fn previous_state(&self) -> TaskState {
        self.previous_state
    }

    /// Returns the state after this transition.
    pub fn new_state(&self) -> TaskState {
        self.new_state
    }
}

/// The lifecycle states supported by a task turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TurnState {
    /// Returns the stable lowercase name used in lifecycle records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled
                )
        )
    }
}

impl fmt::Display for TurnState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The caller-provided result or failure detail recorded when a turn completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    /// The turn completed successfully with its result.
    Success { result: String },
    /// The turn failed with its failure detail.
    Failure { detail: String },
}

impl TurnOutcome {
    fn terminal_state(&self) -> TurnState {
        match self {
            Self::Success { .. } => TurnState::Succeeded,
            Self::Failure { .. } => TurnState::Failed,
        }
    }
}

/// The action category recorded when a workspace action fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnActivityAction {
    /// A workspace file read failed.
    ReadFile,
    /// A bounded workspace file range read failed.
    ReadRange,
    /// A workspace directory listing failed.
    ListDirectory,
    /// A workspace file creation failed.
    CreateFile,
    /// A workspace text replacement failed.
    ReplaceText,
    /// A workspace text search failed.
    SearchText,
    /// The decision source selected an unsupported action.
    Unsupported,
}

/// A metadata-only summary of one action or stop event in a task-owned turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnActivitySummary {
    /// A file was read; only its byte count is retained.
    FileRead { bytes_read: usize },
    /// A file range was read; only its range and excerpt byte count are retained.
    FileRangeRead {
        start_line: usize,
        end_line: usize,
        bytes_returned: usize,
    },
    /// A file was created; only its byte count is retained.
    FileCreated { bytes_written: usize },
    /// Text was replaced; only the resulting file size is retained.
    TextReplaced { bytes_written: usize },
    /// A directory was listed; entry names are not retained.
    DirectoryListed { entry_count: usize },
    /// Text was searched; only matching location counts are retained.
    TextSearched { location_count: usize },
    /// The decision source explicitly completed the turn; its result is not retained here.
    Completed,
    /// The caller requested another turn for the same task.
    Continued,
    /// The loop exhausted its action budget.
    BudgetExhausted { actions: usize },
    /// The caller cancelled the task-owned turn.
    Cancelled,
    /// A workspace or unsupported action failed.
    ActionFailed { action: TurnActivityAction },
}

/// An attempted transition that is not valid for a turn's current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnTransitionError {
    UnknownTurn {
        turn_id: Uuid,
    },
    /// Starting a pending turn requires its owning task to be running.
    TaskNotRunning {
        task_id: Uuid,
        task_state: TaskState,
        turn_id: Uuid,
        turn_state: TurnState,
    },
    InvalidTransition {
        from: TurnState,
        to: TurnState,
    },
}

impl fmt::Display for TurnTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTurn { turn_id } => {
                write!(formatter, "turn '{turn_id}' is not owned by this task")
            }
            Self::TaskNotRunning {
                task_id,
                task_state,
                turn_id,
                turn_state,
            } => write!(
                formatter,
                "cannot start turn '{turn_id}' in state '{turn_state}' while owning task '{task_id}' is '{task_state}'"
            ),
            Self::InvalidTransition { from, to } => write!(
                formatter,
                "cannot transition turn from '{}' to '{}'",
                from.as_str(),
                to.as_str()
            ),
        }
    }
}

impl std::error::Error for TurnTransitionError {}

/// An attempt to complete or cancel a turn whose task or active turn is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnCompletionError {
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
    /// The turn exists but is not currently running.
    TurnNotRunning { turn_id: Uuid, state: TurnState },
    /// The owning task cannot transition to cancelled in its current state.
    TaskTransition(TransitionError),
}

impl fmt::Display for TurnCompletionError {
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
            Self::TaskTransition(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TurnCompletionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TaskTransition(error) => Some(error),
            Self::TaskNotFound { .. }
            | Self::TurnNotFound { .. }
            | Self::ForeignTurn { .. }
            | Self::TurnNotRunning { .. } => None,
        }
    }
}

/// An immutable record of one successful turn state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnEvent {
    task_id: Uuid,
    turn_id: Uuid,
    sequence: u64,
    previous_state: TurnState,
    new_state: TurnState,
}

impl TurnEvent {
    /// Returns the task that owns the turn for this event.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the turn whose history contains this event.
    pub fn turn_id(&self) -> Uuid {
        self.turn_id
    }

    /// Returns the one-based position of this transition in the turn history.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the state before this transition.
    pub fn previous_state(&self) -> TurnState {
        self.previous_state
    }

    /// Returns the state after this transition.
    pub fn new_state(&self) -> TurnState {
        self.new_state
    }
}

/// The lifecycle change recorded in a task's combined history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskHistoryTransition {
    /// A task changed lifecycle state.
    Task {
        previous_state: TaskState,
        new_state: TaskState,
    },
    /// A task created a turn in its initial state.
    TurnCreated {
        turn_id: Uuid,
        initial_state: TurnState,
    },
    /// A task-owned turn changed lifecycle state.
    Turn {
        turn_id: Uuid,
        previous_state: TurnState,
        new_state: TurnState,
    },
}

/// An immutable record in a task's combined task and turn lifecycle history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskHistoryEvent {
    task_id: Uuid,
    sequence: u64,
    transition: TaskHistoryTransition,
}

impl TaskHistoryEvent {
    /// Returns the task whose history contains this event.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the one-based position of this entry in the combined task history.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns which task or turn lifecycle change this entry records.
    pub fn transition(&self) -> TaskHistoryTransition {
        self.transition
    }
}

/// An immutable metadata-only action summary in a task's activity history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskActivityEvent {
    task_id: Uuid,
    turn_id: Uuid,
    sequence: u64,
    summary: TurnActivitySummary,
}

impl TaskActivityEvent {
    /// Returns the task that owns the summarized turn.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the turn that produced this activity.
    pub fn turn_id(&self) -> Uuid {
        self.turn_id
    }

    /// Returns this entry's one-based position in the task activity history.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the action or stop summary without workspace data or result text.
    pub fn summary(&self) -> TurnActivitySummary {
        self.summary
    }
}

/// A task-owned turn with a stable identifier and append-only transition history.
#[derive(Debug)]
pub struct Turn {
    id: Uuid,
    task_id: Uuid,
    state: TurnState,
    outcome: Option<TurnOutcome>,
    events: Vec<TurnEvent>,
    activity_history: Vec<TurnActivitySummary>,
}

impl Turn {
    fn new(task_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            task_id,
            state: TurnState::Pending,
            outcome: None,
            events: Vec::new(),
            activity_history: Vec::new(),
        }
    }

    /// Returns the stable turn identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the identifier of the task that owns this turn.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the current lifecycle state.
    pub fn state(&self) -> TurnState {
        self.state
    }

    /// Returns the recorded terminal outcome by a read-only reference.
    ///
    /// State-only transitions and cancellation do not create an outcome.
    ///
    /// ```compile_fail
    /// use krypper::task_lifecycle::TurnOutcome;
    ///
    /// fn replace(outcome: &TurnOutcome) {
    ///     *outcome = TurnOutcome::Success {
    ///         result: "changed".to_owned(),
    ///     };
    /// }
    /// ```
    pub fn outcome(&self) -> Option<&TurnOutcome> {
        self.outcome.as_ref()
    }

    /// Returns lifecycle history as a read-only slice.
    pub fn events(&self) -> &[TurnEvent] {
        &self.events
    }

    /// Returns metadata-only action summaries in execution order.
    pub fn activity_history(&self) -> &[TurnActivitySummary] {
        &self.activity_history
    }

    fn record_transition(&mut self, next: TurnState) {
        self.events.push(TurnEvent {
            task_id: self.task_id,
            turn_id: self.id,
            sequence: self.events.len() as u64 + 1,
            previous_state: self.state,
            new_state: next,
        });
        self.state = next;
    }

    fn cancel_if_active(&mut self) -> Option<TurnState> {
        if matches!(self.state, TurnState::Pending | TurnState::Running) {
            let previous_state = self.state;
            self.record_transition(TurnState::Cancelled);
            Some(previous_state)
        } else {
            None
        }
    }

    /// Records a valid transition and leaves the turn unchanged on rejection.
    fn transition(&mut self, next: TurnState) -> Result<(), TurnTransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(TurnTransitionError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.record_transition(next);
        Ok(())
    }
}

/// A task with a stable identifier and an append-only transition history.
#[derive(Debug)]
pub struct Task {
    id: Uuid,
    description: String,
    workspace_root: Result<WorkspaceRoot, String>,
    state: TaskState,
    events: Vec<TaskEvent>,
    history: Vec<TaskHistoryEvent>,
    activity_history: Vec<TaskActivityEvent>,
    turns: Vec<Turn>,
}

impl Task {
    /// Creates a pending task and captures the current directory as its workspace.
    ///
    /// Workspace operations report an error if that directory cannot be resolved.
    pub fn new(description: impl Into<String>) -> Self {
        let workspace_root = capture_workspace_root();
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            workspace_root,
            state: TaskState::Pending,
            events: Vec::new(),
            history: Vec::new(),
            activity_history: Vec::new(),
            turns: Vec::new(),
        }
    }

    /// Returns the stable task identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the task description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the current lifecycle state.
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// Returns lifecycle history as a read-only slice.
    pub fn events(&self) -> &[TaskEvent] {
        &self.events
    }

    /// Returns task transitions, turn creations, and owned-turn transitions in chronological order.
    ///
    /// The returned slice is read-only; callers can mutate a copy without changing
    /// the task's recorded history.
    ///
    /// ```compile_fail
    /// use krypper::task_lifecycle::Task;
    /// let task = Task::new("A task");
    /// task.history().clear();
    /// ```
    pub fn history(&self) -> &[TaskHistoryEvent] {
        &self.history
    }

    /// Returns action summaries across owned turns in chronological order.
    pub fn activity_history(&self) -> &[TaskActivityEvent] {
        &self.activity_history
    }

    fn record_transition(&mut self, next: TaskState) {
        let previous_state = self.state;
        self.events.push(TaskEvent {
            task_id: self.id,
            sequence: self.events.len() as u64 + 1,
            previous_state,
            new_state: next,
        });
        self.history.push(TaskHistoryEvent {
            task_id: self.id,
            sequence: self.history.len() as u64 + 1,
            transition: TaskHistoryTransition::Task {
                previous_state,
                new_state: next,
            },
        });
        self.state = next;
    }

    fn record_turn_history(
        history: &mut Vec<TaskHistoryEvent>,
        task_id: Uuid,
        turn_id: Uuid,
        previous_state: TurnState,
        new_state: TurnState,
    ) {
        history.push(TaskHistoryEvent {
            task_id,
            sequence: history.len() as u64 + 1,
            transition: TaskHistoryTransition::Turn {
                turn_id,
                previous_state,
                new_state,
            },
        });
    }

    /// Creates a pending turn, appending it to the turns and combined history in creation order.
    ///
    /// Returns an error without changing the task when it is terminal.
    pub fn create_turn(&mut self) -> Result<Uuid, TurnCreationError> {
        if self.state.is_terminal() {
            return Err(TurnCreationError {
                task_state: self.state,
            });
        }

        let turn = Turn::new(self.id);
        let turn_id = turn.id();
        let initial_state = turn.state();
        self.turns.push(turn);
        self.history.push(TaskHistoryEvent {
            task_id: self.id,
            sequence: self.history.len() as u64 + 1,
            transition: TaskHistoryTransition::TurnCreated {
                turn_id,
                initial_state,
            },
        });
        Ok(turn_id)
    }

    /// Returns a turn owned by this task by identifier.
    pub fn get_turn(&self, turn_id: Uuid) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == turn_id)
    }

    /// Records a valid transition for a turn owned by this task.
    pub fn transition_turn(
        &mut self,
        turn_id: Uuid,
        next: TurnState,
    ) -> Result<(), TurnTransitionError> {
        let task_id = self.id;
        let task_state = self.state;
        let previous_state = {
            let turn = self
                .turns
                .iter_mut()
                .find(|turn| turn.id == turn_id)
                .ok_or(TurnTransitionError::UnknownTurn { turn_id })?;
            if turn.state == TurnState::Pending
                && next == TurnState::Running
                && task_state != TaskState::Running
            {
                return Err(TurnTransitionError::TaskNotRunning {
                    task_id,
                    task_state,
                    turn_id,
                    turn_state: turn.state,
                });
            }
            let previous_state = turn.state;
            turn.transition(next)?;
            previous_state
        };
        Self::record_turn_history(&mut self.history, self.id, turn_id, previous_state, next);
        Ok(())
    }

    /// Completes a running turn and records its caller-provided outcome.
    ///
    /// The matching succeeded or failed state and lifecycle history entry are recorded with the
    /// outcome. A missing or non-running turn is left unchanged.
    pub fn complete_turn(
        &mut self,
        turn_id: Uuid,
        outcome: TurnOutcome,
    ) -> Result<(), TurnCompletionError> {
        self.complete_turn_with_activity(turn_id, outcome, &[])
    }

    fn complete_turn_with_activity(
        &mut self,
        turn_id: Uuid,
        outcome: TurnOutcome,
        activity: &[TurnActivitySummary],
    ) -> Result<(), TurnCompletionError> {
        let turn_index = self
            .turns
            .iter()
            .position(|turn| turn.id == turn_id)
            .ok_or(TurnCompletionError::TurnNotFound {
                task_id: self.id,
                turn_id,
            })?;
        let turn_state = self.turns[turn_index].state;
        if turn_state != TurnState::Running {
            return Err(TurnCompletionError::TurnNotRunning {
                turn_id,
                state: turn_state,
            });
        }

        let terminal_state = outcome.terminal_state();
        self.history.reserve(1);
        self.turns[turn_index].events.reserve(1);
        self.record_activity(turn_index, activity);
        self.turns[turn_index].outcome = Some(outcome);
        self.turns[turn_index].record_transition(terminal_state);
        Self::record_turn_history(
            &mut self.history,
            self.id,
            turn_id,
            TurnState::Running,
            terminal_state,
        );
        Ok(())
    }

    fn record_activity(&mut self, turn_index: usize, activity: &[TurnActivitySummary]) {
        self.activity_history.reserve(activity.len());
        self.turns[turn_index]
            .activity_history
            .reserve(activity.len());
        let turn_id = self.turns[turn_index].id;
        for summary in activity.iter().copied() {
            self.turns[turn_index].activity_history.push(summary);
            self.activity_history.push(TaskActivityEvent {
                task_id: self.id,
                turn_id,
                sequence: self.activity_history.len() as u64 + 1,
                summary,
            });
        }
    }

    /// Returns turns in creation order as a read-only slice.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Cancels this pending or running task and each of its active turns.
    ///
    /// Active turns are cancelled in creation order before the task transition
    /// is recorded. Terminal turns are left unchanged. A task that is already
    /// terminal returns a transition error without changing any history.
    pub fn cancel(&mut self) -> Result<(), TransitionError> {
        if !self.state.can_transition_to(TaskState::Cancelled) {
            return Err(TransitionError {
                from: self.state,
                to: TaskState::Cancelled,
                blocking_turn: None,
            });
        }

        for turn in &mut self.turns {
            if let Some(previous_state) = turn.cancel_if_active() {
                Self::record_turn_history(
                    &mut self.history,
                    self.id,
                    turn.id,
                    previous_state,
                    TurnState::Cancelled,
                );
            }
        }

        self.record_transition(TaskState::Cancelled);
        Ok(())
    }

    /// Records a valid transition and leaves the task unchanged on rejection.
    pub fn transition(&mut self, next: TaskState) -> Result<(), TransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(TransitionError {
                from: self.state,
                to: next,
                blocking_turn: None,
            });
        }

        if next.is_terminal() {
            if let Some(turn) = self
                .turns
                .iter()
                .find(|turn| matches!(turn.state, TurnState::Pending | TurnState::Running))
            {
                return Err(TransitionError {
                    from: self.state,
                    to: next,
                    blocking_turn: Some((turn.id, turn.state)),
                });
            }
        }

        self.record_transition(next);
        Ok(())
    }
}

/// A creation-ordered collection of tasks with a stable session identifier.
#[derive(Debug)]
pub struct TaskSession {
    id: Uuid,
    tasks: Vec<Task>,
}

impl TaskSession {
    /// Creates an empty task session.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            tasks: Vec::new(),
        }
    }

    /// Returns the stable session identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Creates and stores a pending task, returning it for immediate use.
    pub fn create_task(&mut self, description: impl Into<String>) -> &mut Task {
        self.tasks.push(Task::new(description));
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
