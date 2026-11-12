use crate::agent_loop::{
    run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use crate::file_safety::{create_text_file, list_directory, read_text_file};
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;

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
    if contains_help_before_terminator(arguments) {
        write_task_help(output)?;
        return Ok(0);
    }

    let mut goal = None;
    let mut max_actions = None;
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

    let mut session = TaskSession::new();
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
            let mut decision_source = |_goal: &str, observations: &[WorkspaceObservation]| {
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
                if let Err(cause) =
                    write!(error, "action {decisions}/{max_actions}> ").and_then(|()| error.flush())
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
    let invalid_replace = || WorkspaceAction::Unsupported {
        action: "invalid replace action".to_owned(),
    };
    let invalid_search = || WorkspaceAction::Unsupported {
        action: "invalid search action".to_owned(),
    };

    match name {
        "read" => arguments
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| WorkspaceAction::ReadFile {
                path: PathBuf::from(path),
            })
            .unwrap_or_else(invalid),
        "read-range" => parse_task_read_range(arguments).unwrap_or_else(invalid),
        "list" => WorkspaceAction::ListDirectory {
            path: PathBuf::from(
                arguments
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .unwrap_or("."),
            ),
        },
        "mkdir" => arguments
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| WorkspaceAction::CreateDirectory {
                path: PathBuf::from(path),
            })
            .unwrap_or_else(invalid),
        "search" => {
            let Some(arguments) = arguments else {
                return invalid_search();
            };
            let arguments = arguments.trim_start();
            let Some((separator, whitespace)) = arguments
                .char_indices()
                .find(|(_, character)| character.is_whitespace())
            else {
                return invalid_search();
            };
            let directory = &arguments[..separator];
            let query = &arguments[separator + whitespace.len_utf8()..];
            if directory.is_empty() || query.is_empty() {
                return invalid_search();
            }
            WorkspaceAction::SearchText {
                directory: PathBuf::from(directory),
                query: query.to_owned(),
            }
        }
        "create" => {
            let Some(arguments) = arguments else {
                return invalid();
            };
            let arguments = arguments.trim_start();
            let Some(separator) = arguments.find(char::is_whitespace) else {
                return invalid();
            };
            let path = &arguments[..separator];
            if path.is_empty() {
                return invalid();
            }
            WorkspaceAction::CreateFile {
                path: PathBuf::from(path),
                contents: arguments[separator + 1..].to_owned(),
            }
        }
        "replace" => {
            let Some(arguments) = arguments else {
                return invalid_replace();
            };
            let arguments = arguments.trim_start();
            let Some((separator, whitespace)) = arguments
                .char_indices()
                .find(|(_, character)| character.is_whitespace())
            else {
                return invalid_replace();
            };
            let path = &arguments[..separator];
            let Some((match_text, replacement_text)) =
                arguments[separator + whitespace.len_utf8()..].split_once(" => ")
            else {
                return invalid_replace();
            };
            WorkspaceAction::ReplaceText {
                path: PathBuf::from(path),
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

fn parse_task_read_range(arguments: Option<&str>) -> Option<WorkspaceAction> {
    let arguments = arguments?.trim();
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
                writeln!(output, "  {:?}:{}", location.path, location.line)?;
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
        Some("task") => "usage: krypper task [-h] --goal GOAL --max-actions N",
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
            "usage: krypper task [-h] --goal GOAL --max-actions N\n\n",
            "run a caller-directed task across bounded turns in the current directory\n\n",
            "options:\n  --goal GOAL          required task goal\n",
            "  --max-actions N      required positive action limit for the whole task\n",
            "  -h, --help           show this help message and exit\n\n",
            "Enter one action per line on standard input:\n",
            "  read PATH\n  read-range PATH START_LINE MAX_LINES\n",
            "  list [PATH]\n  mkdir PATH\n  create PATH TEXT\n",
            "  replace PATH MATCH_TEXT => REPLACEMENT_TEXT\n",
            "  search DIRECTORY QUERY\n  continue\n  complete [RESULT]\n  cancel\n\n",
            "mkdir creates one directory and requires its parent to exist.\n",
            "read-range uses one-based lines, returns at most 200 lines and 64 KiB, and reports the actual range.\n",
            "replace requires one non-empty exact match; it prints only path and byte-count metadata.\n",
            "search returns relative paths and line numbers without matched text.\n",
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
