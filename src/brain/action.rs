//! Translation of provider tool calls into workspace actions.

use crate::agent_loop::{DecisionFailureCode, WorkspaceAction};
use crate::tools::MAX_PATCH_BYTES;
use serde_json::{Map, Value};
use std::path::PathBuf;

use super::errors::*;
use super::limits::*;
use super::providers::*;
use super::tool_set::AdvertisedToolNames;

pub(crate) fn action_from_provider_call(
    provider: ModelProvider,
    name: &str,
    arguments: &Value,
    advertised_tools: &AdvertisedToolNames,
) -> Result<WorkspaceAction, ModelError> {
    action_from_call(name, arguments, advertised_tools).map_err(|error| match error {
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
    advertised_tools: &AdvertisedToolNames,
) -> Result<WorkspaceAction, ModelError> {
    if !advertised_tools.contains(name) {
        return Err(ModelError::InvalidToolCall("tool was not advertised"));
    }

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
        error => {
            let status = match &error {
                ModelError::HttpStatus(status) | ModelError::ProviderHttpStatus(_, status) => {
                    Some(*status)
                }
                _ => None,
            };
            let code = error
                .decision_failure_code()
                .unwrap_or(DecisionFailureCode::Unknown);
            let label = match status {
                Some(status) => format!("decision_failure:{}:{status}", code.as_code()),
                None => format!("decision_failure:{}", code.as_code()),
            };
            WorkspaceAction::Unsupported { action: label }
        }
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
    use super::{
        action_from_error, DecisionFailureCode, ModelError, ModelProvider, WorkspaceAction,
    };
    use serde_json::json;

    #[test]
    fn decision_failures_use_stable_machine_readable_codes() {
        let cases = [
            (
                ModelError::DecisionBudgetExhausted,
                "decision_budget_exhausted",
            ),
            (
                ModelError::InvalidModelName("invalid model"),
                "invalid_model_name",
            ),
            (ModelError::RequestEncoding, "request_encoding_failed"),
            (ModelError::RequestTooLarge, "request_too_large"),
            (ModelError::InvalidEndpoint, "invalid_endpoint"),
            (ModelError::ObservationsTooLarge, "context_too_large"),
            (ModelError::RepositoryContextTooLarge, "context_too_large"),
            (ModelError::ServiceUnavailable, "provider_unavailable"),
            (
                ModelError::Transport("connection refused".to_owned()),
                "provider_unavailable",
            ),
            (ModelError::ResponseTooLarge, "response_too_large"),
            (ModelError::HttpStatus(429), "provider_rate_limited"),
            (ModelError::HttpStatus(408), "provider_unavailable"),
            (ModelError::HttpStatus(425), "provider_unavailable"),
            (ModelError::HttpStatus(503), "provider_unavailable"),
            (ModelError::HttpStatus(401), "provider_rejected"),
            (ModelError::InvalidJson, "invalid_provider_response"),
            (
                ModelError::InvalidToolCall("invalid"),
                "invalid_model_output",
            ),
            (
                ModelError::InvalidArguments("invalid"),
                "invalid_action_arguments",
            ),
            (
                ModelError::ProviderTransport(ModelProvider::Anthropic),
                "provider_unavailable",
            ),
            (
                ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 429),
                "provider_rate_limited",
            ),
            (
                ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 408),
                "provider_unavailable",
            ),
            (
                ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 425),
                "provider_unavailable",
            ),
            (
                ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 503),
                "provider_unavailable",
            ),
            (
                ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 401),
                "provider_rejected",
            ),
            (
                ModelError::ProviderResponseTooLarge(ModelProvider::Anthropic),
                "response_too_large",
            ),
            (
                ModelError::ProviderInvalidJson(ModelProvider::Anthropic),
                "invalid_provider_response",
            ),
            (
                ModelError::ProviderInvalidToolCall(ModelProvider::Anthropic, "invalid"),
                "invalid_model_output",
            ),
            (
                ModelError::ProviderInvalidArguments(ModelProvider::Anthropic, "invalid"),
                "invalid_action_arguments",
            ),
        ];

        for (error, code) in cases {
            let expected_code = DecisionFailureCode::from_code(code).expect("known code");
            let status = match &error {
                ModelError::HttpStatus(status) | ModelError::ProviderHttpStatus(_, status) => {
                    Some(*status)
                }
                _ => None,
            };
            let action = match status {
                Some(status) => format!("decision_failure:{code}:{status}"),
                None => format!("decision_failure:{code}"),
            };
            assert_eq!(
                action_from_error(error),
                WorkspaceAction::Unsupported {
                    action: action.clone()
                },
                "decision failure should keep its stable class"
            );
            assert_eq!(expected_code.as_code(), code);
            assert_eq!(
                DecisionFailureCode::from_action_label(&action),
                Some(expected_code)
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

        let provider_arguments = json!({"provider_key": "value"});
        let provider_action = action_from_error(ModelError::ProviderUnsupportedTool {
            provider: ModelProvider::Anthropic,
            name: "custom_provider_tool".to_owned(),
            arguments: provider_arguments.clone(),
        });
        assert_eq!(
            provider_action,
            WorkspaceAction::Extension {
                name: "custom_provider_tool".to_owned(),
                arguments: provider_arguments,
            }
        );
    }

    #[test]
    fn transport_error_details_are_redacted_from_display_and_action() {
        let sentinel = "sk_test_do_not_expose_91f4";
        assert_eq!(
            action_from_error(ModelError::Transport(sentinel.to_owned())),
            WorkspaceAction::Unsupported {
                action: "decision_failure:provider_unavailable".to_owned(),
            }
        );
        let errors = [
            ModelError::DecisionBudgetExhausted,
            ModelError::InvalidModelName("invalid model"),
            ModelError::RequestEncoding,
            ModelError::RequestTooLarge,
            ModelError::InvalidEndpoint,
            ModelError::ObservationsTooLarge,
            ModelError::ServiceUnavailable,
            ModelError::Transport(sentinel.to_owned()),
            ModelError::ResponseTooLarge,
            ModelError::HttpStatus(401),
            ModelError::InvalidJson,
            ModelError::InvalidToolCall("invalid tool call"),
            ModelError::UnsupportedTool {
                name: sentinel.to_owned(),
                arguments: json!({"token": sentinel}),
            },
            ModelError::InvalidArguments("invalid arguments"),
            ModelError::ProviderTransport(ModelProvider::Anthropic),
            ModelError::ProviderHttpStatus(ModelProvider::Anthropic, 401),
            ModelError::ProviderResponseTooLarge(ModelProvider::Anthropic),
            ModelError::ProviderInvalidJson(ModelProvider::Anthropic),
            ModelError::ProviderInvalidToolCall(ModelProvider::Anthropic, "invalid tool call"),
            ModelError::ProviderUnsupportedTool {
                provider: ModelProvider::Anthropic,
                name: sentinel.to_owned(),
                arguments: json!({"token": sentinel}),
            },
            ModelError::ProviderInvalidArguments(ModelProvider::Anthropic, "invalid arguments"),
        ];
        for error in errors {
            assert!(
                !error.to_string().contains(sentinel),
                "{} must not expose the sentinel",
                error
                    .decision_failure_code()
                    .map_or("unsupported tool", |code| code.as_code())
            );
        }
        assert_eq!(
            ModelError::UnsupportedTool {
                name: sentinel.to_owned(),
                arguments: json!({}),
            }
            .decision_failure_code(),
            None
        );
    }
}
