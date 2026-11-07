use crate::file_safety::{create_text_file, read_text_file};
use std::fmt;
use std::path::Path;
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

/// An attempted transition that is not valid for a turn's current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnTransitionError {
    UnknownTurn { turn_id: Uuid },
    InvalidTransition { from: TurnState, to: TurnState },
}

impl fmt::Display for TurnTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTurn { turn_id } => {
                write!(formatter, "turn '{turn_id}' is not owned by this task")
            }
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

/// A task-owned turn with a stable identifier and append-only transition history.
#[derive(Debug)]
pub struct Turn {
    id: Uuid,
    task_id: Uuid,
    state: TurnState,
    events: Vec<TurnEvent>,
}

impl Turn {
    fn new(task_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            task_id,
            state: TurnState::Pending,
            events: Vec::new(),
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

    /// Returns lifecycle history as a read-only slice.
    pub fn events(&self) -> &[TurnEvent] {
        &self.events
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
    state: TaskState,
    events: Vec<TaskEvent>,
    history: Vec<TaskHistoryEvent>,
    turns: Vec<Turn>,
}

impl Task {
    /// Creates a task in the `pending` state.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            state: TaskState::Pending,
            events: Vec::new(),
            history: Vec::new(),
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
        let previous_state = {
            let turn = self
                .turns
                .iter_mut()
                .find(|turn| turn.id == turn_id)
                .ok_or(TurnTransitionError::UnknownTurn { turn_id })?;
            let previous_state = turn.state;
            turn.transition(next)?;
            previous_state
        };
        Self::record_turn_history(&mut self.history, self.id, turn_id, previous_state, next);
        Ok(())
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

    /// Reads one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from the process current directory and follows the same
    /// workspace boundary, symlink, regular-file, and 1 MiB policies as the `show` command.
    /// This operation borrows the session immutably and does not change lifecycle state or history.
    pub fn read_workspace_file(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<String, WorkspaceReadError> {
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

        read_text_file(path.as_os_str()).map_err(|error| WorkspaceReadError::File {
            message: error.message().to_owned(),
        })
    }

    /// Creates one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from the process current directory and follows the same
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

        create_text_file(path.as_os_str(), text).map_err(|error| WorkspaceWriteError::File {
            message: error.message().to_owned(),
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
