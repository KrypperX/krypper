use std::fmt;
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
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot transition task from '{}' to '{}'",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for TransitionError {}

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

    /// Records a valid transition and leaves the turn unchanged on rejection.
    fn transition(&mut self, next: TurnState) -> Result<(), TurnTransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(TurnTransitionError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.events.push(TurnEvent {
            task_id: self.task_id,
            turn_id: self.id,
            sequence: self.events.len() as u64 + 1,
            previous_state: self.state,
            new_state: next,
        });
        self.state = next;
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

    /// Creates a pending turn, appends it to this task's history, and returns its ID.
    pub fn create_turn(&mut self) -> Uuid {
        let turn = Turn::new(self.id);
        let turn_id = turn.id();
        self.turns.push(turn);
        turn_id
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
        let turn = self
            .turns
            .iter_mut()
            .find(|turn| turn.id == turn_id)
            .ok_or(TurnTransitionError::UnknownTurn { turn_id })?;
        turn.transition(next)
    }

    /// Returns turns in creation order as a read-only slice.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Records a valid transition and leaves the task unchanged on rejection.
    pub fn transition(&mut self, next: TaskState) -> Result<(), TransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(TransitionError {
                from: self.state,
                to: next,
            });
        }

        self.events.push(TaskEvent {
            task_id: self.id,
            sequence: self.events.len() as u64 + 1,
            previous_state: self.state,
            new_state: next,
        });
        self.state = next;
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
