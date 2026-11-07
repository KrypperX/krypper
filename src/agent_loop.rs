use crate::task_lifecycle::{TaskSession, TurnCompletionError, TurnOutcome, WorkspaceReadError};
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// One action the prototype can ask a task-owned turn to perform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAction {
    /// Read one UTF-8 file under the task's workspace safety policy.
    ReadFile { path: PathBuf },
    /// A decision source requested an action this prototype does not support.
    Unsupported { action: String },
}

/// A replaceable source that selects one workspace action for a goal.
pub trait DecisionSource {
    /// Selects an action using the caller's goal.
    fn choose_action(&mut self, goal: &str) -> WorkspaceAction;
}

impl<F> DecisionSource for F
where
    F: FnMut(&str) -> WorkspaceAction,
{
    fn choose_action(&mut self, goal: &str) -> WorkspaceAction {
        self(goal)
    }
}

/// The read result and matching terminal outcome recorded for a task-owned turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnResult {
    task_id: Uuid,
    turn_id: Uuid,
    read_result: String,
    outcome: TurnOutcome,
}

impl GoalTurnResult {
    /// Returns the task that owns the completed turn.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// Returns the completed turn identifier.
    pub fn turn_id(&self) -> Uuid {
        self.turn_id
    }

    /// Returns the file contents read by the action.
    pub fn read_result(&self) -> &str {
        &self.read_result
    }

    /// Returns the outcome recorded on the completed turn.
    pub fn outcome(&self) -> &TurnOutcome {
        &self.outcome
    }
}

/// An error encountered while running one goal-selected workspace action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTurnError {
    /// The decision source requested an unsupported action.
    UnsupportedAction { action: String },
    /// The existing workspace safety policy rejected or could not perform the read.
    WorkspaceRead(WorkspaceReadError),
    /// The task or turn could not be completed in its current session state.
    Lifecycle(TurnCompletionError),
}

impl fmt::Display for GoalTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedAction { action } => {
                write!(formatter, "unsupported workspace action '{action}'")
            }
            Self::WorkspaceRead(error) => error.fmt(formatter),
            Self::Lifecycle(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GoalTurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsupportedAction { .. } => None,
            Self::WorkspaceRead(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

/// Runs one decision-selected action for an already running, task-owned turn.
///
/// The action is selected once. Reads go through the session's workspace safety checks; a
/// successful read or an execution failure is stored as the matching terminal turn outcome.
/// Ownership and state failures leave the task and turn unchanged.
pub fn run_goal_turn<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    let action = decision_source.choose_action(goal);
    let read_result = match action {
        WorkspaceAction::Unsupported { action } => {
            let error = GoalTurnError::UnsupportedAction { action };
            let outcome = TurnOutcome::Failure {
                detail: error.to_string(),
            };
            session
                .complete_turn(task_id, turn_id, outcome)
                .map_err(GoalTurnError::Lifecycle)?;
            return Err(error);
        }
        WorkspaceAction::ReadFile { path } => {
            match session.read_workspace_file(task_id, turn_id, &path) {
                Ok(result) => result,
                Err(error @ WorkspaceReadError::File { .. }) => {
                    let outcome = TurnOutcome::Failure {
                        detail: error.to_string(),
                    };
                    session
                        .complete_turn(task_id, turn_id, outcome)
                        .map_err(GoalTurnError::Lifecycle)?;
                    return Err(GoalTurnError::WorkspaceRead(error));
                }
                Err(error) => return Err(GoalTurnError::WorkspaceRead(error)),
            }
        }
    };
    let outcome = TurnOutcome::Success {
        result: read_result.clone(),
    };
    session
        .complete_turn(task_id, turn_id, outcome.clone())
        .map_err(GoalTurnError::Lifecycle)?;

    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        outcome,
    })
}
