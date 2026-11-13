use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
    WorkspaceObservation,
};
use crate::file_safety::{create_text_file, list_directory, read_text_file};
use crate::model::{
    validate_model_name, workspace_action_name, OllamaDecisionSource, OLLAMA_ADDRESS,
};
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
        _ if command_name.starts_with('-') => usage_error(
            error,
            None,
            &format!("unrecognized arguments: {command_name}"),
        ),
        _ => usage_error(
            error,
            None,
            &format!(
                "invalid choice: '{command_name}' (choose from 'show', 'list', 'write', 'task')"
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
    run_task_with_endpoint(arguments, input, output, error, OLLAMA_ADDRESS)
}

fn run_task_with_endpoint(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
    model_endpoint: SocketAddr,
) -> io::Result<i32> {
    if contains_help_before_terminator(arguments) {
        write_task_help(output)?;
        return Ok(0);
    }

    let mut goal = None;
    let mut max_actions = None;
    let mut model = None;
    let mut workspace = None;
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
            _ => {
                return usage_error(
                    error,
                    Some("task"),
                    &format!("unrecognized arguments: {option}"),
                );
            }
        }
    }

    let Some(goal) = goal else {
        return usage_error(
            error,
            Some("task"),
            "the following argument is required: --goal",
        );
    };
    if goal.trim().is_empty() {
        return usage_error(error, Some("task"), "goal must not be empty");
    }
    let Some(max_actions) = max_actions else {
        return usage_error(
            error,
            Some("task"),
            "the following argument is required: --max-actions",
        );
    };
    if let Some(model) = model.as_deref() {
        if let Err(message) = validate_model_name(model) {
            return usage_error(error, Some("task"), message);
        }
    }

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
    let mut model_source =
        model.map(|name| OllamaDecisionSource::with_endpoint(name, max_actions, model_endpoint));
    let task_id = session.create_task(goal.clone()).id();
    let mut turn_id = {
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

    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    let mut observation_number = 0;
    let mut decisions = 0;
    let mut stream_error = None;
    let mut actions_taken = 0;
    let mut turn_results = Vec::new();
    loop {
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
                        let action = model_source.choose_action(decision_goal, observations);
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
                    if let Err(cause) = write!(error, "action {decisions}/{max_actions}> ")
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

        match stop_reason {
            GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted => session
                .get_task_mut(task_id)
                .ok_or_else(|| io::Error::other("task was not found in its session"))?
                .transition(TaskState::Succeeded)
                .map_err(io::Error::other)?,
            GoalTurnStopReason::Continued | GoalTurnStopReason::Cancelled => {}
            GoalTurnStopReason::ActionBudgetExhausted | GoalTurnStopReason::ActionFailed => {
                session
                    .get_task_mut(task_id)
                    .ok_or_else(|| io::Error::other("task was not found in its session"))?
                    .transition(TaskState::Failed)
                    .map_err(io::Error::other)?;
            }
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
    let succeeded = task.state() == TaskState::Succeeded;
    let cancelled = task.state() == TaskState::Cancelled;

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

    writeln!(output, "Task state: {}", task.state())?;
    let task_outcome = match task.state() {
        TaskState::Succeeded => "succeeded",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled by caller",
        TaskState::Pending | TaskState::Running => "still running",
    };
    writeln!(output, "Task outcome: {task_outcome}")?;
    writeln!(output, "Turn state: {}", turn.state())?;
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

    Ok(if succeeded {
        0
    } else if cancelled {
        130
    } else {
        1
    })
}

fn write_turn_outcome(
    output: &mut dyn Write,
    prefix: &str,
    outcome: Option<&TurnOutcome>,
) -> io::Result<()> {
    match outcome {
        Some(TurnOutcome::Success { result }) => writeln!(output, "{prefix}success: {result}"),
        Some(TurnOutcome::Failure { detail }) => writeln!(output, "{prefix}failure: {detail}"),
        None => writeln!(output, "{prefix}cancelled by caller"),
    }
}

fn parse_task_action(line: &str) -> WorkspaceAction {
    let line = line.trim_end_matches(&['\r', '\n'][..]);
    let (name, arguments) = match line
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
    {
        Some((separator, whitespace)) => (
            &line[..separator],
            Some(&line[separator + whitespace.len_utf8()..]),
        ),
        None => (line, None),
    };
    let invalid = || WorkspaceAction::Unsupported {
        action: format!("invalid input: {line}"),
    };
    let invalid_quoted_path = || WorkspaceAction::Unsupported {
        action: "invalid quoted path arguments".to_owned(),
    };
    let invalid_replace = || WorkspaceAction::Unsupported {
        action: "invalid replace action".to_owned(),
    };
    let invalid_search = || WorkspaceAction::Unsupported {
        action: "invalid search action".to_owned(),
    };
    let invalid_search_tree = || WorkspaceAction::Unsupported {
        action: "invalid search-tree action".to_owned(),
    };

    match name {
        "read" => match parse_task_single_path(arguments) {
            Some(path) => WorkspaceAction::ReadFile { path },
            None if has_quoted_path(arguments) => invalid_quoted_path(),
            None => invalid(),
        },
        "read-range" => match parse_task_read_range(arguments) {
            Some(action) => action,
            None if has_quoted_path(arguments) => invalid_quoted_path(),
            None => invalid(),
        },
        "list" => match arguments.map(str::trim).filter(|path| !path.is_empty()) {
            None => WorkspaceAction::ListDirectory {
                path: PathBuf::from("."),
            },
            Some(arguments) => match parse_task_single_path(Some(arguments)) {
                Some(path) => WorkspaceAction::ListDirectory { path },
                None if has_quoted_path(Some(arguments)) => invalid_quoted_path(),
                None => invalid(),
            },
        },
        "mkdir" => match parse_task_single_path(arguments) {
            Some(path) => WorkspaceAction::CreateDirectory { path },
            None if has_quoted_path(arguments) => invalid_quoted_path(),
            None => invalid(),
        },
        "search" => {
            let Some((directory, query)) = parse_task_search_arguments(arguments) else {
                return invalid_search();
            };
            WorkspaceAction::SearchText { directory, query }
        }
        "search-tree" => {
            let Some((directory, query)) = parse_task_search_arguments(arguments) else {
                return invalid_search_tree();
            };
            WorkspaceAction::SearchTreeText { directory, query }
        }
        "create" => {
            let Some(arguments) = arguments else {
                return invalid();
            };
            let arguments = arguments.trim_start();
            let quoted_path = arguments.starts_with('"');
            let Some((path, contents, true)) = parse_path_argument_prefix(arguments) else {
                return if quoted_path {
                    invalid_quoted_path()
                } else {
                    invalid()
                };
            };
            if path.as_os_str().is_empty() {
                return if quoted_path {
                    invalid_quoted_path()
                } else {
                    invalid()
                };
            }
            WorkspaceAction::CreateFile {
                path,
                contents: contents.to_owned(),
            }
        }
        "replace" => {
            let Some(arguments) = arguments else {
                return invalid_replace();
            };
            let arguments = arguments.trim_start();
            let Some((path, replacement_arguments, true)) = parse_path_argument_prefix(arguments)
            else {
                return invalid_replace();
            };
            if path.as_os_str().is_empty() {
                return invalid_replace();
            }
            let Some((match_text, replacement_text)) = replacement_arguments.split_once(" => ")
            else {
                return invalid_replace();
            };
            WorkspaceAction::ReplaceText {
                path,
                match_text: match_text.to_owned(),
                replacement_text: replacement_text.to_owned(),
            }
        }
        "complete" => WorkspaceAction::Complete {
            result: arguments.unwrap_or_default().trim_start().to_owned(),
        },
        "continue" if arguments.is_none_or(|arguments| arguments.trim().is_empty()) => {
            WorkspaceAction::Continue
        }
        "cancel" if arguments.is_none_or(|arguments| arguments.trim().is_empty()) => {
            WorkspaceAction::Cancel
        }
        _ => invalid(),
    }
}

fn has_quoted_path(arguments: Option<&str>) -> bool {
    arguments.is_some_and(|arguments| arguments.trim_start().starts_with('"'))
}

fn parse_task_single_path(arguments: Option<&str>) -> Option<PathBuf> {
    let arguments = arguments?.trim();
    if arguments.is_empty() {
        return None;
    }

    if arguments.starts_with('"') {
        let (path, remainder, _) = parse_path_argument_prefix(arguments)?;
        if path.as_os_str().is_empty() || !remainder.trim().is_empty() {
            return None;
        }
        Some(path)
    } else {
        Some(PathBuf::from(arguments))
    }
}

fn parse_task_search_arguments(arguments: Option<&str>) -> Option<(PathBuf, String)> {
    let arguments = arguments?.trim_start();
    let (directory, query, has_separator) = parse_path_argument_prefix(arguments)?;
    if directory.as_os_str().is_empty() || !has_separator || query.is_empty() {
        return None;
    }
    Some((directory, query.to_owned()))
}

fn parse_path_argument_prefix(arguments: &str) -> Option<(PathBuf, &str, bool)> {
    if arguments.starts_with('"') {
        let (path, end) = decode_quoted_path(arguments)?;
        let remainder = &arguments[end..];
        let Some(separator) = remainder.chars().next() else {
            return Some((path, "", false));
        };
        if !separator.is_whitespace() {
            return None;
        }
        return Some((path, &remainder[separator.len_utf8()..], true));
    }

    let Some((separator_index, separator)) = arguments
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
    else {
        return Some((PathBuf::from(arguments), "", false));
    };
    Some((
        PathBuf::from(&arguments[..separator_index]),
        &arguments[separator_index + separator.len_utf8()..],
        true,
    ))
}

fn decode_quoted_path(arguments: &str) -> Option<(PathBuf, usize)> {
    let bytes = arguments.as_bytes();
    let mut path = Vec::new();
    let mut index = 1;

    while let Some(byte) = bytes.get(index).copied() {
        match byte {
            b'"' => return Some((path_buf_from_quoted_bytes(path)?, index + 1)),
            b'\\' => {
                index += 1;
                match bytes.get(index).copied()? {
                    b'"' => path.push(b'"'),
                    b'\\' => path.push(b'\\'),
                    b'/' => path.push(b'/'),
                    b'b' => path.push(b'\x08'),
                    b'f' => path.push(b'\x0c'),
                    b'n' => path.push(b'\n'),
                    b'r' => path.push(b'\r'),
                    b't' => path.push(b'\t'),
                    b'u' => {
                        let (character, next_index) = decode_json_unicode_escape(bytes, index + 1)?;
                        let mut encoded = [0; 4];
                        path.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                        index = next_index;
                        continue;
                    }
                    b'x' => {
                        let (byte, next_index) = read_hex_byte_escape(bytes, index + 1)?;
                        path.push(byte);
                        index = next_index;
                        continue;
                    }
                    _ => return None,
                }
                index += 1;
            }
            0..=0x1f => return None,
            _ => {
                let character = arguments.get(index..)?.chars().next()?;
                let mut encoded = [0; 4];
                path.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                index += character.len_utf8();
            }
        }
    }

    None
}

#[cfg(unix)]
fn path_buf_from_quoted_bytes(path: Vec<u8>) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    Some(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(not(unix))]
fn path_buf_from_quoted_bytes(path: Vec<u8>) -> Option<PathBuf> {
    Some(PathBuf::from(String::from_utf8(path).ok()?))
}

fn read_hex_byte_escape(bytes: &[u8], index: usize) -> Option<(u8, usize)> {
    let end = index.checked_add(2)?;
    let digits = std::str::from_utf8(bytes.get(index..end)?).ok()?;
    Some((u8::from_str_radix(digits, 16).ok()?, end))
}

fn decode_json_unicode_escape(bytes: &[u8], index: usize) -> Option<(char, usize)> {
    let (high, mut next_index) = read_json_hex_quad(bytes, index)?;
    let scalar = match high {
        0xd800..=0xdbff => {
            let marker_end = next_index.checked_add(2)?;
            if bytes.get(next_index..marker_end)? != b"\\u" {
                return None;
            }
            let (low, end) = read_json_hex_quad(bytes, marker_end)?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return None;
            }
            next_index = end;
            0x1_0000 + (u32::from(high - 0xd800) << 10) + u32::from(low - 0xdc00)
        }
        0xdc00..=0xdfff => return None,
        value => u32::from(value),
    };
    Some((char::from_u32(scalar)?, next_index))
}

fn read_json_hex_quad(bytes: &[u8], index: usize) -> Option<(u16, usize)> {
    let end = index.checked_add(4)?;
    let digits = std::str::from_utf8(bytes.get(index..end)?).ok()?;
    Some((u16::from_str_radix(digits, 16).ok()?, end))
}

fn quoted_path_token(path: &Path) -> String {
    let mut token = String::new();
    token.push('"');
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let mut remaining = path.as_os_str().as_bytes();
        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(valid_path) => {
                    append_quoted_path_text(&mut token, valid_path);
                    break;
                }
                Err(error) => {
                    let valid_length = error.valid_up_to();
                    if let Ok(valid_path) = std::str::from_utf8(&remaining[..valid_length]) {
                        append_quoted_path_text(&mut token, valid_path);
                    }
                    let invalid_length =
                        error.error_len().unwrap_or(remaining.len() - valid_length);
                    let invalid_end = valid_length + invalid_length;
                    for byte in &remaining[valid_length..invalid_end] {
                        const HEX: &[u8; 16] = b"0123456789abcdef";
                        token.push_str("\\x");
                        token.push(char::from(HEX[usize::from(byte >> 4)]));
                        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
                    }
                    remaining = &remaining[invalid_end..];
                }
            }
        }
    }
    #[cfg(not(unix))]
    if let Some(path) = path.to_str() {
        append_quoted_path_text(&mut token, path);
    } else {
        token.push_str(&format!("{path:?}"));
    }
    token.push('"');
    token
}

fn append_quoted_path_text(token: &mut String, path: &str) {
    for character in path.chars() {
        match character {
            '"' => token.push_str("\\\""),
            '\\' => token.push_str("\\\\"),
            '\u{0008}' => token.push_str("\\b"),
            '\u{000c}' => token.push_str("\\f"),
            '\n' => token.push_str("\\n"),
            '\r' => token.push_str("\\r"),
            '\t' => token.push_str("\\t"),
            character if character.is_control() => {
                token.push_str(&format!("\\u{:04x}", u32::from(character)));
            }
            character => token.push(character),
        }
    }
}

fn parse_task_read_range(arguments: Option<&str>) -> Option<WorkspaceAction> {
    let arguments = arguments?.trim();
    if arguments.starts_with('"') {
        let (path, range_arguments, has_separator) = parse_path_argument_prefix(arguments)?;
        if path.as_os_str().is_empty() || !has_separator {
            return None;
        }
        let mut range_arguments = range_arguments.split_whitespace();
        let start_line = range_arguments.next()?.parse().ok()?;
        let max_lines = range_arguments.next()?.parse().ok()?;
        if range_arguments.next().is_some() {
            return None;
        }
        return Some(WorkspaceAction::ReadRange {
            path,
            start_line,
            max_lines,
        });
    }

    let max_separator = arguments.rfind(char::is_whitespace)?;
    let max_lines = arguments[max_separator..].trim().parse().ok()?;
    let before_max = arguments[..max_separator].trim_end();
    let start_separator = before_max.rfind(char::is_whitespace)?;
    let start_line = before_max[start_separator..].trim().parse().ok()?;
    let path = before_max[..start_separator].trim_end();
    if path.is_empty() {
        return None;
    }
    Some(WorkspaceAction::ReadRange {
        path: PathBuf::from(path),
        start_line,
        max_lines,
    })
}

fn write_observation(
    output: &mut dyn Write,
    number: usize,
    observation: &WorkspaceObservation,
) -> io::Result<()> {
    match observation {
        WorkspaceObservation::FileRead { path, contents } => {
            writeln!(output, "Observation {number}: read {}", path.display())?;
            writeln!(output, "  contents:")?;
            output.write_all(contents.as_bytes())?;
            if !contents.ends_with('\n') {
                output.write_all(b"\n")?;
            }
        }
        WorkspaceObservation::FileRangeRead {
            path,
            start_line,
            end_line,
            excerpt,
        } => {
            writeln!(
                output,
                "Observation {number}: read-range {} lines {start_line}-{end_line}",
                path.display()
            )?;
            output.write_all(excerpt.as_bytes())?;
            if !excerpt.ends_with('\n') {
                output.write_all(b"\n")?;
            }
        }
        WorkspaceObservation::DirectoryListed { path, entries } => {
            writeln!(
                output,
                "Observation {number}: listed {} ({} entries)",
                path.display(),
                entries.len()
            )?;
            for entry in entries {
                writeln!(output, "  {entry:?}")?;
            }
        }
        WorkspaceObservation::DirectoryCreated { path } => {
            writeln!(
                output,
                "Observation {number}: created directory {}",
                path.display()
            )?;
        }
        WorkspaceObservation::TextSearched {
            directory,
            locations,
        } => {
            writeln!(
                output,
                "Observation {number}: search in {} found {} locations",
                directory.display(),
                locations.len()
            )?;
            for location in locations {
                writeln!(
                    output,
                    "  {}:{}",
                    quoted_path_token(&location.path),
                    location.line
                )?;
            }
        }
        WorkspaceObservation::TreeTextSearched {
            directory,
            locations,
        } => {
            writeln!(
                output,
                "Observation {number}: search-tree in {} found {} locations",
                directory.display(),
                locations.len()
            )?;
            for location in locations {
                writeln!(
                    output,
                    "  {}:{}",
                    quoted_path_token(&location.path),
                    location.line
                )?;
            }
        }
        WorkspaceObservation::FileCreated {
            path,
            bytes_written,
        } => {
            writeln!(
                output,
                "Observation {number}: created {} ({bytes_written} bytes)",
                path.display()
            )?;
        }
        WorkspaceObservation::TextReplaced {
            path,
            bytes_written,
        } => {
            writeln!(
                output,
                "Observation {number}: replaced text in {} ({bytes_written} bytes)",
                path.display()
            )?;
        }
        WorkspaceObservation::FileCreationFailed { path, error } => {
            writeln!(
                output,
                "Observation {number}: create {} failed: {error}",
                path.display()
            )?;
        }
        WorkspaceObservation::DirectoryCreationFailed { path, error } => {
            writeln!(
                output,
                "Observation {number}: mkdir {} failed: {error}",
                path.display()
            )?;
        }
        WorkspaceObservation::TextReplacementFailed { path, error } => {
            writeln!(
                output,
                "Observation {number}: replacement in {} failed: {error}",
                path.display()
            )?;
        }
        WorkspaceObservation::TextSearchFailed { directory, error } => {
            writeln!(
                output,
                "Observation {number}: search in {} failed: {error}",
                directory.display()
            )?;
        }
        WorkspaceObservation::TreeTextSearchFailed { directory, error } => {
            writeln!(
                output,
                "Observation {number}: search-tree in {} failed: {error}",
                directory.display()
            )?;
        }
        WorkspaceObservation::ActionFailed { error, .. } => {
            writeln!(output, "Observation {number}: action failed: {error}")?;
        }
    }
    Ok(())
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
        Some("task") => {
            "usage: krypper task [-h] --goal GOAL --max-actions N [--workspace PATH] [--model NAME]"
        }
        _ => "usage: krypper [-h] {show,list,write,task} ...",
    };
    let program = command.map_or_else(|| "krypper".to_owned(), |name| format!("krypper {name}"));
    writeln!(error, "{usage}")?;
    writeln!(error, "{program}: error: {message}")?;
    Ok(2)
}

fn write_root_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper [-h] {show,list,write,task} ...\n\nLocal command-line entry point for Krypper.\n\npositional arguments:\n  {show,list,write,task}\n    show        display a UTF-8 text file\n    list        list immediate directory entries\n    write       create a new UTF-8 text file\n    task        run a caller-directed bounded task\n\noptions:\n  -h, --help    show this help message and exit\n",
    )
}

fn write_task_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        concat!(
            "usage: krypper task [-h] --goal GOAL --max-actions N [--workspace PATH] [--model NAME]\n\n",
            "run a bounded task in the selected workspace or current directory\n\n",
            "options:\n  --goal GOAL          required task goal\n",
            "  --max-actions N      required positive action limit for the whole task\n",
            "  --workspace PATH     use an existing directory as the captured task workspace\n",
            "  --model NAME         opt in to local Ollama action selection\n",
            "  -h, --help           show this help message and exit\n\n",
            "Without --workspace, task captures the current directory as its workspace.\n",
            "With --workspace, the selected directory is validated before task history starts;\n",
            "the process current directory is not changed.\n",
            "Without --model, task remains caller-directed and makes no model requests.\n",
            "--workspace PATH works with --model NAME and uses the same captured workspace.\n",
            "Model mode sends the goal and prior observations to http://127.0.0.1:11434/api/chat.\n",
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
    use super::{parse_task_action, quoted_path_token, run_with, write_observation};
    use crate::agent_loop::{WorkspaceAction, WorkspaceObservation};
    use crate::task_lifecycle::WorkspaceSearchLocation;
    use std::ffi::OsString;
    use std::io::{Cursor, Read};
    use std::path::{Path, PathBuf};

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
    fn quoted_task_paths_round_trip_and_preserve_action_payloads() {
        let path = PathBuf::from("nested with \"quotes\" \\ café/file.txt");
        let token = r#""nested with \"quotes\" \\ café/file.txt""#;

        assert_eq!(quoted_path_token(&path), token);
        assert!(matches!(
            parse_task_action(&format!("read {token}")),
            WorkspaceAction::ReadFile { path: actual } if actual == path
        ));
        assert!(matches!(
            parse_task_action(&format!("read-range {token} 2 3")),
            WorkspaceAction::ReadRange {
                path: actual,
                start_line: 2,
                max_lines: 3,
            } if actual == path
        ));
        assert!(matches!(
            parse_task_action(&format!("list {token}")),
            WorkspaceAction::ListDirectory { path: actual } if actual == path
        ));
        assert!(matches!(
            parse_task_action(&format!("mkdir {token}")),
            WorkspaceAction::CreateDirectory { path: actual } if actual == path
        ));
        assert!(matches!(
            parse_task_action(&format!("create {token} payload  with spaces")),
            WorkspaceAction::CreateFile {
                path: actual,
                contents,
            } if actual == path && contents == "payload  with spaces"
        ));
        assert!(matches!(
            parse_task_action(&format!("replace {token} match => replacement")),
            WorkspaceAction::ReplaceText {
                path: actual,
                match_text,
                replacement_text,
            } if actual == path && match_text == "match" && replacement_text == "replacement"
        ));
        assert!(matches!(
            parse_task_action(&format!("search {token}  literal query ")),
            WorkspaceAction::SearchText {
                directory,
                query,
            } if directory == path && query == " literal query "
        ));
        assert!(matches!(
            parse_task_action(&format!("search-tree {token} literal query")),
            WorkspaceAction::SearchTreeText {
                directory,
                query,
            } if directory == path && query == "literal query"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_quoted_path_tokens_round_trip() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"nested/file-\xff.txt".to_vec()));
        let token = r#""nested/file-\xff.txt""#;

        assert_eq!(quoted_path_token(&path), token);
        let mut output = Vec::new();
        write_observation(
            &mut output,
            1,
            &WorkspaceObservation::TreeTextSearched {
                directory: PathBuf::from("."),
                locations: vec![WorkspaceSearchLocation {
                    path: path.clone(),
                    line: 2,
                }],
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("Observation 1: search-tree in . found 1 locations\n  {token}:2\n")
        );
        assert!(matches!(
            parse_task_action(&format!("read-range {token} 2 3")),
            WorkspaceAction::ReadRange {
                path: actual,
                start_line: 2,
                max_lines: 3,
            } if actual == path
        ));
        assert!(matches!(
            parse_task_action(&format!("replace {token} match => replacement")),
            WorkspaceAction::ReplaceText {
                path: actual,
                match_text,
                replacement_text,
            } if actual == path && match_text == "match" && replacement_text == "replacement"
        ));
    }

    #[test]
    fn unquoted_task_paths_keep_existing_parsing() {
        assert!(matches!(
            parse_task_action("read legacy path.txt"),
            WorkspaceAction::ReadFile { path } if path == Path::new("legacy path.txt")
        ));
        assert!(matches!(
            parse_task_action("read-range legacy path.txt 2 3"),
            WorkspaceAction::ReadRange {
                path,
                start_line: 2,
                max_lines: 3,
            } if path == Path::new("legacy path.txt")
        ));
        assert!(matches!(
            parse_task_action("search . phrase with spaces"),
            WorkspaceAction::SearchText { directory, query }
                if directory == Path::new(".") && query == "phrase with spaces"
        ));
        assert!(matches!(
            parse_task_action("create note.txt payload  unchanged"),
            WorkspaceAction::CreateFile { path, contents }
                if path == Path::new("note.txt") && contents == "payload  unchanged"
        ));
    }

    #[test]
    fn malformed_quoted_task_paths_are_rejected() {
        for action in [
            "read \"unterminated",
            r#"read "invalid\q""#,
            r#"read "invalid\x""#,
            r#"read "invalid\x0""#,
            r#"read "invalid\xgg""#,
            r#"read "invalid\uD800""#,
            "search \"path\"suffix query",
        ] {
            assert!(
                matches!(
                    parse_task_action(action),
                    WorkspaceAction::Unsupported { .. }
                ),
                "accepted malformed task action: {action}"
            );
        }
    }

    #[test]
    fn quoted_paths_decode_valid_unicode_escapes() {
        assert!(matches!(
            parse_task_action(r#"read "unicode \u4e2d\ud83d\ude00.txt""#),
            WorkspaceAction::ReadFile { path }
                if path == Path::new("unicode 中😀.txt")
        ));
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
