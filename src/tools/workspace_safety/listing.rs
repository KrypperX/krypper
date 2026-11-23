//! Directory listing with the Python-compatible JSON escaping used by the tools.

use std::ffi::{OsStr, OsString};
use std::fs::File;

use super::paths::{open_cwd_relative, open_workspace_relative};
use super::syscalls::DirectoryStream;
use super::types::{CliError, WorkspaceRoot, MAX_TASK_DIRECTORY_ENTRIES};

pub(crate) fn list_directory(path: &OsStr) -> Result<Vec<String>, CliError> {
    list_directory_entries(path).map(|names| {
        names
            .into_iter()
            .map(|name| quote_json(&name.sort_key))
            .collect()
    })
}

pub(crate) fn list_task_directory_names_at(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<Vec<OsString>, CliError> {
    list_open_directory_with_limit(
        open_workspace_relative(root, path, true)?,
        Some(MAX_TASK_DIRECTORY_ENTRIES),
    )
    .map(|names| names.into_iter().map(|name| name.name).collect())
}

pub(super) fn list_directory_entries(path: &OsStr) -> Result<Vec<ListedName>, CliError> {
    list_open_directory(open_cwd_relative(path, true)?)
}

pub(super) fn list_open_directory(directory: File) -> Result<Vec<ListedName>, CliError> {
    list_open_directory_with_limit(directory, None)
}

pub(super) fn list_open_directory_with_limit(
    directory: File,
    maximum_entries: Option<usize>,
) -> Result<Vec<ListedName>, CliError> {
    list_open_directory_with_limit_and_error(
        directory,
        maximum_entries,
        "directory exceeds 1,024 entries",
    )
}

pub(super) fn list_open_directory_with_limit_and_error(
    directory: File,
    maximum_entries: Option<usize>,
    limit_error: &'static str,
) -> Result<Vec<ListedName>, CliError> {
    let mut stream =
        DirectoryStream::open(directory).map_err(|_| CliError::new("unable to list directory"))?;
    let mut names = Vec::new();

    while let Some((name, sort_key)) = stream
        .next_entry()
        .map_err(|_| CliError::new("unable to list directory"))?
    {
        if name != OsStr::new(".") && name != OsStr::new("..") {
            if maximum_entries.is_some_and(|maximum| names.len() == maximum) {
                return Err(CliError::new(limit_error));
            }
            names.push(ListedName { sort_key, name });
        }
    }

    names.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(names)
}

pub(super) struct ListedName {
    pub(super) sort_key: Vec<u32>,
    pub(super) name: OsString,
}

pub(super) fn quote_json(codepoints: &[u32]) -> String {
    let mut quoted = String::from("\"");
    for codepoint in codepoints {
        match *codepoint {
            0x08 => quoted.push_str("\\b"),
            0x09 => quoted.push_str("\\t"),
            0x0a => quoted.push_str("\\n"),
            0x0c => quoted.push_str("\\f"),
            0x0d => quoted.push_str("\\r"),
            0x00..=0x1f => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x20..=0x7e if *codepoint != u32::from(b'"') && *codepoint != u32::from(b'\\') => {
                quoted.push(
                    char::from_u32(*codepoint).expect("invariant: ASCII is a Unicode scalar"),
                );
            }
            0x22 => quoted.push_str("\\\""),
            0x5c => quoted.push_str("\\\\"),
            0x7f..=0xffff => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x10000..=0x10ffff => {
                let scalar = codepoint - 0x10000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                quoted.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
            _ => quoted.push('\u{fffd}'),
        }
    }
    quoted.push('"');
    quoted
}
