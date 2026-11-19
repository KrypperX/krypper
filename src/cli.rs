use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::brain::{
    validate_model_name_for_provider, AnthropicDecisionSource, CloudflareAccountId,
    GeminiDecisionSource, ModelDecisionSource, ModelProvider, OllamaDecisionSource,
    OpenAiCompatibleCredentials, OpenAiCompatibleDecisionSource, OpenAiCompatibleProvider,
    ResponsesDecisionSource, ResponsesProvider, OLLAMA_ADDRESS,
};
use crate::harness::execution::workspace_action_name;
use crate::harness::recovery::{
    prepare_checkpoint_for_pause, prepare_resumed_session, ResumeError,
};
use crate::harness::verification::reconcile_task_state;
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use crate::tools::{create_text_file, list_directory, read_text_file};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

mod checkpoint_files;
mod config;
mod models;
mod outcome;
mod task_args;
use self::checkpoint_files::{
    read_task_checkpoint, validate_checkpoint_output_path, write_task_checkpoint,
};
use self::config::{config_path, load_model_defaults};
use self::outcome::{write_observation, write_turn_outcome};
use self::task_args::parse_task_action;

const MAX_FILE_SIZE: usize = 1024 * 1024;
// Leave room for the action command and path beside a maximum-size file body.
const MAX_TASK_ACTION_LINE_BYTES: usize = MAX_FILE_SIZE + 4096;

/// Runs Krypper using the process arguments and standard streams.
pub fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    match run_with(
        &args,
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    ) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("krypper: error: {error}");
            1
        }
    }
}

fn run_with(
    args: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    let Some(command) = args.first() else {
        return usage_error(error, None, "a command is required");
    };
    let Some(command_name) = command.to_str() else {
        return usage_error(error, None, "invalid command");
    };

    if command_name == "-h" || command_name == "--help" {
        write_root_help(output)?;
        return Ok(0);
    }

    let arguments = &args[1..];
    match command_name {
        "show" => run_show(arguments, output, error),
        "list" => run_list(arguments, output, error),
        "write" => run_write(arguments, input, output, error),
        "task" => run_task(arguments, input, output, error),
        "models" => models::run(arguments, output, error),
        _ if command_name.starts_with('-') => usage_error(
            error,
            None,
            &format!("unrecognized arguments: {command_name}"),
        ),
        _ => usage_error(
            error,
            None,
            &format!(
                "invalid choice: '{command_name}' (choose from 'show', 'list', 'write', 'task', 'models')"
            ),
        ),
    }
}

fn run_task(
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
fn run_task_with_endpoint(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
) -> io::Result<i32> {
    run_task_with_options(arguments, input, output, error, model_endpoint, false)
}

fn run_task_with_options(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
    use_config: bool,
) -> io::Result<i32> {
    if contains_help_before_terminator(arguments) {
        write_task_help(output)?;
        return Ok(0);
    }

    let mut goal = None;
    let mut max_actions = None;
    let mut model = None;
    let mut provider = None;
    let mut workspace = None;
    let mut checkpoint_out = None;
    let mut resume = None;
    let mut no_config = false;
    let mut activity_jsonl = false;
    let mut stream_events_jsonl = false;
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
                let Some(selected_provider) = ModelProvider::parse(value) else {
                    return usage_error(
                        error,
                        Some("task"),
                        &format!(
                            "unsupported provider '{value}' (choose from 'ollama', 'gemini', 'openai', 'xai', 'anthropic', 'cloudflare', 'groq', 'openrouter', 'deepseek', 'zai', 'mistral')"
                        ),
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
    // Command-line selection always wins: --model or --provider (alone or
    // together) disables the configured defaults entirely, so a flag never mixes
    // with a configured provider.
    if use_config && !no_config && model.is_none() && provider.is_none() {
        match configured_model_selection(error) {
            Ok(Some((configured_model, configured_provider))) => {
                model = Some(configured_model);
                provider = configured_provider;
            }
            Ok(None) => {}
            Err(message) => return usage_error(error, Some("task"), &message),
        }
    }
    let mut model_source = match prepare_model_source(model, provider, max_actions, model_endpoint)
    {
        Ok(source) => source,
        Err(message) => return usage_error(error, Some("task"), message),
    };

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
    let mut paused_checkpoint = None;
    // The emitter's sequence is scoped to this CLI invocation, not TaskSession history.
    let mut stream_event_sequence = 1_u64;
    let mut last_stream_positions = Vec::new();
    loop {
        let project_guidance = if model_source.is_some() {
            session
                .read_root_project_guidance(task_id)
                .map_err(io::Error::other)?
        } else {
            None
        };
        let mut displayed_observations = 0;
        let result = {
            let mut decision_source =
                |decision_goal: &str, observations: &[WorkspaceObservation]| {
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
                        let action = model_source.choose_action_with_root_project_guidance(
                            decision_goal,
                            observations,
                            project_guidance.as_deref(),
                        );
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
        actions_taken += result.actions_taken();
        let stop_reason = result.stop_reason();

        reconcile_task_state(&mut session, task_id, stop_reason)?;
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

fn snapshot_task_and_turn_state(
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

/// Looks up the user-level model defaults and announces any that apply.
///
/// The notice goes to standard error so a configured default can never start
/// model requests without the person seeing which model and file selected it.
fn configured_model_selection(
    error: &mut dyn Write,
) -> Result<Option<(String, Option<ModelProvider>)>, String> {
    let Some(path) = config_path(
        std::env::var_os("KRYPPER_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )?
    else {
        return Ok(None);
    };
    let Some(defaults) = load_model_defaults(&path)? else {
        return Ok(None);
    };
    let provider = match defaults.provider.as_deref() {
        Some(name) => Some(ModelProvider::parse(name).ok_or_else(|| {
            format!(
                "invalid config file '{}': unsupported provider '{name}'",
                defaults.path.display()
            )
        })?),
        None => None,
    };
    let _ = writeln!(
        error,
        "krypper: using model '{}' (provider: {}) from {}; pass --no-config to ignore it",
        defaults.model,
        defaults.provider.as_deref().unwrap_or("ollama"),
        defaults.path.display()
    );
    Ok(Some((defaults.model, provider)))
}

fn prepare_model_source(
    model: Option<String>,
    provider: Option<ModelProvider>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
) -> Result<Option<ModelDecisionSource>, &'static str> {
    let Some(model) = model else {
        return if provider.is_some() {
            Err("--model is required with --provider")
        } else {
            Ok(None)
        };
    };
    let provider = provider.unwrap_or(ModelProvider::Ollama);
    validate_model_name_for_provider(&model, provider)?;
    let source = match provider {
        ModelProvider::Ollama => ModelDecisionSource::Ollama(OllamaDecisionSource::with_endpoint(
            model,
            action_budget,
            ollama_endpoint,
        )),
        ModelProvider::Gemini => ModelDecisionSource::Gemini(GeminiDecisionSource::new(
            model,
            hosted_api_key(
                "GEMINI_API_KEY",
                "GEMINI_API_KEY must be set and nonempty for --provider gemini",
            )?,
            action_budget,
        )?),
        ModelProvider::OpenAi => ModelDecisionSource::Responses(ResponsesDecisionSource::new(
            model,
            ResponsesProvider::OpenAi,
            hosted_api_key(
                "OPENAI_API_KEY",
                "OPENAI_API_KEY must be set and nonempty for --provider openai",
            )?,
            action_budget,
        )),
        ModelProvider::Xai => ModelDecisionSource::Responses(ResponsesDecisionSource::new(
            model,
            ResponsesProvider::Xai,
            hosted_api_key(
                "XAI_API_KEY",
                "XAI_API_KEY must be set and nonempty for --provider xai",
            )?,
            action_budget,
        )),
        ModelProvider::Anthropic => ModelDecisionSource::Anthropic(AnthropicDecisionSource::new(
            model,
            hosted_api_key(
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_API_KEY must be set and nonempty for --provider anthropic",
            )?,
            action_budget,
        )),
        ModelProvider::OpenAiCompatible(provider) => {
            let credentials = match provider {
                OpenAiCompatibleProvider::Cloudflare => {
                    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID").map_err(|_| {
                        "CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare"
                    })?;
                    let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                    let api_token = hosted_api_key(
                        "CLOUDFLARE_API_TOKEN",
                        "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare",
                    )?;
                    OpenAiCompatibleCredentials::Cloudflare {
                        account_id,
                        api_token,
                    }
                }
                OpenAiCompatibleProvider::Groq => OpenAiCompatibleCredentials::Groq {
                    api_key: hosted_api_key(
                        "GROQ_API_KEY",
                        "GROQ_API_KEY must be set and nonempty for --provider groq",
                    )?,
                },
                OpenAiCompatibleProvider::OpenRouter => OpenAiCompatibleCredentials::OpenRouter {
                    api_key: hosted_api_key(
                        "OPENROUTER_API_KEY",
                        "OPENROUTER_API_KEY must be set and nonempty for --provider openrouter",
                    )?,
                },
                OpenAiCompatibleProvider::DeepSeek => OpenAiCompatibleCredentials::DeepSeek {
                    api_key: hosted_api_key(
                        "DEEPSEEK_API_KEY",
                        "DEEPSEEK_API_KEY must be set and nonempty for --provider deepseek",
                    )?,
                },
                OpenAiCompatibleProvider::Zai => OpenAiCompatibleCredentials::Zai {
                    api_key: hosted_api_key(
                        "ZAI_API_KEY",
                        "ZAI_API_KEY must be set and nonempty for --provider zai",
                    )?,
                },
                OpenAiCompatibleProvider::Mistral => OpenAiCompatibleCredentials::Mistral {
                    api_key: hosted_api_key(
                        "MISTRAL_API_KEY",
                        "MISTRAL_API_KEY must be set and nonempty for --provider mistral",
                    )?,
                },
            };
            ModelDecisionSource::OpenAiCompatible(OpenAiCompatibleDecisionSource::new(
                model,
                credentials,
                action_budget,
            )?)
        }
    };
    Ok(Some(source))
}

fn hosted_api_key(
    name: &'static str,
    missing_message: &'static str,
) -> Result<String, &'static str> {
    let key = std::env::var(name).map_err(|_| missing_message)?;
    if key.trim().is_empty() || key.trim() != key || key.chars().any(char::is_control) {
        return Err(missing_message);
    }
    Ok(key)
}

fn run_show(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if contains_help_before_terminator(arguments) {
        write_show_help(output)?;
        return Ok(0);
    }
    let mut positional = arguments;
    let options_ended = positional.first().is_some_and(|argument| argument == "--");
    if options_ended {
        positional = &positional[1..];
    }
    let Some(path) = positional.first() else {
        return usage_error(
            error,
            Some("show"),
            "the following arguments are required: path",
        );
    };
    if (!options_ended && starts_with_option(path)) || positional.len() > 1 {
        let unrecognized = if !options_ended && starts_with_option(path) {
            unrecognized_arguments(positional, 0)
        } else {
            unrecognized_arguments(positional, 1)
        };
        return usage_error(error, Some("show"), &unrecognized);
    }
    match read_text_file(path.as_os_str()) {
        Ok(contents) => {
            output.write_all(contents.as_bytes())?;
            Ok(0)
        }
        Err(cause) => usage_error(error, Some("show"), cause.message()),
    }
}

fn run_list(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if contains_help_before_terminator(arguments) {
        write_list_help(output)?;
        return Ok(0);
    }
    let mut positional = arguments;
    if positional.first().is_some_and(|argument| argument == "--") {
        positional = &positional[1..];
    } else if positional
        .first()
        .is_some_and(|argument| starts_with_option(argument))
    {
        return usage_error(error, Some("list"), &unrecognized_arguments(positional, 0));
    }
    if positional.len() > 1 {
        return usage_error(error, Some("list"), &unrecognized_arguments(positional, 1));
    }
    let path = positional
        .first()
        .map_or_else(|| OsStr::new("."), OsString::as_os_str);
    match list_directory(path) {
        Ok(entries) => {
            for line in entries {
                writeln!(output, "{line}")?;
            }
            Ok(0)
        }
        Err(cause) => usage_error(error, Some("list"), cause.message()),
    }
}

fn run_write(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    let mut position = 0;
    let mut from_stdin = false;
    let mut options_ended = false;
    while let Some(argument) = arguments.get(position) {
        if !options_ended {
            if argument == "-h" || argument == "--help" {
                write_write_help(output)?;
                return Ok(0);
            }
            if argument == "--stdin" {
                from_stdin = true;
                position += 1;
                continue;
            }
            if argument == "--" {
                options_ended = true;
                position += 1;
                continue;
            }
            if starts_with_option(argument) {
                return usage_error(
                    error,
                    Some("write"),
                    &unrecognized_arguments(arguments, position),
                );
            }
        }
        break;
    }

    let Some(path) = arguments.get(position) else {
        return usage_error(
            error,
            Some("write"),
            "the following arguments are required: path",
        );
    };
    position += 1;
    let trailing = &arguments[position..];

    let text = if from_stdin {
        if !trailing.is_empty() {
            return usage_error(
                error,
                Some("write"),
                "extra text is not allowed with --stdin",
            );
        }
        let mut bytes = Vec::with_capacity(MAX_FILE_SIZE + 1);
        input
            .take(MAX_FILE_SIZE as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_SIZE {
            return usage_error(error, Some("write"), "stdin exceeds 1 MiB");
        }
        match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return usage_error(error, Some("write"), "stdin is not valid UTF-8"),
        }
    } else {
        if trailing.is_empty() {
            return usage_error(error, Some("write"), "text is required");
        }
        let mut words = Vec::with_capacity(trailing.len());
        for word in trailing {
            let Some(word) = word.to_str() else {
                return usage_error(error, Some("write"), "text is not valid UTF-8");
            };
            words.push(word);
        }
        words.join(" ")
    };

    match create_text_file(path.as_os_str(), &text) {
        Ok(()) => Ok(0),
        Err(cause) => usage_error(error, Some("write"), cause.message()),
    }
}

fn starts_with_option(argument: &OsStr) -> bool {
    argument.to_string_lossy().starts_with('-')
}

fn contains_help_before_terminator(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != OsStr::new("--"))
        .any(|argument| argument == "-h" || argument == "--help")
}

fn unrecognized_arguments(arguments: &[OsString], skip: usize) -> String {
    let rest = arguments
        .iter()
        .skip(skip)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!("unrecognized arguments: {rest}")
}

fn usage_error(error: &mut dyn Write, command: Option<&str>, message: &str) -> io::Result<i32> {
    let usage = match command {
        Some("show") => "usage: krypper show [-h] path",
        Some("list") => "usage: krypper list [-h] [path]",
        Some("write") => "usage: krypper write [-h] [--stdin] path ...",
        Some("models") => "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json]",
        Some("task") => {
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--no-config] [--checkpoint-out PATH] [--resume PATH] [--activity-jsonl]"
        }
        _ => "usage: krypper [-h] {show,list,write,task,models} ...",
    };
    let program = command.map_or_else(|| "krypper".to_owned(), |name| format!("krypper {name}"));
    writeln!(error, "{usage}")?;
    writeln!(error, "{program}: error: {message}")?;
    Ok(2)
}

fn write_root_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper [-h] {show,list,write,task,models} ...\n\nLocal command-line entry point for Krypper.\n\npositional arguments:\n  {show,list,write,task,models}\n    show        display a UTF-8 text file\n    list        list immediate directory entries\n    write       create a new UTF-8 text file\n    task        run a caller-directed bounded task\n    models      browse offline model catalog\n\noptions:\n  -h, --help    show this help message and exit\n",
    )
}

fn write_task_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        concat!(
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--no-config] [--checkpoint-out PATH] [--resume PATH]\n\n",
            "run a bounded task in the selected workspace or current directory\n\n",
            "options:\n  --goal GOAL          task goal (required unless --resume is used)\n",
            "  --max-actions N      required positive action limit for this invocation\n",
            "  --workspace PATH     use an existing directory as the captured task workspace\n",
            "  --model NAME         opt in to model-selected actions (defaults to local Ollama)\n",
            "  --provider NAME      choose ollama, gemini, openai, xai, anthropic, cloudflare, groq, openrouter, deepseek, zai, or mistral; requires --model\n",
            "  --no-config          ignore the user configuration file for this run\n",
            "  --checkpoint-out PATH write only when a completed turn reaches continue; file must be new\n",
            "  --resume PATH        restore one running task and start a fresh turn\n",
            "  --activity-jsonl    write read-only task activity positions as JSONL to stderr after each turn\n",
            "  --stream-events-jsonl  write versioned metadata-only stream events as JSONL to stderr after each turn\n",
            "  -h, --help           show this help message and exit\n\n",
            "Without --workspace, task captures the current directory as its workspace.\n",
            "With --workspace, the selected directory is validated before task history starts;\n",
            "the process current directory is not changed.\n",
            "Without --model or --provider, task remains caller-directed and makes no model requests,\n",
            "unless the user configuration file (see README) sets a default model; --no-config ignores it.\n",
            "Without --model, task remains caller-directed if --provider is also omitted.\n",
            "--model NAME without --provider uses local Ollama at 127.0.0.1:11434. Ollama needs no API key.\n",
            "Each hosted provider requires explicit --provider selection and sends HTTPS requests only\n",
            "to its fixed API endpoint; no custom API URL is accepted. Required credentials:\n",
            "  gemini: GEMINI_API_KEY -> https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent\n",
            "  Gemini model name must be one safe path component; no custom URL is accepted.\n",
            "  openai: OPENAI_API_KEY -> https://api.openai.com/v1/responses\n",
            "  xai: XAI_API_KEY -> https://api.x.ai/v1/responses\n",
            "  anthropic: ANTHROPIC_API_KEY -> https://api.anthropic.com/v1/messages\n",
            "  cloudflare: CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN ->\n",
            "    https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/v1/chat/completions\n",
            "  groq: GROQ_API_KEY -> https://api.groq.com/openai/v1/chat/completions\n",
            "  openrouter: OPENROUTER_API_KEY -> https://openrouter.ai/api/v1/chat/completions\n",
            "  deepseek: DEEPSEEK_API_KEY -> https://api.deepseek.com/chat/completions\n",
            "  zai: ZAI_API_KEY -> https://api.z.ai/api/paas/v4/chat/completions\n",
            "  mistral: MISTRAL_API_KEY -> https://api.mistral.ai/v1/chat/completions\n",
            "OpenRouter may select an upstream provider for the requested model even when fallback\n",
            "providers are disabled in the request.\n",
            "Hosted requests send the task goal and observations, which can include workspace file text,\n",
            "to that provider and may incur provider charges.\n",
            "--workspace PATH works with --model NAME or --provider NAME and uses the same captured workspace.\n",
            "Without --checkpoint-out, task can continue across turns up to the action limit.\n",
            "With --checkpoint-out, a new checkpoint is written only after a completed\n",
            "turn reaches the safe continue boundary.\n",
            "A failed or interrupted turn may already have changed workspace files.\n",
            "The most recent checkpoint stays unchanged or the requested new checkpoint\n",
            "is absent.\n",
            "Inspect and reconcile the current workspace before resuming from an older\n",
            "checkpoint after such a turn.\n",
            "Checkpoint files retain lifecycle metadata, not prior observations or file contents.\n",
            "Resume from the newest checkpoint and use a new --checkpoint-out path for each pause.\n",
            "Ollama sends the task description and current-turn observations to http://127.0.0.1:11434/api/chat.\n",
            "Read observations can include workspace file text. Ollama must be running locally\n",
            "with the named model available. Only supported task actions and completion are exposed.\n\n",
            "Enter one action per line on standard input:\n",
            "  read PATH\n  read-range PATH START_LINE MAX_LINES\n",
            "  list [PATH]\n  mkdir PATH\n  create PATH TEXT\n",
            "  replace PATH MATCH_TEXT => REPLACEMENT_TEXT\n",
            "  search DIRECTORY QUERY\n  search-tree DIRECTORY QUERY\n",
            "  continue\n  complete [RESULT]\n  cancel\n\n",
            "PATH may be unquoted as before or a JSON double-quoted string; JSON escapes apply, with \\xNN for non-UTF-8 Unix bytes.\n",
            "Search results print reusable quoted path tokens. Text after PATH keeps its existing syntax.\n",
            "mkdir creates one directory and requires its parent to exist.\n",
            "read-range uses one-based lines, returns at most 200 lines and 64 KiB, and reports the actual range.\n",
            "replace requires one non-empty exact match; it prints only path and byte-count metadata.\n",
            "search returns relative paths and line numbers without matched text.\n",
            "search-tree does the same recursively with 4 KiB queries, 1 MiB files, 16 MiB total,\n",
            "10,000 locations, depth 16, and 4,096 directory entries at most.\n",
            "continue ends this turn and starts another while action budget remains.\n",
            "complete ends the task successfully. Invalid input, end-of-input, action failures,\n",
            "and budget exhaustion fail the task. Explicit cancellation reports a cancelled\n",
            "task and exits with status 130.\n",
        )
        .as_bytes(),
    )
}

fn write_show_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper show [-h] path\n\ndisplay a UTF-8 text file\n\npositional arguments:\n  path        relative path inside the current directory\n\noptions:\n  -h, --help  show this help message and exit\n",
    )
}

fn write_list_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper list [-h] [path]\n\nlist immediate directory entries\n\npositional arguments:\n  path        relative directory inside the current directory\n\noptions:\n  -h, --help  show this help message and exit\n",
    )
}

fn write_write_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper write [-h] [--stdin] path ...\n\ncreate a new UTF-8 text file\n\npositional arguments:\n  path        relative file path inside the current directory\n  text        text to write\n\noptions:\n  -h, --help  show this help message and exit\n  --stdin     read text from standard input (up to 1 MiB)\n",
    )
}

#[cfg(test)]
mod tests {
    use super::run_with;
    use std::ffi::OsString;
    use std::io::{Cursor, Read};

    struct CountingReader {
        input: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let bytes_read = self.input.read(buffer)?;
            self.bytes_read += bytes_read;
            Ok(bytes_read)
        }
    }

    #[test]
    fn write_help_is_printed_to_stdout() {
        let args = [OsString::from("write"), OsString::from("--help")];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            run_with(&args, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("usage: krypper write"));
        assert!(error.is_empty());
    }

    #[test]
    fn oversized_task_line_stops_reading_without_buffering_the_rest() {
        const MAX_TASK_ACTION_LINE_BYTES: usize = 1024 * 1024 + 4096;

        let mut bytes = b"list . ".to_vec();
        bytes.extend(std::iter::repeat_n(b' ', MAX_TASK_ACTION_LINE_BYTES * 4));
        bytes.push(b'\n');
        let input_len = bytes.len();
        let mut input = CountingReader {
            input: Cursor::new(bytes),
            bytes_read: 0,
        };
        let args = [
            OsString::from("task"),
            OsString::from("--goal"),
            OsString::from("Inspect files"),
            OsString::from("--max-actions"),
            OsString::from("2"),
        ];
        let mut output = Vec::new();
        let mut error = Vec::new();

        let code = run_with(&args, &mut input, &mut output, &mut error).unwrap();

        assert_eq!(code, 1);
        assert!(input.bytes_read < input_len);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("action line exceeds limit of 1052672 bytes"));
    }
}

#[cfg(test)]
#[path = "cli_model_tests.rs"]
mod model_cli_tests;
