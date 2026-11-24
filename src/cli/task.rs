//! The `task` command: option parsing, model selection wiring and the turn loop output.

use super::checkpoint_files::{
    read_task_checkpoint, validate_checkpoint_output_path, write_task_checkpoint,
};
use super::help::{contains_help_before_terminator, usage_error, write_task_help};
use super::model_selection::{
    configured_model_selection, parse_provider_selection, prepare_model_source_for_selection,
    ProviderSelection,
};
use super::outcome::{write_observation, write_turn_outcome};
use super::task_args::parse_task_action;
#[path = "verified_task.rs"]
mod verified_task;
use self::verified_task::{
    normalize_verification_path, run_verified_turn, VerifiedTaskControl, VerifiedTaskReport,
};
use super::MAX_FILE_SIZE;
use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::brain::ModelDecisionSource;
use crate::brain::{ModelProvider, OLLAMA_ADDRESS};
use crate::harness::context::{MAX_REPOSITORY_CONTEXT_BYTES, MAX_REPOSITORY_CONTEXT_JSON_BYTES};
use crate::harness::execution::workspace_action_name;
use crate::harness::recovery::{
    prepare_checkpoint_for_pause, prepare_resumed_session, ResumeError,
};
use crate::harness::verification::{reconcile_task_state, MAX_COMPLETION_CHECKS};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use serde_json::Value;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

// Leave room for the action command and path beside a maximum-size file body.
const MAX_TASK_ACTION_LINE_BYTES: usize = MAX_FILE_SIZE + 4096;

fn read_repository_context_json(path: &Path) -> Result<Value, String> {
    let file = std::fs::File::open(path).map_err(|cause| {
        format!(
            "cannot read repository context file '{}': {cause}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_REPOSITORY_CONTEXT_JSON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|cause| {
            format!(
                "cannot read repository context file '{}': {cause}",
                path.display()
            )
        })?;
    if bytes.len() > MAX_REPOSITORY_CONTEXT_JSON_BYTES {
        return Err(format!(
            "repository context JSON must not exceed {MAX_REPOSITORY_CONTEXT_JSON_BYTES} bytes"
        ));
    }

    let context: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "repository context file must contain valid JSON".to_owned())?;
    if !context.is_object() {
        return Err("repository context JSON must be an object".to_owned());
    }
    let context_entries = match (
        context.get("snippets").filter(|value| value.is_array()),
        context.get("modules").filter(|value| value.is_array()),
    ) {
        (Some(entries), None) | (None, Some(entries)) => entries,
        _ => {
            return Err(
                "repository context JSON must contain exactly one snippets or modules array"
                    .to_owned(),
            )
        }
    };
    let Some(context_bytes) = context.get("bytes").and_then(Value::as_u64) else {
        return Err("repository context JSON must include a byte count".to_owned());
    };
    if context_bytes > MAX_REPOSITORY_CONTEXT_BYTES as u64 {
        return Err(format!(
            "repository context byte count must not exceed {MAX_REPOSITORY_CONTEXT_BYTES}"
        ));
    }
    let measured_bytes = serde_json::to_vec(context_entries)
        .map_err(|_| "repository context entries could not be encoded".to_owned())?
        .len();
    if measured_bytes > MAX_REPOSITORY_CONTEXT_BYTES {
        return Err(format!(
            "repository context entries must not exceed {MAX_REPOSITORY_CONTEXT_BYTES} bytes"
        ));
    }
    if context_bytes != measured_bytes as u64 {
        return Err("repository context byte count does not match its entries".to_owned());
    }
    Ok(context)
}

pub(super) fn run_task(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    run_task_with_options(arguments, input, output, error, OLLAMA_ADDRESS, true)
}

/// Runs a task without reading the user configuration file, so unit tests stay
/// independent of the machine they run on.
#[cfg(test)]
pub(super) fn run_task_with_endpoint(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
) -> io::Result<i32> {
    run_task_with_options(arguments, input, output, error, model_endpoint, false)
}

pub(super) fn run_task_with_options(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
    use_config: bool,
) -> io::Result<i32> {
    run_task_with_selection_loader(
        arguments,
        input,
        output,
        error,
        model_endpoint,
        use_config,
        configured_model_selection,
    )
}

fn run_task_with_selection_loader<F>(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
    use_config: bool,
    mut load_configured_selection: F,
) -> io::Result<i32>
where
    F: FnMut(&mut dyn Write) -> Result<Option<(String, Option<String>)>, String>,
{
    if contains_help_before_terminator(arguments) {
        write_task_help(output)?;
        return Ok(0);
    }

    let mut goal = None;
    let mut max_actions = None;
    let mut model = None;
    let mut provider = None::<ProviderSelection>;
    let mut workspace = None;
    let mut checkpoint_out = None;
    let mut resume = None;
    let mut no_config = false;
    let mut activity_jsonl = false;
    let mut stream_events_jsonl = false;
    let mut model_usage_jsonl = false;
    let mut repository_context_json = None;
    let mut verified = false;
    let mut verify_files = Vec::new();
    let mut position = 0;
    while let Some(argument) = arguments.get(position) {
        let Some(option) = argument.to_str() else {
            return usage_error(error, Some("task"), "arguments must be valid UTF-8");
        };
        position += 1;
        match option {
            "--goal" => {
                if goal.is_some() {
                    return usage_error(error, Some("task"), "--goal may be provided only once");
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --goal",
                    );
                };
                let Some(value) = value.to_str() else {
                    return usage_error(error, Some("task"), "goal must be valid UTF-8");
                };
                goal = Some(value.to_owned());
                position += 1;
            }
            "--max-actions" => {
                if max_actions.is_some() {
                    return usage_error(
                        error,
                        Some("task"),
                        "--max-actions may be provided only once",
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --max-actions",
                    );
                };
                let limit = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|limit| *limit > 0);
                let Some(limit) = limit else {
                    return usage_error(
                        error,
                        Some("task"),
                        "action limit must be a positive integer",
                    );
                };
                max_actions = Some(limit);
                position += 1;
            }
            "--model" => {
                if model.is_some() {
                    return usage_error(error, Some("task"), "--model may be provided only once");
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --model",
                    );
                };
                let Some(value) = value.to_str() else {
                    return usage_error(error, Some("task"), "model name must be valid UTF-8");
                };
                model = Some(value.to_owned());
                position += 1;
            }
            "--provider" => {
                if provider.is_some() {
                    return usage_error(
                        error,
                        Some("task"),
                        "--provider may be provided only once",
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --provider",
                    );
                };
                let Some(value) = value.to_str() else {
                    return usage_error(error, Some("task"), "provider name must be valid UTF-8");
                };
                let Some(selected_provider) = parse_provider_selection(value) else {
                    return usage_error(
                        error,
                        Some("task"),
                        &format!("unsupported provider or invalid profile ID '{value}'"),
                    );
                };
                provider = Some(selected_provider);
                position += 1;
            }
            "--workspace" => {
                if workspace.is_some() {
                    return usage_error(
                        error,
                        Some("task"),
                        "--workspace may be provided only once",
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --workspace",
                    );
                };
                workspace = Some(PathBuf::from(value.as_os_str()));
                position += 1;
            }
            "--checkpoint-out" => {
                if checkpoint_out.is_some() {
                    return usage_error(
                        error,
                        Some("task"),
                        "--checkpoint-out may be provided only once",
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --checkpoint-out",
                    );
                };
                checkpoint_out = Some(PathBuf::from(value.as_os_str()));
                position += 1;
            }
            "--resume" => {
                if resume.is_some() {
                    return usage_error(error, Some("task"), "--resume may be provided only once");
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --resume",
                    );
                };
                resume = Some(PathBuf::from(value.as_os_str()));
                position += 1;
            }
            "--stream-events-jsonl" => {
                if stream_events_jsonl {
                    return usage_error(
                        error,
                        Some("task"),
                        "--stream-events-jsonl may be provided only once",
                    );
                }
                stream_events_jsonl = true;
            }
            "--activity-jsonl" => {
                if activity_jsonl {
                    return usage_error(
                        error,
                        Some("task"),
                        "--activity-jsonl may be provided only once",
                    );
                }
                activity_jsonl = true;
            }
            "--model-usage-jsonl" => {
                if model_usage_jsonl {
                    return usage_error(
                        error,
                        Some("task"),
                        "--model-usage-jsonl may be provided only once",
                    );
                }
                model_usage_jsonl = true;
            }
            "--repository-context-json" => {
                if repository_context_json.is_some() {
                    return usage_error(
                        error,
                        Some("task"),
                        "--repository-context-json may be provided only once",
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --repository-context-json",
                    );
                };
                let Some(value) = value.to_str() else {
                    return usage_error(
                        error,
                        Some("task"),
                        "repository context path must be valid UTF-8",
                    );
                };
                repository_context_json = Some(PathBuf::from(value));
                position += 1;
            }
            "--verified" => {
                if verified {
                    return usage_error(
                        error,
                        Some("task"),
                        "--verified may be provided only once",
                    );
                }
                verified = true;
            }
            "--verify-file" => {
                let Some(value) = arguments.get(position) else {
                    return usage_error(
                        error,
                        Some("task"),
                        "the following argument is required: --verify-file",
                    );
                };
                let Some(value) = value.to_str() else {
                    return usage_error(
                        error,
                        Some("task"),
                        "verification file paths must be valid UTF-8",
                    );
                };
                let path = match normalize_verification_path(value) {
                    Ok(path) => path,
                    Err(message) => return usage_error(error, Some("task"), message),
                };
                if verify_files.iter().any(|existing| existing == &path) {
                    return usage_error(
                        error,
                        Some("task"),
                        "each --verify-file path may be provided only once",
                    );
                }
                if verify_files.len() == MAX_COMPLETION_CHECKS {
                    return usage_error(
                        error,
                        Some("task"),
                        "at most 32 --verify-file paths may be provided",
                    );
                }
                verify_files.push(path);
                position += 1;
            }
            "--no-config" => {
                if no_config {
                    return usage_error(
                        error,
                        Some("task"),
                        "--no-config may be provided only once",
                    );
                }
                no_config = true;
            }
            _ => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("unrecognized arguments: {option}"),
                );
            }
        }
    }

    let Some(max_actions) = max_actions else {
        return usage_error(
            error,
            Some("task"),
            "the following argument is required: --max-actions",
        );
    };
    if resume.is_some() && goal.is_some() {
        return usage_error(
            error,
            Some("task"),
            "--goal cannot be used with --resume; the checkpoint stores the task description",
        );
    }
    if repository_context_json.is_some() && (resume.is_some() || checkpoint_out.is_some()) {
        return usage_error(
            error,
            Some("task"),
            "--repository-context-json cannot be combined with --resume or --checkpoint-out",
        );
    }
    if resume.is_none() {
        let Some(goal) = goal.as_deref() else {
            return usage_error(
                error,
                Some("task"),
                "the following argument is required: --goal",
            );
        };
        if goal.trim().is_empty() {
            return usage_error(error, Some("task"), "goal must not be empty");
        }
    }
    if verified {
        if verify_files.is_empty() {
            return usage_error(
                error,
                Some("task"),
                "--verified requires at least one --verify-file PATH",
            );
        }
        if max_actions < 2 {
            return usage_error(
                error,
                Some("task"),
                "--verified requires --max-actions of at least 2 for a plan and task control",
            );
        }
    } else if !verify_files.is_empty() {
        return usage_error(error, Some("task"), "--verify-file requires --verified");
    }
    // Command-line selection always wins: --model or --provider (alone or
    // together) disables the configured defaults entirely, so a flag never mixes
    // with a configured provider.
    if use_config && !no_config && model.is_none() && provider.is_none() {
        match load_configured_selection(error) {
            Ok(Some((configured_model, configured_provider))) => {
                model = Some(configured_model);
                if let Some(name) = configured_provider {
                    let Some(selection) = parse_provider_selection(&name) else {
                        return usage_error(
                            error,
                            Some("task"),
                            "unsupported provider or invalid profile ID in saved defaults",
                        );
                    };
                    provider = Some(selection);
                }
            }
            Ok(None) => {}
            Err(message) => return usage_error(error, Some("task"), &message),
        }
    }
    if verified && model.is_none() {
        return usage_error(
            error,
            Some("task"),
            "--verified requires --model or a saved Ollama model default",
        );
    }
    if verified
        && provider
            .as_ref()
            .is_some_and(|provider| provider != &ProviderSelection::BuiltIn(ModelProvider::Ollama))
    {
        return usage_error(
            error,
            Some("task"),
            "--verified requires the local Ollama provider",
        );
    }
    let mut model_source = match prepare_model_source_for_selection(
        model,
        provider,
        max_actions,
        model_endpoint,
        use_config && !no_config,
    ) {
        Ok(source) => source,
        Err(message) => return usage_error(error, Some("task"), message),
    };
    if let Some(path) = repository_context_json.as_deref() {
        let context = match read_repository_context_json(path) {
            Ok(context) => context,
            Err(message) => return usage_error(error, Some("task"), &message),
        };
        let Some(source) = model_source.as_mut() else {
            return usage_error(
                error,
                Some("task"),
                "--repository-context-json requires an OpenAI-compatible model provider",
            );
        };
        if let Err(message) = source.set_repository_context(context) {
            return usage_error(error, Some("task"), message);
        }
    }
    if model_usage_jsonl {
        match model_source.as_ref() {
            None => {
                return usage_error(
                    error,
                    Some("task"),
                    "--model-usage-jsonl requires a model-backed compatible provider",
                );
            }
            Some(source) if !source.supports_token_usage() => {
                return usage_error(
                    error,
                    Some("task"),
                    "--model-usage-jsonl is supported by OpenAI-compatible providers",
                );
            }
            Some(_) => {}
        }
    }

    if let Some(path) = checkpoint_out.as_deref() {
        if let Err(cause) = validate_checkpoint_output_path(path) {
            return usage_error(
                error,
                Some("task"),
                &format!(
                    "invalid checkpoint output path '{}': {cause}",
                    path.display()
                ),
            );
        }
    }

    let (mut session, task_id, mut turn_id, goal) = if let Some(path) = resume.as_deref() {
        let checkpoint = match read_task_checkpoint(path) {
            Ok(checkpoint) => checkpoint,
            Err(cause) => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("cannot read checkpoint '{}': {cause}", path.display()),
                );
            }
        };
        let prepared = match prepare_resumed_session(&checkpoint, workspace.as_deref()) {
            Ok(prepared) => prepared,
            Err(ResumeError::Checkpoint(cause)) => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("cannot restore checkpoint '{}': {cause}", path.display()),
                );
            }
            Err(ResumeError::TaskCount(count)) => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("checkpoint must contain exactly one task, found {count}"),
                );
            }
            Err(ResumeError::TaskNotRunning(state)) => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("checkpoint task must be running, found {state}"),
                );
            }
            Err(ResumeError::SessionSetup(cause)) => return Err(cause),
        };
        (
            prepared.session,
            prepared.task_id,
            prepared.turn_id,
            prepared.goal,
        )
    } else {
        let Some(goal) = goal else {
            return usage_error(
                error,
                Some("task"),
                "the following argument is required: --goal",
            );
        };
        let mut session = match workspace {
            Some(path) => match TaskSession::new_in_workspace(&path) {
                Ok(session) => session,
                Err(cause) => {
                    return usage_error(
                        error,
                        Some("task"),
                        &format!("invalid workspace path '{}': {cause}", path.display()),
                    );
                }
            },
            None => TaskSession::new(),
        };
        let task_id = session.create_task(goal.clone()).id();
        let turn_id = {
            let task = session
                .get_task_mut(task_id)
                .ok_or_else(|| io::Error::other("task was not found in its session"))?;
            task.transition(TaskState::Running)
                .map_err(io::Error::other)?;
            let turn_id = task.create_turn().map_err(io::Error::other)?;
            task.transition_turn(turn_id, TurnState::Running)
                .map_err(io::Error::other)?;
            turn_id
        };
        (session, task_id, turn_id, goal)
    };
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    let mut observation_number = 0;
    let mut decisions = 0;
    let mut stream_error = None;
    let mut actions_taken = 0;
    let mut turn_results = Vec::new();
    let mut verified_report = None;
    let mut correction_used = false;
    let mut paused_checkpoint = None;
    // The emitter's sequence is scoped to this CLI invocation, not TaskSession history.
    let mut stream_event_sequence = 1_u64;
    let mut last_stream_positions = Vec::new();
    loop {
        let mut displayed_observations = 0;
        let (result, stop_reason, reconcile_after_run, iteration_actions) = if verified {
            let Some(source) = model_source
                .as_mut()
                .and_then(ModelDecisionSource::ollama_source_mut)
            else {
                return Err(io::Error::other(
                    "verified mode requires a local Ollama model",
                ));
            };
            let mut control = VerifiedTaskControl {
                reader: &mut reader,
                line: &mut line,
                error,
            };
            let mut batch = run_verified_turn(
                &mut session,
                task_id,
                turn_id,
                &goal,
                max_actions - actions_taken,
                &verify_files,
                !correction_used,
                checkpoint_out.is_some(),
                source,
                &mut control,
            )?;
            correction_used |= batch.correction_attempted;
            let stop_reason = batch.stop_reason;
            let report = VerifiedTaskReport {
                verification: batch.verification,
                correction_status: batch.correction_status,
                stop_reason,
            };
            let iteration_actions = batch.actions_taken;
            let result = batch
                .turns
                .pop()
                .ok_or_else(|| io::Error::other("verified task did not produce a turn result"))?;
            for earlier_result in batch.turns {
                for observation in earlier_result.observations() {
                    write_observation(output, observation_number + 1, observation)?;
                    observation_number += 1;
                }
                turn_results.push(earlier_result);
            }
            for observation in result.observations() {
                write_observation(output, observation_number + 1, observation)?;
                observation_number += 1;
            }
            displayed_observations = result.observations().len();
            turn_id = result.turn_id();
            verified_report = Some(report);
            (result, stop_reason, false, iteration_actions)
        } else {
            let result = {
                let mut decision_source = CliDecisionSource {
                    needs_project_guidance: model_source.is_some(),
                    decide: |decision_goal: &str,
                             observations: &[WorkspaceObservation],
                             project_guidance: Option<&str>| {
                        while displayed_observations < observations.len() {
                            if let Err(cause) = write_observation(
                                output,
                                observation_number + 1,
                                &observations[displayed_observations],
                            ) {
                                stream_error = Some(cause);
                                return WorkspaceAction::Unsupported {
                                    action: "unable to display action observation".to_owned(),
                                };
                            }
                            displayed_observations += 1;
                            observation_number += 1;
                        }
                        if let Err(cause) = output.flush() {
                            stream_error = Some(cause);
                            return WorkspaceAction::Unsupported {
                                action: "unable to display action observation".to_owned(),
                            };
                        }

                        decisions += 1;
                        if let Some(model_source) = model_source.as_mut() {
                            if let Err(cause) =
                                writeln!(error, "model decision {decisions}/{max_actions}")
                                    .and_then(|()| error.flush())
                            {
                                stream_error = Some(cause);
                                return WorkspaceAction::Unsupported {
                                    action: "unable to display model decision progress".to_owned(),
                                };
                            }
                            let model_started = Instant::now();
                            let action = model_source.choose_action_with_root_project_guidance(
                                decision_goal,
                                observations,
                                project_guidance,
                            );
                            if model_usage_jsonl {
                                let elapsed_ms = model_started.elapsed().as_millis() as u64;
                                let usage = model_source.take_token_usage();
                                let event = serde_json::json!({
                                    "type": "model_token_usage",
                                    "decision": decisions,
                                    "elapsed_ms": elapsed_ms,
                                    "prompt_tokens": usage.and_then(|value| value.prompt_tokens),
                                    "completion_tokens": usage.and_then(|value| value.completion_tokens),
                                    "total_tokens": usage.and_then(|value| value.total_tokens),
                                });
                                if let Err(cause) = writeln!(error, "KRYPPER_MODEL_USAGE {event}")
                                    .and_then(|()| error.flush())
                                {
                                    stream_error = Some(cause);
                                    return WorkspaceAction::Unsupported {
                                        action: "unable to display model usage event".to_owned(),
                                    };
                                }
                            }
                            if let Err(cause) = writeln!(
                                output,
                                "Model selected action: {}",
                                workspace_action_name(&action)
                            ) {
                                stream_error = Some(cause);
                                return WorkspaceAction::Unsupported {
                                    action: "unable to display model action".to_owned(),
                                };
                            }
                            return action;
                        }
                        if let Err(cause) = if activity_jsonl {
                            writeln!(error, "action {decisions}/{max_actions}> ")
                        } else {
                            write!(error, "action {decisions}/{max_actions}> ")
                        }
                        .and_then(|()| error.flush())
                        {
                            stream_error = Some(cause);
                            return WorkspaceAction::Unsupported {
                                action: "unable to display action prompt".to_owned(),
                            };
                        }

                        line.clear();
                        match (&mut reader)
                            .take(MAX_TASK_ACTION_LINE_BYTES as u64 + 1)
                            .read_until(b'\n', &mut line)
                        {
                            Ok(0) => WorkspaceAction::Unsupported {
                                action: "end of input before the next action".to_owned(),
                            },
                            Ok(bytes_read) if bytes_read > MAX_TASK_ACTION_LINE_BYTES => {
                                WorkspaceAction::Unsupported {
                                    action: format!(
                                "action line exceeds limit of {MAX_TASK_ACTION_LINE_BYTES} bytes"
                            ),
                                }
                            }
                            Ok(_) => match std::str::from_utf8(&line) {
                                Ok(line) => parse_task_action(line),
                                Err(cause) => WorkspaceAction::Unsupported {
                                    action: format!("input error: {cause}"),
                                },
                            },
                            Err(cause) => WorkspaceAction::Unsupported {
                                action: format!("input error: {cause}"),
                            },
                        }
                    },
                };
                run_goal_turn_with_budget(
                    &mut session,
                    task_id,
                    turn_id,
                    &goal,
                    max_actions - actions_taken,
                    &mut decision_source,
                )
                .map_err(io::Error::other)?
            };
            let stop_reason = result.stop_reason();
            let iteration_actions = result.actions_taken();
            (result, stop_reason, true, iteration_actions)
        };
        actions_taken += iteration_actions;

        if reconcile_after_run {
            reconcile_task_state(&mut session, task_id, stop_reason)?;
        }
        if activity_jsonl {
            crate::streaming::write_activity_positions(&session, error)?;
        }
        if stream_events_jsonl {
            // Reuse the authoritative metadata-only snapshot. Do not project
            // raw model deltas, tool arguments, or workspace observations.
            let positions = crate::streaming::snapshot_positions(&session);
            for position in &positions {
                if last_stream_positions
                    .iter()
                    .any(|old: &crate::streaming::ActivityPosition| old == position)
                {
                    continue;
                }
                let next = stream_event_sequence
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("stream event sequence exhausted"))?;
                let envelope = crate::streaming::StreamEnvelope {
                    version: 1,
                    sequence: stream_event_sequence,
                    event: crate::streaming::StreamEvent::ActivityPositionUpdated {
                        task_id: position.task_id.clone(),
                        lifecycle_position: position.lifecycle_position,
                        activity_position: position.activity_position,
                    },
                };
                serde_json::to_writer(&mut *error, &envelope).map_err(io::Error::other)?;
                error.write_all(b"\n")?;
                error.flush()?;
                stream_event_sequence = next;
            }
            last_stream_positions = positions;
        }

        if let Some(cause) = stream_error.take() {
            return Err(cause);
        }
        for observation in result.observations().iter().skip(displayed_observations) {
            write_observation(output, observation_number + 1, observation)?;
            observation_number += 1;
        }
        turn_results.push(result);

        match stop_reason {
            GoalTurnStopReason::Continued => {
                if let Some(path) = checkpoint_out.as_deref() {
                    let checkpoint = prepare_checkpoint_for_pause(&session, task_id)?;
                    write_task_checkpoint(path, &checkpoint)?;
                    paused_checkpoint = Some(path.to_path_buf());
                    break;
                }
                let task = session
                    .get_task_mut(task_id)
                    .ok_or_else(|| io::Error::other("task was not found in its session"))?;
                turn_id = task.create_turn().map_err(io::Error::other)?;
                task.transition_turn(turn_id, TurnState::Running)
                    .map_err(io::Error::other)?;
            }
            _ => break,
        }
    }

    let task = session
        .get_task(task_id)
        .ok_or_else(|| io::Error::other("task was not found in its session"))?;
    let result = turn_results
        .last()
        .ok_or_else(|| io::Error::other("task did not produce a turn result"))?;
    let turn = task
        .get_turn(turn_id)
        .ok_or_else(|| io::Error::other("turn was not found in its task"))?;
    let (task_state, turn_state) = snapshot_task_and_turn_state(&session, task_id, turn_id)?;
    let succeeded = task_state == TaskState::Succeeded;
    let cancelled = task_state == TaskState::Cancelled;

    if turn_results.len() > 1 {
        writeln!(output, "Turn results (in order):")?;
        for (index, turn_result) in turn_results.iter().enumerate() {
            write_turn_outcome(
                output,
                &format!("Turn {} result: ", index + 1),
                turn_result.outcome(),
            )?;
            writeln!(
                output,
                "Turn {} actions: {}",
                index + 1,
                turn_result.actions_taken()
            )?;
        }
    }

    writeln!(output, "Task state: {task_state}")?;
    let task_outcome = if paused_checkpoint.is_some() {
        "paused (still running)"
    } else {
        match task_state {
            TaskState::Succeeded => "succeeded",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled by caller",
            TaskState::Pending | TaskState::Running => "still running",
        }
    };
    writeln!(output, "Task outcome: {task_outcome}")?;
    if let Some(path) = paused_checkpoint.as_deref() {
        writeln!(output, "Checkpoint saved: {}", path.display())?;
    }
    writeln!(output, "Turn state: {turn_state}")?;
    write_turn_outcome(output, "Turn outcome: ", result.outcome())?;
    writeln!(output, "Actions: {actions_taken}/{max_actions}")?;
    if let Some(report) = verified_report.as_ref() {
        writeln!(output, "Verification: {:?}", report.verification.status)?;
        for check in &report.verification.checks {
            write!(output, "- {}: {:?}", check.description, check.status)?;
            if let Some(summary) = check.evidence_summary.as_deref() {
                write!(output, " ({summary})")?;
            }
            writeln!(output)?;
        }
        writeln!(output, "Correction: {}", report.correction_status)?;
        writeln!(output, "Stop reason: {:?}", report.stop_reason)?;
        writeln!(output, "Actions remaining: {}", max_actions - actions_taken)?;
    }

    if task.turns().len() > 1 {
        writeln!(output, "Task/turn lifecycle history (metadata only):")?;
        for event in task.history() {
            writeln!(
                output,
                "- sequence {}: {:?}",
                event.sequence(),
                event.transition()
            )?;
        }
    }

    writeln!(output, "Task lifecycle history (metadata only):")?;
    for event in task.events() {
        writeln!(
            output,
            "- sequence {}: task_transition {} -> {}",
            event.sequence(),
            event.previous_state(),
            event.new_state()
        )?;
    }
    writeln!(output, "Turn lifecycle history (metadata only):")?;
    for event in turn.events() {
        writeln!(
            output,
            "- sequence {}: turn_transition {} -> {}",
            event.sequence(),
            event.previous_state(),
            event.new_state()
        )?;
    }

    writeln!(output, "Activity history (metadata only):")?;
    if task.turns().len() == 1 {
        for summary in turn.activity_history() {
            writeln!(output, "- {summary:?}")?;
        }
    } else {
        for event in task.activity_history() {
            writeln!(
                output,
                "- sequence {}: {:?}",
                event.sequence(),
                event.summary()
            )?;
        }
    }

    Ok(if succeeded || paused_checkpoint.is_some() {
        0
    } else if cancelled {
        130
    } else {
        1
    })
}

struct CliDecisionSource<F> {
    needs_project_guidance: bool,
    decide: F,
}

impl<F> BoundedDecisionSource for CliDecisionSource<F>
where
    F: FnMut(&str, &[WorkspaceObservation], Option<&str>) -> WorkspaceAction,
{
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        (self.decide)(goal, observations, None)
    }

    fn needs_root_project_guidance(&self) -> bool {
        self.needs_project_guidance
    }

    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        guidance: Option<&str>,
    ) -> WorkspaceAction {
        (self.decide)(goal, observations, guidance)
    }
}

pub(super) fn snapshot_task_and_turn_state(
    session: &TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
) -> io::Result<(TaskState, TurnState)> {
    let snapshot = session.snapshot();
    let task = snapshot
        .tasks()
        .iter()
        .find(|task| task.id() == task_id)
        .ok_or_else(|| io::Error::other("task was not found in its snapshot"))?;
    let turn = task
        .turns()
        .iter()
        .find(|turn| turn.id() == turn_id)
        .ok_or_else(|| io::Error::other("turn was not found in its task snapshot"))?;

    Ok((task.state(), turn.state()))
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
