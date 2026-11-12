use crate::task_lifecycle::{
    TaskSession, TurnActivityAction, TurnActivitySummary, TurnCompletionError, TurnOutcome,
    WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError, WorkspaceSearchError,
    WorkspaceSearchLocation, WorkspaceWriteError,
};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

const MAX_READ_RANGE_LINES: usize = 200;
const MAX_READ_RANGE_BYTES: usize = 64 * 1024;

/// One action a task-owned turn can request from its workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAction {
    /// Read one UTF-8 file under the task's workspace safety policy.
    ReadFile { path: PathBuf },
    /// Read at most 200 one-based lines from a UTF-8 file under the task's workspace safety policy.
    ReadRange {
        path: PathBuf,
        start_line: usize,
        max_lines: usize,
    },
    /// List the immediate entries of a directory under the task's workspace safety policy.
    ListDirectory { path: PathBuf },
    /// Find a literal query in the immediate regular UTF-8 files of a workspace directory.
    SearchText { directory: PathBuf, query: String },
    /// Create one UTF-8 file without replacing an existing path under the task's workspace policy.
    CreateFile { path: PathBuf, contents: String },
    /// Replace one unique exact text match in an existing UTF-8 workspace file.
    ReplaceText {
        path: PathBuf,
        match_text: String,
        replacement_text: String,
    },
    /// Finish the turn with an explicit result.
    Complete { result: String },
    /// Finish this turn and let the same task continue in a new turn.
    Continue,
    /// Cancel the active task and turn.
    Cancel,
    /// An unsupported action, returned as a clear failed observation.
    Unsupported { action: String },
}

/// One ordered observation produced while a task-owned turn is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceObservation {
    /// The contents read from one workspace file.
    FileRead { path: PathBuf, contents: String },
    /// A selected file excerpt and the actual one-based line range returned.
    FileRangeRead {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
        excerpt: String,
    },
    /// Immediate filesystem-native entry names in deterministic order.
    DirectoryListed {
        path: PathBuf,
        entries: Vec<OsString>,
    },
    /// Matching relative file paths and one-based line numbers, without matched line contents.
    TextSearched {
        directory: PathBuf,
        locations: Vec<WorkspaceSearchLocation>,
    },
    /// A file was created; its contents are not retained in the observation.
    FileCreated { path: PathBuf, bytes_written: usize },
    /// A file creation rejected under the workspace policy; contents are not retained.
    FileCreationFailed { path: PathBuf, error: GoalTurnError },
    /// Text was replaced; the matched and replacement text are not retained.
    TextReplaced { path: PathBuf, bytes_written: usize },
    /// A replacement failed; the matched and replacement text are not retained.
    TextReplacementFailed { path: PathBuf, error: GoalTurnError },
    /// A text search failed; the query and file contents are not retained.
    TextSearchFailed {
        directory: PathBuf,
        error: GoalTurnError,
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
    /// The decision source ended this turn and requested another bounded turn.
    Continued,
    /// The caller's maximum number of decisions was reached.
    ActionBudgetExhausted,
    /// A workspace action failed and the turn was recorded as failed.
    ActionFailed,
    /// The legacy one-action entry point completed its single action.
    ActionCompleted,
    /// The caller cancelled the active task and turn.
    Cancelled,
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

/// The observations, stop reason, and optional completion outcome for a task-owned turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnResult {
    task_id: Uuid,
    turn_id: Uuid,
    read_result: String,
    observations: Vec<WorkspaceObservation>,
    actions_taken: usize,
    stop_reason: GoalTurnStopReason,
    outcome: Option<TurnOutcome>,
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

    /// Returns the contents or excerpt from the first successful file read.
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

    /// Returns the success or failure outcome, or `None` when the turn was cancelled.
    pub fn outcome(&self) -> Option<&TurnOutcome> {
        self.outcome.as_ref()
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
    /// The existing workspace safety policy rejected or could not create a file.
    WorkspaceWrite(WorkspaceWriteError),
    /// The existing workspace safety policy rejected or could not replace file text.
    WorkspaceReplace(WorkspaceReplaceError),
    /// The existing workspace safety policy rejected or could not search workspace text.
    WorkspaceSearch(WorkspaceSearchError),
    /// The task or turn could not be completed or cancelled in its current session state.
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
            Self::WorkspaceWrite(error) => error.fmt(formatter),
            Self::WorkspaceReplace(error) => error.fmt(formatter),
            Self::WorkspaceSearch(error) => error.fmt(formatter),
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
            Self::WorkspaceWrite(error) => Some(error),
            Self::WorkspaceReplace(error) => Some(error),
            Self::WorkspaceSearch(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

enum ActionExecution {
    Observation(WorkspaceObservation),
    Complete(String),
    Continue,
    Cancel,
    Failed {
        observation: WorkspaceObservation,
        error: GoalTurnError,
    },
}

struct TurnCompletionData {
    activity: Vec<TurnActivitySummary>,
    actions_taken: usize,
    stop_reason: GoalTurnStopReason,
    outcome: TurnOutcome,
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
            TurnCompletionData {
                activity: vec![TurnActivitySummary::Completed],
                actions_taken: 1,
                stop_reason: GoalTurnStopReason::Completed,
                outcome: TurnOutcome::Success { result },
            },
        ),
        ActionExecution::Continue => finish_turn(
            session,
            task_id,
            turn_id,
            Vec::new(),
            TurnCompletionData {
                activity: vec![TurnActivitySummary::Continued],
                actions_taken: 1,
                stop_reason: GoalTurnStopReason::Continued,
                outcome: TurnOutcome::Success {
                    result: "continued".to_owned(),
                },
            },
        ),
        ActionExecution::Cancel => {
            cancel_turn(session, task_id, turn_id, Vec::new(), Vec::new(), 1)
        }
        ActionExecution::Observation(observation) => {
            let result = match &observation {
                WorkspaceObservation::FileRead { contents, .. } => contents.clone(),
                WorkspaceObservation::FileRangeRead { excerpt, .. } => excerpt.clone(),
                WorkspaceObservation::DirectoryListed { entries, .. } => {
                    format!("listed {} entries", entries.len())
                }
                WorkspaceObservation::TextSearched { locations, .. } => {
                    format!("found {} matching locations", locations.len())
                }
                WorkspaceObservation::FileCreated { bytes_written, .. } => {
                    format!("created file ({bytes_written} bytes)")
                }
                WorkspaceObservation::TextReplaced { bytes_written, .. } => {
                    format!("replaced text ({bytes_written} bytes)")
                }
                WorkspaceObservation::FileCreationFailed { .. }
                | WorkspaceObservation::TextReplacementFailed { .. }
                | WorkspaceObservation::TextSearchFailed { .. }
                | WorkspaceObservation::ActionFailed { .. } => String::new(),
            };
            let activity = summarize_observation(&observation);
            finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                TurnCompletionData {
                    activity: vec![activity],
                    actions_taken: 1,
                    stop_reason: GoalTurnStopReason::ActionCompleted,
                    outcome: TurnOutcome::Success { result },
                },
            )
        }
        ActionExecution::Failed { observation, error } => {
            let detail = error.to_string();
            let activity = summarize_observation(&observation);
            finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                TurnCompletionData {
                    activity: vec![activity],
                    actions_taken: 1,
                    stop_reason: GoalTurnStopReason::ActionFailed,
                    outcome: TurnOutcome::Failure { detail },
                },
            )?;
            Err(error)
        }
    }
}

/// Runs up to `max_actions` decisions for one already running, task-owned turn.
///
/// Each decision receives the original goal and every earlier observation in order. The action
/// budget counts decisions, including explicit completion. Workspace read/list/search failures
/// are returned in the observations and recorded as the turn's one failure outcome. Ownership
/// and turn-state errors return `Err` without invoking the decision source or changing lifecycle
/// state.
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
    let mut activity = Vec::new();
    let mut actions_taken = 0;
    while actions_taken < max_actions {
        let action = decision_source.choose_action(goal, &observations);
        actions_taken += 1;
        match execute_action(session, task_id, turn_id, &action)? {
            ActionExecution::Complete(result) => {
                activity.push(TurnActivitySummary::Completed);
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::Completed,
                        outcome: TurnOutcome::Success { result },
                    },
                );
            }
            ActionExecution::Continue => {
                activity.push(TurnActivitySummary::Continued);
                if actions_taken == max_actions {
                    activity.push(TurnActivitySummary::BudgetExhausted {
                        actions: actions_taken,
                    });
                    return finish_turn(
                        session,
                        task_id,
                        turn_id,
                        observations,
                        TurnCompletionData {
                            activity,
                            actions_taken,
                            stop_reason: GoalTurnStopReason::ActionBudgetExhausted,
                            outcome: TurnOutcome::Failure {
                                detail: format!(
                                    "action budget exhausted after {actions_taken} actions"
                                ),
                            },
                        },
                    );
                }
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::Continued,
                        outcome: TurnOutcome::Success {
                            result: "continued".to_owned(),
                        },
                    },
                );
            }
            ActionExecution::Cancel => {
                return cancel_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    activity,
                    actions_taken,
                );
            }
            ActionExecution::Observation(observation) => {
                activity.push(summarize_observation(&observation));
                observations.push(observation);
            }
            ActionExecution::Failed { observation, error } => {
                let detail = error.to_string();
                activity.push(summarize_observation(&observation));
                observations.push(observation);
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::ActionFailed,
                        outcome: TurnOutcome::Failure { detail },
                    },
                );
            }
        }
    }

    activity.push(TurnActivitySummary::BudgetExhausted {
        actions: actions_taken,
    });
    finish_turn(
        session,
        task_id,
        turn_id,
        observations,
        TurnCompletionData {
            activity,
            actions_taken,
            stop_reason: GoalTurnStopReason::ActionBudgetExhausted,
            outcome: TurnOutcome::Failure {
                detail: format!("action budget exhausted after {actions_taken} actions"),
            },
        },
    )
}

fn summarize_observation(observation: &WorkspaceObservation) -> TurnActivitySummary {
    match observation {
        WorkspaceObservation::FileRead { contents, .. } => TurnActivitySummary::FileRead {
            bytes_read: contents.len(),
        },
        WorkspaceObservation::FileRangeRead {
            start_line,
            end_line,
            excerpt,
            ..
        } => TurnActivitySummary::FileRangeRead {
            start_line: *start_line,
            end_line: *end_line,
            bytes_returned: excerpt.len(),
        },
        WorkspaceObservation::DirectoryListed { entries, .. } => {
            TurnActivitySummary::DirectoryListed {
                entry_count: entries.len(),
            }
        }
        WorkspaceObservation::TextSearched { locations, .. } => TurnActivitySummary::TextSearched {
            location_count: locations.len(),
        },
        WorkspaceObservation::FileCreated { bytes_written, .. } => {
            TurnActivitySummary::FileCreated {
                bytes_written: *bytes_written,
            }
        }
        WorkspaceObservation::TextReplaced { bytes_written, .. } => {
            TurnActivitySummary::TextReplaced {
                bytes_written: *bytes_written,
            }
        }
        WorkspaceObservation::FileCreationFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::CreateFile,
        },
        WorkspaceObservation::TextReplacementFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::ReplaceText,
        },
        WorkspaceObservation::TextSearchFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::SearchText,
        },
        WorkspaceObservation::ActionFailed { action, .. } => {
            let action = match action {
                WorkspaceAction::ReadFile { .. } => TurnActivityAction::ReadFile,
                WorkspaceAction::ReadRange { .. } => TurnActivityAction::ReadRange,
                WorkspaceAction::ListDirectory { .. } => TurnActivityAction::ListDirectory,
                WorkspaceAction::CreateFile { .. } => TurnActivityAction::CreateFile,
                WorkspaceAction::ReplaceText { .. } => TurnActivityAction::ReplaceText,
                WorkspaceAction::SearchText { .. } => TurnActivityAction::SearchText,
                WorkspaceAction::Complete { .. }
                | WorkspaceAction::Continue
                | WorkspaceAction::Cancel
                | WorkspaceAction::Unsupported { .. } => TurnActivityAction::Unsupported,
            };
            TurnActivitySummary::ActionFailed { action }
        }
    }
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
        WorkspaceAction::ReadRange {
            path,
            start_line,
            max_lines,
        } => {
            let result = validate_read_range(*start_line, *max_lines)
                .map_err(|message| WorkspaceReadError::File { message })
                .and_then(|()| session.read_workspace_file(task_id, turn_id, path))
                .and_then(|contents| {
                    select_read_range(&contents, *start_line, *max_lines)
                        .map_err(|message| WorkspaceReadError::File { message })
                });
            match result {
                Ok((start_line, end_line, excerpt)) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::FileRangeRead {
                        path: path.clone(),
                        start_line,
                        end_line,
                        excerpt,
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
        WorkspaceAction::CreateFile { path, contents } => {
            match session.create_workspace_file(task_id, turn_id, path, contents) {
                Ok(()) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::FileCreated {
                        path: path.clone(),
                        bytes_written: contents.len(),
                    },
                )),
                Err(error @ WorkspaceWriteError::File { .. }) => {
                    let error = GoalTurnError::WorkspaceWrite(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::FileCreationFailed {
                            path: path.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceWrite(error)),
            }
        }
        WorkspaceAction::ReplaceText {
            path,
            match_text,
            replacement_text,
        } => match session.replace_workspace_text(
            task_id,
            turn_id,
            path,
            match_text,
            replacement_text,
        ) {
            Ok(bytes_written) => Ok(ActionExecution::Observation(
                WorkspaceObservation::TextReplaced {
                    path: path.clone(),
                    bytes_written,
                },
            )),
            Err(error @ WorkspaceReplaceError::File { .. }) => {
                let error = GoalTurnError::WorkspaceReplace(error);
                Ok(ActionExecution::Failed {
                    observation: WorkspaceObservation::TextReplacementFailed {
                        path: path.clone(),
                        error: error.clone(),
                    },
                    error,
                })
            }
            Err(error) => Err(GoalTurnError::WorkspaceReplace(error)),
        },
        WorkspaceAction::SearchText { directory, query } => {
            match session.search_workspace_text(task_id, turn_id, directory, query) {
                Ok(locations) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::TextSearched {
                        directory: directory.clone(),
                        locations,
                    },
                )),
                Err(error @ WorkspaceSearchError::Search { .. }) => {
                    let error = GoalTurnError::WorkspaceSearch(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::TextSearchFailed {
                            directory: directory.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceSearch(error)),
            }
        }
        WorkspaceAction::Complete { result } => Ok(ActionExecution::Complete(result.clone())),
        WorkspaceAction::Continue => Ok(ActionExecution::Continue),
        WorkspaceAction::Cancel => Ok(ActionExecution::Cancel),
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

fn validate_read_range(start_line: usize, max_lines: usize) -> Result<(), String> {
    if start_line == 0 {
        return Err("starting line must be positive".to_owned());
    }
    if max_lines == 0 {
        return Err("line count must be positive".to_owned());
    }
    if max_lines > MAX_READ_RANGE_LINES {
        return Err("requested line count exceeds 200 lines".to_owned());
    }
    Ok(())
}

fn select_read_range(
    contents: &str,
    start_line: usize,
    max_lines: usize,
) -> Result<(usize, usize, String), String> {
    let lines = contents
        .lines()
        .skip(start_line - 1)
        .take(max_lines)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Err(format!("starting line {start_line} does not exist"));
    }

    let excerpt = lines.join("\n");
    if excerpt.len() > MAX_READ_RANGE_BYTES {
        return Err("selected excerpt exceeds 64 KiB".to_owned());
    }
    let end_line = start_line + lines.len() - 1;
    Ok((start_line, end_line, excerpt))
}

fn cancel_turn(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    observations: Vec<WorkspaceObservation>,
    mut activity: Vec<TurnActivitySummary>,
    actions_taken: usize,
) -> Result<GoalTurnResult, GoalTurnError> {
    activity.push(TurnActivitySummary::Cancelled);
    session
        .cancel_turn_with_activity(task_id, turn_id, &activity)
        .map_err(GoalTurnError::Lifecycle)?;
    let read_result = observations
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::FileRead { contents, .. } => Some(contents.clone()),
            WorkspaceObservation::FileRangeRead { excerpt, .. } => Some(excerpt.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        observations,
        actions_taken,
        stop_reason: GoalTurnStopReason::Cancelled,
        outcome: None,
    })
}

fn finish_turn(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    observations: Vec<WorkspaceObservation>,
    completion: TurnCompletionData,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .complete_turn_with_activity(
            task_id,
            turn_id,
            completion.outcome.clone(),
            &completion.activity,
        )
        .map_err(GoalTurnError::Lifecycle)?;
    let read_result = observations
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::FileRead { contents, .. } => Some(contents.clone()),
            WorkspaceObservation::FileRangeRead { excerpt, .. } => Some(excerpt.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        observations,
        actions_taken: completion.actions_taken,
        stop_reason: completion.stop_reason,
        outcome: Some(completion.outcome),
    })
}
