//! Execution of one decision-selected workspace action.

use super::types::{
    ActionExecution, GoalTurnError, WorkspaceAction, WorkspaceObservation, MAX_READ_RANGE_BYTES,
    MAX_READ_RANGE_LINES,
};
use crate::task_lifecycle::{
    TaskSession, TurnActivityAction, TurnActivitySummary, WorkspaceCreateDirectoryError,
    WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError, WorkspaceSearchError,
    WorkspaceWriteError,
};
use uuid::Uuid;

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

pub(super) fn execute_action(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    action: &WorkspaceAction,
) -> Result<ActionExecution, GoalTurnError> {
    match action {
        WorkspaceAction::OfflineCargoTest { test_filter } => {
            let result = session
                .run_offline_cargo_tests(task_id, turn_id, test_filter.as_deref())
                .map_err(GoalTurnError::WorkspaceRead)?;
            let error = GoalTurnError::Process(result.outcome);
            let succeeded = result.succeeded();
            let observation = WorkspaceObservation::ProcessFinished { result };
            Ok(if succeeded {
                ActionExecution::Observation(observation)
            } else {
                ActionExecution::Failed { observation, error }
            })
        }
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
        WorkspaceAction::CreateDirectory { path } => {
            match session.create_workspace_directory(task_id, turn_id, path) {
                Ok(()) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::DirectoryCreated { path: path.clone() },
                )),
                Err(error @ WorkspaceCreateDirectoryError::Directory { .. }) => {
                    let error = GoalTurnError::WorkspaceDirectory(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::DirectoryCreationFailed {
                            path: path.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceDirectory(error)),
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
        WorkspaceAction::ApplyPatch { path, patch } => {
            match session.apply_workspace_patch(task_id, turn_id, path, patch) {
                Ok(evidence) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::PatchApplied {
                        path: path.clone(),
                        bytes_written: evidence.bytes_written,
                        before_diff: evidence.before_diff,
                        after_diff: evidence.after_diff,
                    },
                )),
                Err(error @ WorkspaceReplaceError::File { .. }) => {
                    let error = GoalTurnError::WorkspaceReplace(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::PatchFailed {
                            path: path.clone(),
                            error: error.clone(),
                        },
                        error,
                    })
                }
                Err(error) => Err(GoalTurnError::WorkspaceReplace(error)),
            }
        }
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
        WorkspaceAction::SearchTreeText { directory, query } => {
            match session.search_workspace_tree_text(task_id, turn_id, directory, query) {
                Ok(locations) => Ok(ActionExecution::Observation(
                    WorkspaceObservation::TreeTextSearched {
                        directory: directory.clone(),
                        locations,
                    },
                )),
                Err(error @ WorkspaceSearchError::Search { .. }) => {
                    let error = GoalTurnError::WorkspaceSearch(error);
                    Ok(ActionExecution::Failed {
                        observation: WorkspaceObservation::TreeTextSearchFailed {
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
        WorkspaceAction::Extension { name, .. } => {
            let error = GoalTurnError::UnsupportedAction {
                action: name.clone(),
            };
            Ok(ActionExecution::Failed {
                observation: WorkspaceObservation::ActionFailed {
                    action: WorkspaceAction::Unsupported {
                        action: name.clone(),
                    },
                    error: error.clone(),
                },
                error,
            })
        }
        WorkspaceAction::Unsupported { action: name } => {
            // Registered extensions use the argument-aware variant before this
            // dispatcher. Plain unsupported actions must never gain authority.
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
pub(super) fn validate_read_range(start_line: usize, max_lines: usize) -> Result<(), String> {
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
pub(super) fn select_read_range(
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
