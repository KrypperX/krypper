//! The action, observation and decision vocabulary of a goal turn.

use crate::task_lifecycle::{
    TurnActivitySummary, TurnCompletionError, TurnOutcome, WorkspaceCreateDirectoryError,
    WorkspaceGuidanceReadError, WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError,
    WorkspaceSearchError, WorkspaceSearchLocation, WorkspaceWriteError,
};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

pub(super) const MAX_READ_RANGE_LINES: usize = 200;
pub(super) const MAX_READ_RANGE_BYTES: usize = 64 * 1024;

/// One action a task-owned turn can request from its workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceAction {
    /// Request the fixed, explicitly authorized offline Cargo profile.
    OfflineCargoTest { test_filter: Option<String> },
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
    /// Create one directory under the task's workspace safety policy.
    CreateDirectory { path: PathBuf },
    /// Find a literal query in immediate regular UTF-8 files, honoring applicable root and nested `.gitignore` glob rules and negation.
    SearchText { directory: PathBuf, query: String },
    /// Find a literal query recursively in regular UTF-8 files, honoring applicable root and nested `.gitignore` glob rules and negation, and skipping `.git` metadata directories.
    SearchTreeText { directory: PathBuf, query: String },
    /// Create one UTF-8 file without replacing an existing path under the task's workspace policy.
    CreateFile { path: PathBuf, contents: String },
    /// Replace one unique exact text match in an existing UTF-8 workspace file.
    ReplaceText {
        path: PathBuf,
        match_text: String,
        replacement_text: String,
    },
    /// Apply one bounded, exact-context unified diff to an existing UTF-8 workspace file.
    ApplyPatch { path: PathBuf, patch: String },
    /// Finish the turn with an explicit result.
    Complete { result: String },
    /// Finish this turn and let the same task continue in a new turn.
    Continue,
    /// Cancel the active task and turn.
    Cancel,
    /// An unsupported action, returned as a clear failed observation.
    Unsupported { action: String },
}

pub(crate) fn workspace_action_name(action: &WorkspaceAction) -> &'static str {
    match action {
        WorkspaceAction::OfflineCargoTest { .. } => "offline_cargo_test",
        WorkspaceAction::ReadFile { .. } => "read_file",
        WorkspaceAction::ReadRange { .. } => "read_range",
        WorkspaceAction::ListDirectory { .. } => "list_directory",
        WorkspaceAction::CreateDirectory { .. } => "create_directory",
        WorkspaceAction::SearchText { .. } => "search_text",
        WorkspaceAction::SearchTreeText { .. } => "search_tree_text",
        WorkspaceAction::CreateFile { .. } => "create_file",
        WorkspaceAction::ReplaceText { .. } => "replace_text",
        WorkspaceAction::ApplyPatch { .. } => "apply_patch",
        WorkspaceAction::Complete { .. } => "complete_task",
        WorkspaceAction::Continue => "continue",
        WorkspaceAction::Cancel => "cancel",
        WorkspaceAction::Unsupported { .. } => "unsupported",
    }
}

/// One ordered observation produced while a task-owned turn is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceObservation {
    /// Bounded, untrusted process output and a verified typed outcome.
    ProcessFinished { result: crate::ProcessResult },
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
    /// A directory was created; no directory contents are retained.
    DirectoryCreated { path: PathBuf },
    /// Matching relative file paths and one-based line numbers, without matched line contents.
    TextSearched {
        directory: PathBuf,
        locations: Vec<WorkspaceSearchLocation>,
    },
    /// Recursive matches, as workspace-relative paths and one-based lines without line contents.
    TreeTextSearched {
        directory: PathBuf,
        locations: Vec<WorkspaceSearchLocation>,
    },
    /// A file was created; its contents are not retained in the observation.
    FileCreated { path: PathBuf, bytes_written: usize },
    /// A file creation rejected under the workspace policy; contents are not retained.
    FileCreationFailed { path: PathBuf, error: GoalTurnError },
    /// A directory creation rejected under the workspace policy.
    DirectoryCreationFailed { path: PathBuf, error: GoalTurnError },
    /// Text was replaced; the matched and replacement text are not retained.
    TextReplaced { path: PathBuf, bytes_written: usize },
    /// A replacement failed; the matched and replacement text are not retained.
    TextReplacementFailed { path: PathBuf, error: GoalTurnError },
    /// A patch was completely applied and read back; evidence contains only bounded hunk text.
    PatchApplied {
        path: PathBuf,
        bytes_written: usize,
        before_diff: String,
        after_diff: String,
    },
    /// A patch failed; its contents are not retained.
    PatchFailed { path: PathBuf, error: GoalTurnError },
    /// A text search failed; the query and file contents are not retained.
    TextSearchFailed {
        directory: PathBuf,
        error: GoalTurnError,
    },
    /// A recursive text search failed; the query and file contents are not retained.
    TreeTextSearchFailed {
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

    /// Returns whether this source builds model context from the task's root project guidance.
    fn needs_root_project_guidance(&self) -> bool {
        false
    }

    /// Selects an action with bounded root project guidance when the source requests it.
    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        _guidance: Option<&str>,
    ) -> WorkspaceAction {
        self.choose_action(goal, observations)
    }
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
    pub(super) task_id: Uuid,
    pub(super) turn_id: Uuid,
    pub(super) read_result: String,
    pub(super) observations: Vec<WorkspaceObservation>,
    pub(super) actions_taken: usize,
    pub(super) stop_reason: GoalTurnStopReason,
    pub(super) outcome: Option<TurnOutcome>,
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
    /// The fixed process profile did not produce verified success.
    Process(crate::ProcessOutcome),
    /// The decision source requested an unsupported action.
    UnsupportedAction { action: String },
    /// The existing workspace safety policy rejected or could not perform a file read.
    WorkspaceRead(WorkspaceReadError),
    /// Root project guidance could not be read safely before a model decision.
    RootProjectGuidance(WorkspaceGuidanceReadError),
    /// The existing workspace safety policy rejected or could not list a directory.
    WorkspaceList(WorkspaceListError),
    /// The existing workspace safety policy rejected or could not create a directory.
    WorkspaceDirectory(WorkspaceCreateDirectoryError),
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
            Self::Process(outcome) => write!(formatter, "offline Cargo profile: {outcome:?}"),
            Self::UnsupportedAction { action } => {
                write!(formatter, "unsupported workspace action '{action}'")
            }
            Self::WorkspaceRead(error) => error.fmt(formatter),
            Self::RootProjectGuidance(error) => {
                write!(formatter, "project guidance read failed: {error}")
            }
            Self::WorkspaceList(error) => error.fmt(formatter),
            Self::WorkspaceDirectory(error) => error.fmt(formatter),
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
            Self::Process(_) | Self::UnsupportedAction { .. } => None,
            Self::WorkspaceRead(error) => Some(error),
            Self::RootProjectGuidance(error) => Some(error),
            Self::WorkspaceList(error) => Some(error),
            Self::WorkspaceDirectory(error) => Some(error),
            Self::WorkspaceWrite(error) => Some(error),
            Self::WorkspaceReplace(error) => Some(error),
            Self::WorkspaceSearch(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
        }
    }
}

pub(super) enum ActionExecution {
    Observation(WorkspaceObservation),
    Complete(String),
    Continue,
    Cancel,
    Failed {
        observation: WorkspaceObservation,
        error: GoalTurnError,
    },
}

pub(super) struct TurnCompletionData {
    pub(super) activity: Vec<TurnActivitySummary>,
    pub(super) actions_taken: usize,
    pub(super) stop_reason: GoalTurnStopReason,
    pub(super) outcome: TurnOutcome,
}
