//! Provider-independent action and observation contracts for one bounded task turn.

use crate::task_lifecycle::{
    TurnActivityAction, TurnActivitySummary, TurnCompletionError, TurnOutcome,
    WorkspaceCreateDirectoryError, WorkspaceGuidanceReadError, WorkspaceListError,
    WorkspaceReadError, WorkspaceReplaceError, WorkspaceSearchError, WorkspaceSearchLocation,
    WorkspaceWriteError,
};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// Stable, secret-free classification for a model decision failure.
///
/// These codes are carried through the existing `WorkspaceAction::Unsupported` string contract
/// with the reserved `decision_failure:` prefix, so existing public action variants stay intact.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionFailureCode {
    /// A decision source exhausted its bounded request budget.
    DecisionBudgetExhausted,
    /// The selected model name is invalid for its provider.
    InvalidModelName,
    /// A request could not be encoded.
    RequestEncodingFailed,
    /// A request exceeded the configured size limit.
    RequestTooLarge,
    /// A configured provider endpoint is invalid.
    InvalidEndpoint,
    /// The observations exceeded the model context limit.
    ContextTooLarge,
    /// A provider could not be reached or returned a server failure.
    ProviderUnavailable,
    /// A provider rate limited the request.
    ProviderRateLimited,
    /// A provider rejected the request with a non-rate-limit client status.
    ProviderRejected,
    /// A provider response exceeded the configured size limit.
    ResponseTooLarge,
    /// A provider response was not valid JSON.
    InvalidProviderResponse,
    /// The model response did not contain a valid supported tool call.
    InvalidModelOutput,
    /// The model supplied invalid arguments for a tool call.
    InvalidActionArguments,
    /// An unclassified decision failure; reserved for forward compatibility.
    Unknown,
}

impl DecisionFailureCode {
    /// Returns this code's stable machine-readable representation.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::DecisionBudgetExhausted => "decision_budget_exhausted",
            Self::InvalidModelName => "invalid_model_name",
            Self::RequestEncodingFailed => "request_encoding_failed",
            Self::RequestTooLarge => "request_too_large",
            Self::InvalidEndpoint => "invalid_endpoint",
            Self::ContextTooLarge => "context_too_large",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderRejected => "provider_rejected",
            Self::ResponseTooLarge => "response_too_large",
            Self::InvalidProviderResponse => "invalid_provider_response",
            Self::InvalidModelOutput => "invalid_model_output",
            Self::InvalidActionArguments => "invalid_action_arguments",
            Self::Unknown => "unknown",
        }
    }

    /// Parses a stable code returned by [`Self::as_code`].
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "decision_budget_exhausted" => Self::DecisionBudgetExhausted,
            "invalid_model_name" => Self::InvalidModelName,
            "request_encoding_failed" => Self::RequestEncodingFailed,
            "request_too_large" => Self::RequestTooLarge,
            "invalid_endpoint" => Self::InvalidEndpoint,
            "context_too_large" => Self::ContextTooLarge,
            "provider_unavailable" => Self::ProviderUnavailable,
            "provider_rate_limited" => Self::ProviderRateLimited,
            "provider_rejected" => Self::ProviderRejected,
            "response_too_large" => Self::ResponseTooLarge,
            "invalid_provider_response" => Self::InvalidProviderResponse,
            "invalid_model_output" => Self::InvalidModelOutput,
            "invalid_action_arguments" => Self::InvalidActionArguments,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    /// Parses the reserved action label used to carry a model decision failure.
    pub fn from_action_label(action: &str) -> Option<Self> {
        let mut parts = action.strip_prefix("decision_failure:")?.split(':');
        let code = Self::from_code(parts.next()?)?;
        match parts.next() {
            None => Some(code),
            Some(status)
                if matches!(
                    code,
                    Self::ProviderUnavailable | Self::ProviderRateLimited | Self::ProviderRejected
                ) && status.parse::<u16>().is_ok()
                    && parts.next().is_none() =>
            {
                Some(code)
            }
            Some(_) => None,
        }
    }

    /// Returns a safe HTTP status suffix carried by a classified provider failure label.
    pub fn http_status_from_action_label(action: &str) -> Option<u16> {
        let code = Self::from_action_label(action)?;
        if !matches!(
            code,
            Self::ProviderUnavailable | Self::ProviderRateLimited | Self::ProviderRejected
        ) {
            return None;
        }
        action
            .strip_prefix("decision_failure:")?
            .split_once(':')?
            .1
            .parse()
            .ok()
    }
}

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
    /// A host-registered extension requested by a provider with JSON arguments.
    Extension {
        name: String,
        arguments: serde_json::Value,
    },
    /// An unsupported action, returned as a clear failed observation.
    Unsupported { action: String },
}

impl WorkspaceAction {
    /// Returns the stable decision-failure code when this is a classified Brain failure.
    pub fn decision_failure_code(&self) -> Option<DecisionFailureCode> {
        let Self::Unsupported { action } = self else {
            return None;
        };
        DecisionFailureCode::from_action_label(action)
    }
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
        WorkspaceAction::Extension { .. } => "extension",
        WorkspaceAction::Unsupported { .. } => "unsupported",
    }
}

pub(crate) fn workspace_mutation_target(action: &WorkspaceAction) -> Option<&PathBuf> {
    match action {
        WorkspaceAction::CreateDirectory { path }
        | WorkspaceAction::CreateFile { path, .. }
        | WorkspaceAction::ReplaceText { path, .. }
        | WorkspaceAction::ApplyPatch { path, .. } => Some(path),
        _ => None,
    }
}

/// One ordered observation produced while a task-owned turn is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceObservation {
    /// Bounded, untrusted process output and a verified typed outcome.
    ProcessFinished {
        result: crate::contracts::process_execution::ProcessResult,
    },
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
    /// A host-owned extension completed and returned a bounded, host-supplied result.
    ExtensionCompleted { name: String, result: String },
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

    /// Returns a full content revision precondition for the next selected file mutation.
    /// Sources that do not bind edits to an earlier read keep the default unrestricted behavior.
    fn expected_file_revision(&mut self, _action: &WorkspaceAction) -> Option<String> {
        None
    }

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
    pub(crate) task_id: Uuid,
    pub(crate) turn_id: Uuid,
    pub(crate) read_result: String,
    pub(crate) observations: Vec<WorkspaceObservation>,
    pub(crate) actions_taken: usize,
    pub(crate) stop_reason: GoalTurnStopReason,
    pub(crate) outcome: Option<TurnOutcome>,
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
    Process(crate::contracts::process_execution::ProcessOutcome),
    /// The host denied this action under the selected permission mode.
    PermissionDenied { action: &'static str },
    /// This action requires a host capability the current runner does not provide.
    PermissionUnavailable { action: &'static str },
    /// The selected mode requires confirmation, but no trusted prompt channel is available.
    PermissionPromptUnavailable { action: &'static str },
    /// The trusted confirmation callback failed; the action was not dispatched.
    PermissionPromptFailed { action: &'static str },
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

impl GoalTurnError {
    /// Returns the model decision failure code carried by this error, if any.
    pub fn decision_failure_code(&self) -> Option<DecisionFailureCode> {
        let Self::UnsupportedAction { action } = self else {
            return None;
        };
        DecisionFailureCode::from_action_label(action)
    }
}

impl fmt::Display for GoalTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(outcome) => write!(formatter, "offline Cargo profile: {outcome:?}"),
            Self::PermissionDenied { action } => {
                write!(formatter, "permission denied for action '{action}'")
            }
            Self::PermissionUnavailable { action } => {
                write!(
                    formatter,
                    "required host capability is unavailable for action '{action}'"
                )
            }
            Self::PermissionPromptUnavailable { action } => {
                write!(
                    formatter,
                    "confirmation is required for action '{action}', but no prompt is available"
                )
            }
            Self::PermissionPromptFailed { action } => {
                write!(formatter, "confirmation failed for action '{action}'")
            }
            Self::UnsupportedAction { action } => match self.decision_failure_code() {
                Some(code) => match DecisionFailureCode::http_status_from_action_label(action) {
                    Some(status) => write!(
                        formatter,
                        "model decision failed: {} (HTTP {status})",
                        code.as_code()
                    ),
                    None => write!(formatter, "model decision failed: {}", code.as_code()),
                },
                None => write!(formatter, "unsupported workspace action '{action}'"),
            },
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
            Self::Process(_)
            | Self::PermissionDenied { .. }
            | Self::PermissionUnavailable { .. }
            | Self::PermissionPromptUnavailable { .. }
            | Self::PermissionPromptFailed { .. }
            | Self::UnsupportedAction { .. } => None,
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

pub(crate) fn summarize_observation(observation: &WorkspaceObservation) -> TurnActivitySummary {
    match observation {
        WorkspaceObservation::ProcessFinished { result } => TurnActivitySummary::ProcessFinished {
            succeeded: result.succeeded(),
            output_bytes: result.stdout.len() + result.stderr.len(),
        },
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
        WorkspaceObservation::TreeTextSearched { locations, .. } => {
            TurnActivitySummary::TreeTextSearched {
                location_count: locations.len(),
            }
        }
        WorkspaceObservation::FileCreated { bytes_written, .. } => {
            TurnActivitySummary::FileCreated {
                bytes_written: *bytes_written,
            }
        }
        WorkspaceObservation::DirectoryCreated { .. } => TurnActivitySummary::DirectoryCreated,
        WorkspaceObservation::TextReplaced { bytes_written, .. } => {
            TurnActivitySummary::TextReplaced {
                bytes_written: *bytes_written,
            }
        }
        WorkspaceObservation::PatchApplied { bytes_written, .. } => {
            TurnActivitySummary::TextReplaced {
                bytes_written: *bytes_written,
            }
        }
        WorkspaceObservation::FileCreationFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::CreateFile,
        },
        WorkspaceObservation::DirectoryCreationFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::CreateDirectory,
        },
        WorkspaceObservation::TextReplacementFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::ReplaceText,
        },
        WorkspaceObservation::PatchFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::ReplaceText,
        },
        WorkspaceObservation::TextSearchFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::SearchText,
        },
        WorkspaceObservation::TreeTextSearchFailed { .. } => TurnActivitySummary::ActionFailed {
            action: TurnActivityAction::SearchTreeText,
        },
        WorkspaceObservation::ExtensionCompleted { .. } => TurnActivitySummary::Completed,
        WorkspaceObservation::ActionFailed { action, .. } => {
            let action = match action {
                WorkspaceAction::OfflineCargoTest { .. } => TurnActivityAction::OfflineCargoTest,
                WorkspaceAction::ReadFile { .. } => TurnActivityAction::ReadFile,
                WorkspaceAction::ReadRange { .. } => TurnActivityAction::ReadRange,
                WorkspaceAction::ListDirectory { .. } => TurnActivityAction::ListDirectory,
                WorkspaceAction::CreateDirectory { .. } => TurnActivityAction::CreateDirectory,
                WorkspaceAction::CreateFile { .. } => TurnActivityAction::CreateFile,
                WorkspaceAction::ReplaceText { .. } => TurnActivityAction::ReplaceText,
                WorkspaceAction::ApplyPatch { .. } => TurnActivityAction::ReplaceText,
                WorkspaceAction::SearchText { .. } => TurnActivityAction::SearchText,
                WorkspaceAction::SearchTreeText { .. } => TurnActivityAction::SearchTreeText,
                WorkspaceAction::Complete { .. }
                | WorkspaceAction::Continue
                | WorkspaceAction::Cancel
                | WorkspaceAction::Extension { .. }
                | WorkspaceAction::Unsupported { .. } => TurnActivityAction::Unsupported,
            };
            TurnActivitySummary::ActionFailed { action }
        }
    }
}

pub(crate) fn summarize_observation_activity(
    observation: &WorkspaceObservation,
) -> Vec<TurnActivitySummary> {
    let mut activity = vec![summarize_observation(observation)];
    match observation {
        WorkspaceObservation::ProcessFinished { result } => {
            activity.push(TurnActivitySummary::OfflineCargoTestFinished {
                filter_hash: result.test_filter.as_deref().map(offline_test_filter_hash),
                passed: result.succeeded()
                    && result.tests_ran
                    && result.executed_tests.is_some_and(|count| count > 0),
                retry_any_filter: matches!(
                    result.outcome,
                    crate::contracts::process_execution::ProcessOutcome::NoTestsMatched
                ) && result.test_filter.is_some()
                    && !result.tests_ran
                    && result.executed_tests == Some(0)
                    && !result.truncated
                    && result.cleanup_confirmed
                    && !result.recovery_hold,
            });
        }
        WorkspaceObservation::ActionFailed {
            action: WorkspaceAction::OfflineCargoTest { test_filter },
            ..
        } => {
            activity.push(TurnActivitySummary::OfflineCargoTestFinished {
                filter_hash: test_filter.as_deref().map(offline_test_filter_hash),
                passed: false,
                retry_any_filter: false,
            });
        }
        WorkspaceObservation::ExtensionCompleted { .. } => {
            activity.push(TurnActivitySummary::ExtensionCompleted);
        }
        _ => {}
    }
    activity
}

fn offline_test_filter_hash(filter: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, filter.as_bytes());
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}
