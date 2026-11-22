//! Translation of provider tool calls into workspace actions.

use crate::agent_loop::WorkspaceAction;
use crate::tools::MAX_PATCH_BYTES;
use serde_json::{Map, Value};
use std::path::PathBuf;

use super::errors::*;
use super::limits::*;
use super::providers::*;

pub(crate) fn action_from_provider_call(
    provider: ModelProvider,
    name: &str,
    arguments: &Value,
) -> Result<WorkspaceAction, ModelError> {
    action_from_call(name, arguments).map_err(|error| match error {
        ModelError::InvalidToolCall(message) => {
            ModelError::ProviderInvalidToolCall(provider, message)
        }
        ModelError::UnsupportedTool { name, arguments } => ModelError::ProviderUnsupportedTool {
            provider,
            name,
            arguments,
        },
        ModelError::InvalidArguments(message) => {
            ModelError::ProviderInvalidArguments(provider, message)
        }
        other => other,
    })
}

pub(crate) fn action_from_call(
    name: &str,
    arguments: &Value,
) -> Result<WorkspaceAction, ModelError> {
    match name {
        "offline_cargo_test" => {
            let arguments = checked_arguments(arguments, &["test_filter"], &[])?;
            let test_filter = arguments
                .get("test_filter")
                .map(|_| string_argument(arguments, "test_filter"))
                .transpose()?;
            crate::tools::process::validate_test_filter(test_filter)
                .map_err(ModelError::InvalidArguments)?;
            Ok(WorkspaceAction::OfflineCargoTest {
                test_filter: test_filter.map(str::to_owned),
            })
        }
        "read_file" => {
            let arguments = checked_arguments(arguments, &["path"], &["path"])?;
            Ok(WorkspaceAction::ReadFile {
                path: path_argument(arguments, "path")?,
            })
        }
        "read_range" => {
            let arguments = checked_arguments(
                arguments,
                &["path", "start_line", "max_lines"],
                &["path", "start_line", "max_lines"],
            )?;
            let start_line = positive_integer(arguments, "start_line")?;
            let max_lines = positive_integer(arguments, "max_lines")?;
            if max_lines > MAX_READ_RANGE_LINES {
                return Err(ModelError::InvalidArguments(
                    "max_lines must not exceed 200",
                ));
            }
            Ok(WorkspaceAction::ReadRange {
                path: path_argument(arguments, "path")?,
                start_line,
                max_lines,
            })
        }
        "list_directory" => {
            let arguments = checked_arguments(arguments, &["path"], &["path"])?;
            Ok(WorkspaceAction::ListDirectory {
                path: path_argument(arguments, "path")?,
            })
        }
        "create_directory" => {
            let arguments = checked_arguments(arguments, &["path"], &["path"])?;
            Ok(WorkspaceAction::CreateDirectory {
                path: path_argument(arguments, "path")?,
            })
        }
        "search_text" | "search_tree_text" => {
            let arguments =
                checked_arguments(arguments, &["directory", "query"], &["directory", "query"])?;
            let directory = path_argument(arguments, "directory")?;
            let query = string_argument(arguments, "query")?;
            if query.is_empty() || query.len() > MAX_SEARCH_QUERY_BYTES {
                return Err(ModelError::InvalidArguments(
                    "query must be nonempty and at most 4 KiB",
                ));
            }
            if name == "search_text" {
                Ok(WorkspaceAction::SearchText {
                    directory,
                    query: query.to_owned(),
                })
            } else {
                Ok(WorkspaceAction::SearchTreeText {
                    directory,
                    query: query.to_owned(),
                })
            }
        }
        "create_file" => {
            let arguments =
                checked_arguments(arguments, &["path", "contents"], &["path", "contents"])?;
            Ok(WorkspaceAction::CreateFile {
                path: path_argument(arguments, "path")?,
                contents: string_argument(arguments, "contents")?.to_owned(),
            })
        }
        "replace_text" => {
            let arguments = checked_arguments(
                arguments,
                &["path", "match_text", "replacement_text"],
                &["path", "match_text", "replacement_text"],
            )?;
            let match_text = string_argument(arguments, "match_text")?;
            if match_text.is_empty() {
                return Err(ModelError::InvalidArguments("match_text must not be empty"));
            }
            Ok(WorkspaceAction::ReplaceText {
                path: path_argument(arguments, "path")?,
                match_text: match_text.to_owned(),
                replacement_text: string_argument(arguments, "replacement_text")?.to_owned(),
            })
        }
        "apply_patch" => {
            let arguments = checked_arguments(arguments, &["path", "patch"], &["path", "patch"])?;
            let patch = string_argument(arguments, "patch")?;
            if patch.is_empty() || patch.len() > MAX_PATCH_BYTES {
                return Err(ModelError::InvalidArguments(
                    "patch must be nonempty and at most 32 KiB",
                ));
            }
            Ok(WorkspaceAction::ApplyPatch {
                path: path_argument(arguments, "path")?,
                patch: patch.to_owned(),
            })
        }
        "complete_task" => {
            let arguments = checked_arguments(arguments, &["result"], &["result"])?;
            let result = string_argument(arguments, "result")?;
            if result.trim().is_empty() {
                return Err(ModelError::InvalidArguments("result must not be empty"));
            }
            Ok(WorkspaceAction::Complete {
                result: result.to_owned(),
            })
        }
        _ => Err(ModelError::UnsupportedTool {
            name: name.to_owned(),
            arguments: arguments.clone(),
        }),
    }
}

/// Map a provider decision error back to the bounded action vocabulary.
///
/// Unsupported tool calls keep their host-requested name and JSON arguments so
/// the Harness can apply its own registry and capability checks. All other
/// failures remain ordinary failed actions.
pub(crate) fn action_from_error(error: ModelError) -> WorkspaceAction {
    match error {
        ModelError::UnsupportedTool { name, arguments }
        | ModelError::ProviderUnsupportedTool {
            name, arguments, ..
        } => WorkspaceAction::Extension { name, arguments },
        error => WorkspaceAction::Unsupported {
            action: error.to_string(),
        },
    }
}

pub(crate) fn checked_arguments<'a>(
    arguments: &'a Value,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a Map<String, Value>, ModelError> {
    let object = arguments
        .as_object()
        .ok_or(ModelError::InvalidArguments("arguments must be an object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ModelError::InvalidArguments(
            "contains an unsupported argument",
        ));
    }
    if required.iter().any(|key| !object.contains_key(*key)) {
        return Err(ModelError::InvalidArguments(
            "is missing a required argument",
        ));
    }
    Ok(object)
}

pub(crate) fn string_argument<'a>(
    arguments: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, ModelError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or(ModelError::InvalidArguments(
            "string arguments must be strings",
        ))
}

pub(crate) fn path_argument(
    arguments: &Map<String, Value>,
    name: &'static str,
) -> Result<PathBuf, ModelError> {
    let path = string_argument(arguments, name)?;
    if path.is_empty() {
        return Err(ModelError::InvalidArguments("paths must not be empty"));
    }
    Ok(PathBuf::from(path))
}

pub(crate) fn positive_integer(
    arguments: &Map<String, Value>,
    name: &'static str,
) -> Result<usize, ModelError> {
    let value = arguments
        .get(name)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or(ModelError::InvalidArguments(
            "line numbers and counts must be positive integers",
        ))?;
    usize::try_from(value).map_err(|_| {
        ModelError::InvalidArguments("line number or count is outside the supported range")
    })
}

#[cfg(test)]
mod decision_error_mapping_tests {
    use super::{action_from_error, ModelError, WorkspaceAction};
    use serde_json::json;

    #[test]
    fn provider_and_local_failures_remain_failed_actions() {
        for error in [
            ModelError::DecisionBudgetExhausted,
            ModelError::RequestEncoding,
            ModelError::RequestTooLarge,
            ModelError::InvalidEndpoint,
            ModelError::ObservationsTooLarge,
            ModelError::ServiceUnavailable,
            ModelError::Transport("connection refused".to_owned()),
            ModelError::ResponseTooLarge,
            ModelError::HttpStatus(429),
            ModelError::InvalidJson,
            ModelError::InvalidToolCall("invalid"),
            ModelError::InvalidArguments("invalid"),
        ] {
            assert!(
                matches!(
                    action_from_error(error),
                    WorkspaceAction::Unsupported { .. }
                ),
                "decision failures must not become extension requests"
            );
        }
    }

    #[test]
    fn unknown_tools_preserve_extension_dispatch_arguments() {
        let arguments = json!({"key": "value"});
        let action = action_from_error(ModelError::UnsupportedTool {
            name: "custom_tool".to_owned(),
            arguments: arguments.clone(),
        });
        assert_eq!(
            action,
            WorkspaceAction::Extension {
                name: "custom_tool".to_owned(),
                arguments,
            }
        );
    }

    #[test]
    fn transport_errors_currently_expose_unstructured_text() {
        let action = action_from_error(ModelError::Transport("connection refused".to_owned()));
        assert_eq!(
            action,
            WorkspaceAction::Unsupported {
                action: "Ollama request failed: connection refused".to_owned(),
            }
        );
    }
}
