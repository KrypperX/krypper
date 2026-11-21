use super::help::{
    contains_help_before_terminator, starts_with_option, unrecognized_arguments, usage_error,
    write_context_help,
};
use crate::harness::context::collect_repository_context;
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

pub(super) fn run(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if contains_help_before_terminator(arguments) {
        write_context_help(output)?;
        return Ok(0);
    }

    let mut goal = None;
    let mut workspace = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        match argument.as_ref() {
            "--" => {
                index += 1;
                break;
            }
            "--goal" | "--workspace" => {
                let Some(value) = arguments.get(index + 1) else {
                    return usage_error(
                        error,
                        Some("context"),
                        &format!("the following argument is required: {argument}"),
                    );
                };
                let value = value.to_string_lossy().into_owned();
                if argument == "--goal" {
                    if goal.replace(value).is_some() {
                        return usage_error(
                            error,
                            Some("context"),
                            "--goal may be provided only once",
                        );
                    }
                } else if workspace.replace(PathBuf::from(value)).is_some() {
                    return usage_error(
                        error,
                        Some("context"),
                        "--workspace may be provided only once",
                    );
                }
                index += 2;
            }
            _ if starts_with_option(&arguments[index]) => {
                return usage_error(
                    error,
                    Some("context"),
                    &unrecognized_arguments(arguments, index),
                );
            }
            _ => {
                return usage_error(
                    error,
                    Some("context"),
                    &unrecognized_arguments(arguments, index),
                );
            }
        }
    }
    if index < arguments.len() {
        return usage_error(
            error,
            Some("context"),
            &unrecognized_arguments(arguments, index),
        );
    }
    let Some(goal) = goal else {
        return usage_error(
            error,
            Some("context"),
            "the following argument is required: --goal",
        );
    };

    let mut session = match workspace {
        Some(path) => match TaskSession::new_in_workspace(&path) {
            Ok(session) => session,
            Err(cause) => {
                return usage_error(
                    error,
                    Some("context"),
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

    let context = match collect_repository_context(&session, task_id, turn_id, &goal) {
        Ok(context) => context,
        Err(cause) => return usage_error(error, Some("context"), &cause.to_string()),
    };
    let encoded = serde_json::to_string_pretty(&context)
        .map_err(|cause| io::Error::other(format!("encode repository context: {cause}")))?;
    writeln!(output, "{encoded}")?;
    Ok(0)
}
