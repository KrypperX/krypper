use crate::agent_loop::WorkspaceAction;
use std::path::PathBuf;

pub(super) fn parse_task_action(line: &str) -> WorkspaceAction {
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

#[cfg(test)]
mod tests {
    use super::parse_task_action;
    use crate::agent_loop::{WorkspaceAction, WorkspaceObservation};
    use crate::cli::outcome::{quoted_path_token, write_observation};
    use crate::task_lifecycle::WorkspaceSearchLocation;
    use std::path::{Path, PathBuf};

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
    fn malformed_ranges_and_searches_keep_action_specific_errors() {
        assert!(matches!(
            parse_task_action("read-range note.txt 2"),
            WorkspaceAction::Unsupported { action }
                if action == "invalid input: read-range note.txt 2"
        ));
        assert!(matches!(
            parse_task_action(r#"read-range "note with spaces.txt" 2 3 trailing"#),
            WorkspaceAction::Unsupported { action }
                if action == "invalid quoted path arguments"
        ));
        assert!(matches!(
            parse_task_action("search"),
            WorkspaceAction::Unsupported { action } if action == "invalid search action"
        ));
        assert!(matches!(
            parse_task_action("search-tree ."),
            WorkspaceAction::Unsupported { action }
                if action == "invalid search-tree action"
        ));
    }

    #[test]
    fn parsing_does_not_require_paths_to_exist() {
        assert!(matches!(
            parse_task_action("read path-that-is-not-created-by-the-parser.txt"),
            WorkspaceAction::ReadFile { path }
                if path == Path::new("path-that-is-not-created-by-the-parser.txt")
        ));
    }
}
