//! The `task` command: option parsing, model selection wiring and the turn loop output.

use super::checkpoint_files::{
    read_task_checkpoint, validate_checkpoint_output_path, write_task_checkpoint,
};
use super::help::{contains_help_before_terminator, usage_error, write_task_help};
use super::model_selection::{configured_model_selection, prepare_model_source};
use super::outcome::{write_observation, write_turn_outcome};
use super::task_args::parse_task_action;
use super::MAX_FILE_SIZE;
use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnError, GoalTurnStopReason,
    WorkspaceAction, WorkspaceObservation,
};
use crate::brain::{ModelProvider, OLLAMA_ADDRESS};
use crate::harness::execution::workspace_action_name;
use crate::harness::recovery::{
    prepare_checkpoint_for_pause, prepare_resumed_session, ResumeError,
};
use crate::harness::verification::reconcile_task_state;
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

// Leave room for the action command and path beside a maximum-size file body.
const MAX_TASK_ACTION_LINE_BYTES: usize = MAX_FILE_SIZE + 4096;

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
        let mut displayed_observations = 0;
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
                        let action = model_source.choose_action_with_root_project_guidance(
                            decision_goal,
                            observations,
                            project_guidance,
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
            .map_err(|cause| match cause {
                GoalTurnError::RootProjectGuidance(cause) => io::Error::other(cause),
                cause => io::Error::other(cause),
            })?
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
