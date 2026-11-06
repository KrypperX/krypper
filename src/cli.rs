use crate::file_safety::{create_text_file, list_directory, read_text_file};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};

const MAX_FILE_SIZE: usize = 1024 * 1024;

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
        _ if command_name.starts_with('-') => usage_error(
            error,
            None,
            &format!("unrecognized arguments: {command_name}"),
        ),
        _ => usage_error(
            error,
            None,
            &format!("invalid choice: '{command_name}' (choose from 'show', 'list', 'write')"),
        ),
    }
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
        _ => "usage: krypper [-h] {show,list,write} ...",
    };
    let program = command.map_or_else(|| "krypper".to_owned(), |name| format!("krypper {name}"));
    writeln!(error, "{usage}")?;
    writeln!(error, "{program}: error: {message}")?;
    Ok(2)
}

fn write_root_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper [-h] {show,list,write} ...\n\nLocal command-line entry point for Krypper.\n\npositional arguments:\n  {show,list,write}\n    show        display a UTF-8 text file\n    list        list immediate directory entries\n    write       create a new UTF-8 text file\n\noptions:\n  -h, --help    show this help message and exit\n",
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
    use std::io::Cursor;

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
}
