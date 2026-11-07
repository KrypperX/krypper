use crate::task_lifecycle::{
    TaskSession, TurnCompletionError, TurnOutcome, WorkspaceListError, WorkspaceReadError,
};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// One action a task-owned turn can request from its workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAction {
    /// Read one UTF-8 file under the task's workspace safety policy.
    ReadFile { path: PathBuf },
    /// List the immediate entries of a directory under the task's workspace safety policy.
    ListDirectory { path: PathBuf },
    /// Finish the turn with an explicit result.
    Complete { result: String },
    /// An unsupported action, returned as a clear failed observation.
    Unsupported { action: String },
}

/// One ordered observation produced while a task-owned turn is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceObservation {
    /// The contents read from one workspace file.
    FileRead { path: PathBuf, contents: String },
    /// Immediate filesystem-native entry names in deterministic order.
    DirectoryListed {
        path: PathBuf,
        entries: Vec<OsString>,
    },
    /// A requested action that failed under the existing workspace policy.
    ActionFailed {
        action: WorkspaceAction,
        error: GoalTurnError,
    },
}

/// Why a task-owned turn stopped running its action loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalTurnStopReason {
    /// The decision source explicitly completed the turn.
    Completed,
    /// The caller's maximum number of decisions was reached.
    ActionBudgetExhausted,
    /// A workspace action failed and the turn was recorded as failed.
    ActionFailed,
    /// The legacy one-action entry point completed its single action.
    ActionCompleted,
}

/// A replaceable source that chooses an action from the goal and prior observations.
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

/// A replaceable source that chooses an action from the goal and prior observations.
pub trait BoundedDecisionSource {
    /// Selects an action using the original goal and ordered earlier observations.
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction;
}

impl<F> BoundedDecisionSource for F
where
    F: FnMut(&str, &[WorkspaceObservation]) -> WorkspaceAction,
{
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self(goal, observations)
    }
}

/// The observations, stop reason, and matching terminal outcome for a task-owned turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnResult {
    task_id: Uuid,
    turn_id: Uuid,
    read_result: String,
    observations: Vec<WorkspaceObservation>,
    actions_taken: usize,
    stop_reason: GoalTurnStopReason,
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

    /// Returns the first file's contents for callers of the original one-read API.
    ///
    /// New callers should use [`Self::observations`] to inspect every read and listing.
    pub fn read_result(&self) -> &str {
        &self.read_result
    }

    /// Returns the ordered results of successful and failed workspace actions.
    pub fn observations(&self) -> &[WorkspaceObservation] {
        &self.observations
    }

    /// Returns the number of decisions made, including explicit completion.
    pub fn actions_taken(&self) -> usize {
        self.actions_taken
    }

    /// Returns why the loop stopped.
    pub fn stop_reason(&self) -> GoalTurnStopReason {
        self.stop_reason
    }

    /// Returns the outcome recorded on the completed turn.
    pub fn outcome(&self) -> &TurnOutcome {
        &self.outcome
    }
}

/// An action, workspace, or lifecycle error raised while running a task-owned turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTurnError {
    /// The decision source requested an unsupported action.
    UnsupportedAction { action: String },
    /// The existing workspace safety policy rejected or could not perform a file read.
    WorkspaceRead(WorkspaceReadError),
    /// The existing workspace safety policy rejected or could not list a directory.
    WorkspaceList(WorkspaceListError),
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
            Self::WorkspaceList(error) => error.fmt(formatter),
            Self::Lifecycle(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GoalTurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsupportedAction { .. } => None,
            Self::WorkspaceRead(error) => Some(error),
            Self::WorkspaceList(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

enum ActionExecution {
    Observation(WorkspaceObservation),
    Complete(String),
    Failed {
        observation: WorkspaceObservation,
        error: GoalTurnError,
    },
}

/// Runs one decision-selected action for an already running, task-owned turn.
///
/// This preserves the original one-action API. Use [`run_goal_turn_with_budget`] when the
/// decision source should receive prior observations and the caller needs to set an action cap.
pub fn run_goal_turn<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(GoalTurnError::WorkspaceRead)?;

    let action = decision_source.choose_action(goal);
    match execute_action(session, task_id, turn_id, &action)? {
        ActionExecution::Complete(result) => finish_turn(
            session,
            task_id,
            turn_id,
            Vec::new(),
            1,
            GoalTurnStopReason::Completed,
            TurnOutcome::Success { result },
        ),
        ActionExecution::Observation(observation) => {
            let result = match &observation {
                WorkspaceObservation::FileRead { contents, .. } => contents.clone(),
                WorkspaceObservation::DirectoryListed { entries, .. } => {
                    format!("listed {} entries", entries.len())
                }
                WorkspaceObservation::ActionFailed { .. } => String::new(),
            };
            finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                1,
                GoalTurnStopReason::ActionCompleted,
                TurnOutcome::Success { result },
            )
        }
        ActionExecution::Failed { observation, error } => {
            let detail = error.to_string();
            finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                1,
                GoalTurnStopReason::ActionFailed,
                TurnOutcome::Failure { detail },
            )?;
            Err(error)
        }
    }
}

/// Runs up to `max_actions` decisions for one already running, task-owned turn.
///
/// Each decision receives the original goal and every earlier observation in order. The action
/// budget counts decisions, including explicit completion. Workspace read/list failures are
/// returned in the observations and recorded as the turn's one failure outcome. Ownership and
/// turn-state errors return `Err` without invoking the decision source or changing lifecycle state.
pub fn run_goal_turn_with_budget<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(GoalTurnError::WorkspaceRead)?;

    let mut observations = Vec::new();
    let mut actions_taken = 0;
    while actions_taken < max_actions {
        let action = decision_source.choose_action(goal, &observations);
        actions_taken += 1;
        match execute_action(session, task_id, turn_id, &action)? {
            ActionExecution::Complete(result) => {
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    actions_taken,
                    GoalTurnStopReason::Completed,
                    TurnOutcome::Success { result },
                );
            }
            ActionExecution::Observation(observation) => observations.push(observation),
            ActionExecution::Failed { observation, error } => {
                let detail = error.to_string();
                observations.push(observation);
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    actions_taken,
                    GoalTurnStopReason::ActionFailed,
                    TurnOutcome::Failure { detail },
                );
            }
        }
    }

    finish_turn(
        session,
        task_id,
        turn_id,
        observations,
        actions_taken,
        GoalTurnStopReason::ActionBudgetExhausted,
        TurnOutcome::Failure {
            detail: format!("action budget exhausted after {actions_taken} actions"),
        },
    )
}

fn execute_action(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    action: &WorkspaceAction,
) -> Result<ActionExecution, GoalTurnError> {
    match action {
        WorkspaceAction::ReadFile { path } => {
            match session.read_workspace_file(task_id, turn_id, path) {
                Ok(contents) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::FileRead {
                        path: path.clone(),
                        contents,
                    },
                )),
                Err(error @ WorkspaceReadError::File { .. }) => {
                    let error = GoalTurnError::WorkspaceRead(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::ActionFailed {
                            action: action.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceRead(error)),
            }
        }
        WorkspaceAction::ListDirectory { path } => {
            match session.list_workspace_directory(task_id, turn_id, path) {
                Ok(entries) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::DirectoryListed {
                        path: path.clone(),
                        entries,
                    },
                )),
                Err(error @ WorkspaceListError::Directory { .. }) => {
                    let error = GoalTurnError::WorkspaceList(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::ActionFailed {
                            action: action.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceList(error)),
            }
        }
        WorkspaceAction::Complete { result } => Ok(ActionExecution::Complete(result.clone())),
        WorkspaceAction::Unsupported { action: name } => {
            let error = GoalTurnError::UnsupportedAction {
                action: name.clone(),
            };
            Ok(ActionExecution::Failed {
                observation: WorkspaceObservation::ActionFailed {
                    action: action.clone(),
                    error: error.clone(),
                },
                error,
            })
        }
    }
}

fn finish_turn(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    observations: Vec<WorkspaceObservation>,
    actions_taken: usize,
    stop_reason: GoalTurnStopReason,
    outcome: TurnOutcome,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .complete_turn(task_id, turn_id, outcome.clone())
        .map_err(GoalTurnError::Lifecycle)?;
    let read_result = observations
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::FileRead { contents, .. } => Some(contents.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        observations,
        actions_taken,
        stop_reason,
        outcome,
    })
}
