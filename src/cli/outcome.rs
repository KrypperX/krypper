use crate::agent_loop::WorkspaceObservation;
use crate::task_lifecycle::TurnOutcome;
use std::io::{self, Write};
use std::path::Path;

pub(super) fn write_turn_outcome(
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

pub(super) fn quoted_path_token(path: &Path) -> String {
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

pub(super) fn write_observation(
    output: &mut dyn Write,
    number: usize,
    observation: &WorkspaceObservation,
) -> io::Result<()> {
    match observation {
        WorkspaceObservation::ProcessFinished { result } => {
            writeln!(
                output,
                "Observation {number}: offline Cargo {:?}; filter={:?}; tests_ran={} executed_tests={:?}; cleanup={} truncated={}",
                result.outcome,
                result.test_filter,
                result.tests_ran,
                result.executed_tests,
                result.cleanup_confirmed,
                result.truncated
            )?;
            writeln!(
                output,
                "stdout: {:?}\nstderr: {:?}",
                result.stdout, result.stderr
            )?;
        }
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
        WorkspaceObservation::PatchApplied {
            path,
            bytes_written,
            ..
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
        WorkspaceObservation::PatchFailed { path, error } => {
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
        WorkspaceObservation::ExtensionCompleted { name, result } => {
            writeln!(output, "Observation {number}: extension {name} completed")?;
            output.write_all(result.as_bytes())?;
            if !result.ends_with('\n') {
                output.write_all(b"\n")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{quoted_path_token, write_observation, write_turn_outcome};
    use crate::agent_loop::{GoalTurnError, WorkspaceAction, WorkspaceObservation};
    use crate::task_lifecycle::{TurnOutcome, WorkspaceSearchLocation};
    use crate::{ProcessOutcome, ProcessResult};
    use std::ffi::OsString;
    use std::io::{self, Write};
    use std::path::PathBuf;

    #[test]
    fn turn_outcome_variants_keep_the_existing_text_and_newline() {
        let cases = [
            (
                Some(TurnOutcome::Success {
                    result: "finished".to_owned(),
                }),
                "prefixsuccess: finished\n",
            ),
            (
                Some(TurnOutcome::Failure {
                    detail: "could not finish".to_owned(),
                }),
                "prefixfailure: could not finish\n",
            ),
            (None, "prefixcancelled by caller\n"),
        ];

        for (outcome, expected) in cases {
            let mut output = Vec::new();
            write_turn_outcome(&mut output, "prefix", outcome.as_ref()).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), expected);
        }
    }

    #[test]
    fn observation_variants_keep_the_existing_output() {
        let error = GoalTurnError::UnsupportedAction {
            action: "fixture".to_owned(),
        };
        let observations = [
            (
                WorkspaceObservation::ProcessFinished {
                    result: ProcessResult {
                        run_id: uuid::Uuid::new_v4(),
                        outcome: ProcessOutcome::NoTestsMatched,
                        stdout: String::new(),
                        stderr: String::new(),
                        truncated: false,
                        diagnostic: Some("test filter executed no tests".to_owned()),
                        cleanup_confirmed: true,
                        test_filter: Some("absent_test".to_owned()),
                        tests_ran: false,
                        executed_tests: Some(0),
                        elapsed_ms: 0,
                        output_bytes: 0,
                        output_limit: 0,
                        resource_events: Vec::new(),
                        recovery_hold: false,
                        sandbox_evidence: None,
                    },
                },
                "Observation 3: offline Cargo NoTestsMatched; filter=Some(\"absent_test\"); tests_ran=false executed_tests=Some(0); cleanup=true truncated=false\nstdout: \"\"\nstderr: \"\"\n",
            ),
            (
                WorkspaceObservation::FileRead {
                    path: PathBuf::from("read file.txt"),
                    contents: "first\nsecond".to_owned(),
                },
                "Observation 3: read read file.txt\n  contents:\nfirst\nsecond\n",
            ),
            (
                WorkspaceObservation::FileRangeRead {
                    path: PathBuf::from("range.txt"),
                    start_line: 2,
                    end_line: 3,
                    excerpt: "second\n".to_owned(),
                },
                "Observation 3: read-range range.txt lines 2-3\nsecond\n",
            ),
            (
                WorkspaceObservation::DirectoryListed {
                    path: PathBuf::from("workspace"),
                    entries: vec![OsString::from("alpha"), OsString::from("nested dir")],
                },
                "Observation 3: listed workspace (2 entries)\n  \"alpha\"\n  \"nested dir\"\n",
            ),
            (
                WorkspaceObservation::DirectoryCreated {
                    path: PathBuf::from("created"),
                },
                "Observation 3: created directory created\n",
            ),
            (
                WorkspaceObservation::TextSearched {
                    directory: PathBuf::from("workspace"),
                    locations: vec![WorkspaceSearchLocation {
                        path: PathBuf::from("nested/file name.txt"),
                        line: 4,
                    }],
                },
                "Observation 3: search in workspace found 1 locations\n  \"nested/file name.txt\":4\n",
            ),
            (
                WorkspaceObservation::TreeTextSearched {
                    directory: PathBuf::from("workspace"),
                    locations: vec![WorkspaceSearchLocation {
                        path: PathBuf::from("nested/quoted \"file\".txt"),
                        line: 5,
                    }],
                },
                "Observation 3: search-tree in workspace found 1 locations\n  \"nested/quoted \\\"file\\\".txt\":5\n",
            ),
            (
                WorkspaceObservation::FileCreated {
                    path: PathBuf::from("new file.txt"),
                    bytes_written: 8,
                },
                "Observation 3: created new file.txt (8 bytes)\n",
            ),
            (
                WorkspaceObservation::TextReplaced {
                    path: PathBuf::from("replace.txt"),
                    bytes_written: 7,
                },
                "Observation 3: replaced text in replace.txt (7 bytes)\n",
            ),
            (
                WorkspaceObservation::PatchApplied {
                    path: PathBuf::from("patch.txt"),
                    bytes_written: 7,
                    before_diff: "old".to_owned(),
                    after_diff: "new".to_owned(),
                },
                "Observation 3: replaced text in patch.txt (7 bytes)\n",
            ),
            (
                WorkspaceObservation::FileCreationFailed {
                    path: PathBuf::from("create.txt"),
                    error: error.clone(),
                },
                "Observation 3: create create.txt failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::DirectoryCreationFailed {
                    path: PathBuf::from("mkdir"),
                    error: error.clone(),
                },
                "Observation 3: mkdir mkdir failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::TextReplacementFailed {
                    path: PathBuf::from("replace.txt"),
                    error: error.clone(),
                },
                "Observation 3: replacement in replace.txt failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::PatchFailed {
                    path: PathBuf::from("patch.txt"),
                    error: error.clone(),
                },
                "Observation 3: replacement in patch.txt failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::TextSearchFailed {
                    directory: PathBuf::from("workspace"),
                    error: error.clone(),
                },
                "Observation 3: search in workspace failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::TreeTextSearchFailed {
                    directory: PathBuf::from("workspace"),
                    error: error.clone(),
                },
                "Observation 3: search-tree in workspace failed: unsupported workspace action 'fixture'\n",
            ),
            (
                WorkspaceObservation::ActionFailed {
                    action: WorkspaceAction::Unsupported {
                        action: "not displayed".to_owned(),
                    },
                    error,
                },
                "Observation 3: action failed: unsupported workspace action 'fixture'\n",
            ),
        ];

        for (observation, expected) in observations {
            let mut output = Vec::new();
            write_observation(&mut output, 3, &observation).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), expected);
        }
    }

    #[test]
    fn quoted_path_tokens_escape_spaces_quotes_backslashes_and_controls() {
        let path = PathBuf::from(
            "dir with spaces/quote\"/back\\slash/\u{0008}\u{000c}\n\r\t\u{0000}\u{001f}\u{007f}",
        );

        assert_eq!(
            quoted_path_token(&path),
            "\"dir with spaces/quote\\\"/back\\\\slash/\\b\\f\\n\\r\\t\\u0000\\u001f\\u007f\""
        );
    }

    #[test]
    fn rendering_propagates_writer_errors() {
        struct BrokenWriter;

        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut output = BrokenWriter;
        let error = write_turn_outcome(&mut output, "", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
