//! Opt-in Rust module mapping built on the existing workspace search and read boundaries.

use super::context::{
    is_oversized_error, read_workspace, revision_of, search_workspace, RepositoryContextError,
};
use crate::task_lifecycle::{TaskSession, WorkspaceSearchLocation};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const MAX_REPOSITORY_MAP_FILES: usize = 64;
const MAX_REPOSITORY_MAP_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_REPOSITORY_MAP_ENTRIES: usize = 128;
const MAX_REPOSITORY_MAP_CONTEXT_BYTES: usize = 4 * 1024;
const MIN_GOAL_TERM_BYTES: usize = 3;
const QUERY: &str = "mod ";
const RUST_RESERVED_KEYWORDS: [&str; 49] = [
    "abstract", "as", "become", "box", "break", "const", "continue", "crate", "do", "else", "enum",
    "extern", "false", "final", "fn", "for", "if", "impl", "in", "let", "loop", "macro", "match",
    "mod", "move", "mut", "override", "priv", "pub", "ref", "return", "Self", "self", "static",
    "struct", "super", "trait", "true", "try", "type", "typeof", "union", "unsafe", "unsized",
    "use", "virtual", "where", "while", "yield",
];

/// A bounded, opt-in index of out-of-line Rust module declarations.
///
/// File candidates use Rust's conventional module layout and are advisory; callers must still
/// verify a candidate through the existing workspace read operation. This map does not parse
/// Rust symbols or grant workspace access.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RepositoryModuleMap {
    pub modules: Vec<RepositoryModuleEntry>,
    pub files_examined: usize,
    /// Total bytes returned by safe reads, including a final file discarded at the source cap.
    pub source_bytes_read: usize,
    pub stale_skipped: usize,
    pub oversized_skipped: usize,
    /// Serialized module entries, capped at 4 KiB.
    pub bytes: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepositoryModuleEntry {
    pub name: String,
    pub declared_in: String,
    pub line: usize,
    /// SHA-256 prefix of the source file containing the declaration.
    pub revision: String,
    /// Conventional Rust paths only; `None` means a `#[path = ...]` attribute was present.
    pub conventional_path_candidates: Option<Vec<String>>,
}

/// Collects an opt-in Rust module map through the existing bounded workspace search and read
/// operations. The normal planner continues to use the existing bounded-search context.
pub fn collect_repository_module_map(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
) -> Result<RepositoryModuleMap, RepositoryContextError> {
    collect_repository_module_map_with_sources(
        goal,
        |query| search_workspace(session, task_id, turn_id, query),
        |path| read_workspace(session, task_id, turn_id, path),
    )
}

fn collect_repository_module_map_with_sources<F, G>(
    goal: &str,
    mut search: F,
    mut read: G,
) -> Result<RepositoryModuleMap, RepositoryContextError>
where
    F: FnMut(&str) -> Result<Vec<WorkspaceSearchLocation>, RepositoryContextError>,
    G: FnMut(&Path) -> Result<String, RepositoryContextError>,
{
    let mut map = RepositoryModuleMap::default();
    let locations = match search(QUERY) {
        Ok(locations) => locations,
        Err(RepositoryContextError::TaskUnavailable) => {
            return Err(RepositoryContextError::TaskUnavailable);
        }
        Err(RepositoryContextError::SearchFailed(message)) if is_oversized_error(&message) => {
            map.oversized_skipped = 1;
            map.truncated = true;
            return Ok(map);
        }
        Err(error) => return Err(error),
    };

    let mut locations_by_path = BTreeMap::<PathBuf, Vec<usize>>::new();
    for location in locations {
        if location.path.is_absolute()
            || location.path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || location
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("rs")
        {
            map.stale_skipped += 1;
            continue;
        }
        locations_by_path
            .entry(location.path)
            .or_default()
            .push(location.line);
    }
    if locations_by_path.len() > MAX_REPOSITORY_MAP_FILES {
        map.truncated = true;
    }

    let mut source_bytes = 0_usize;
    let mut entries = Vec::new();
    for (path, matched_lines) in locations_by_path.into_iter().take(MAX_REPOSITORY_MAP_FILES) {
        map.files_examined += 1;
        let contents = match read(&path) {
            Ok(contents) => contents,
            Err(RepositoryContextError::TaskUnavailable) => {
                return Err(RepositoryContextError::TaskUnavailable);
            }
            Err(RepositoryContextError::SearchFailed(message)) if is_oversized_error(&message) => {
                map.oversized_skipped += 1;
                map.truncated = true;
                continue;
            }
            Err(_) => {
                map.stale_skipped += 1;
                continue;
            }
        };
        let bytes_after_read = source_bytes.saturating_add(contents.len());
        map.source_bytes_read = bytes_after_read;
        if bytes_after_read > MAX_REPOSITORY_MAP_SOURCE_BYTES {
            map.truncated = true;
            break;
        }
        source_bytes = bytes_after_read;

        let lines = top_level_module_lines(&contents);
        let current_matches = matched_lines
            .into_iter()
            .filter(|line| {
                line.checked_sub(1)
                    .and_then(|index| lines.get(index))
                    .is_some_and(|contents| contents.contains(QUERY))
            })
            .collect::<BTreeSet<_>>();
        if current_matches.is_empty() {
            map.stale_skipped += 1;
            continue;
        }

        let revision = revision_of(&contents);
        let mut custom_path_attribute = false;
        for (index, line) in lines.iter().enumerate() {
            let line_number = index + 1;
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("#[") {
                custom_path_attribute |= trimmed.starts_with("#[path");
                continue;
            }
            if let Some(name) = parse_module_declaration(line) {
                if current_matches.contains(&line_number) {
                    entries.push(RepositoryModuleEntry {
                        name: name.clone(),
                        declared_in: path.to_string_lossy().into_owned(),
                        line: line_number,
                        revision: revision.clone(),
                        conventional_path_candidates: if custom_path_attribute {
                            None
                        } else {
                            Some(conventional_module_paths(&path, &name))
                        },
                    });
                }
                custom_path_attribute = false;
                continue;
            }
            custom_path_attribute = false;
        }
    }

    let goal_terms = goal_terms(goal);
    entries.sort_by_key(|entry| {
        (
            !goal_terms.contains(&entry.name.to_ascii_lowercase()),
            entry.declared_in.clone(),
            entry.line,
            entry.name.clone(),
        )
    });
    entries
        .dedup_by(|left, right| left.declared_in == right.declared_in && left.line == right.line);
    if entries.len() > MAX_REPOSITORY_MAP_ENTRIES {
        map.truncated = true;
        entries.truncate(MAX_REPOSITORY_MAP_ENTRIES);
    }

    for entry in entries {
        map.modules.push(entry);
        let bytes = serde_json::to_vec(&map.modules).map_or(usize::MAX, |encoded| encoded.len());
        if bytes > MAX_REPOSITORY_MAP_CONTEXT_BYTES {
            map.modules.pop();
            map.truncated = true;
            break;
        }
    }
    map.bytes = serde_json::to_vec(&map.modules).map_or(0, |encoded| encoded.len());
    Ok(map)
}

fn goal_terms(goal: &str) -> BTreeSet<String> {
    goal.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-'))
    })
    .map(|term| term.trim_matches(|character| matches!(character, ':' | '-')))
    .filter(|term| term.len() >= MIN_GOAL_TERM_BYTES)
    .map(str::to_owned)
    .map(|term| term.to_ascii_lowercase())
    .collect()
}

/// Keeps top-level Rust code lines while masking comments and string contents. This intentionally
/// stays a small lexical filter, not a Rust parser; nested inline modules are omitted because the
/// conventional path calculation below only applies at a source file's module root.
fn top_level_module_lines(source: &str) -> Vec<String> {
    let masked = mask_non_code(source);
    let mut brace_depth = 0_usize;
    masked
        .lines()
        .map(|line| {
            let is_top_level = brace_depth == 0;
            for byte in line.bytes() {
                match byte {
                    b'{' => brace_depth = brace_depth.saturating_add(1),
                    b'}' => brace_depth = brace_depth.saturating_sub(1),
                    _ => {}
                }
            }
            if is_top_level {
                line.to_owned()
            } else {
                String::new()
            }
        })
        .collect()
}

fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;
    let mut block_comment_depth = 0_usize;
    let mut raw_string_hashes = None;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        if block_comment_depth > 0 {
            if bytes.get(index..index + 2) == Some(b"/*") {
                mask_range(&mut masked, index, index + 2);
                block_comment_depth += 1;
                index += 2;
            } else if bytes.get(index..index + 2) == Some(b"*/") {
                mask_range(&mut masked, index, index + 2);
                block_comment_depth -= 1;
                index += 2;
            } else {
                mask_byte(&mut masked, index);
                index += 1;
            }
            continue;
        }

        if let Some(hash_count) = raw_string_hashes {
            if bytes[index] == b'"'
                && bytes
                    .get(index + 1..index + 1 + hash_count)
                    .is_some_and(|hashes| hashes.iter().all(|byte| *byte == b'#'))
            {
                mask_range(&mut masked, index, index + hash_count + 1);
                raw_string_hashes = None;
                index += hash_count + 1;
            } else {
                mask_byte(&mut masked, index);
                index += 1;
            }
            continue;
        }

        if in_string {
            let byte = bytes[index];
            mask_byte(&mut masked, index);
            index += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        if bytes.get(index..index + 2) == Some(b"//") {
            let end = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + offset);
            mask_range(&mut masked, index, end);
            index = end;
        } else if bytes.get(index..index + 2) == Some(b"/*") {
            mask_range(&mut masked, index, index + 2);
            block_comment_depth = 1;
            index += 2;
        } else if let Some((prefix_len, hash_count)) = raw_string_start(bytes, index) {
            mask_range(&mut masked, index, index + prefix_len);
            raw_string_hashes = Some(hash_count);
            index += prefix_len;
        } else if bytes[index] == b'"' {
            mask_byte(&mut masked, index);
            in_string = true;
            escaped = false;
            index += 1;
        } else if bytes[index] == b'\'' {
            if let Some(end) = character_literal_end(bytes, index) {
                mask_range(&mut masked, index, end);
                index = end;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }

    // All masked ranges replace complete UTF-8 spans and preserve line endings.
    String::from_utf8(masked).expect("masking preserves valid UTF-8")
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let raw_index = match bytes.get(index) {
        Some(b'r') => index,
        Some(b'b' | b'c') if bytes.get(index + 1) == Some(&b'r') => index + 1,
        _ => return None,
    };
    if index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_') {
        return None;
    }
    let mut quote_index = raw_index + 1;
    while bytes.get(quote_index) == Some(&b'#') {
        quote_index += 1;
    }
    if bytes.get(quote_index) != Some(&b'"') {
        return None;
    }
    Some((quote_index + 1 - index, quote_index - raw_index - 1))
}

fn character_literal_end(bytes: &[u8], quote_index: usize) -> Option<usize> {
    let mut character_index = quote_index + 1;
    let first = *bytes.get(character_index)?;
    if first == b'\n' || first == b'\r' {
        return None;
    }
    if first == b'\\' {
        character_index += 1;
        match bytes.get(character_index)? {
            b'u' if bytes.get(character_index + 1) == Some(&b'{') => {
                character_index += 2;
                let mut digit_count = 0;
                while bytes
                    .get(character_index)
                    .is_some_and(u8::is_ascii_hexdigit)
                    && digit_count < 6
                {
                    character_index += 1;
                    digit_count += 1;
                }
                if digit_count == 0 || bytes.get(character_index) != Some(&b'}') {
                    return None;
                }
                character_index += 1;
            }
            b'x' if bytes
                .get(character_index + 1..character_index + 3)
                .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit)) =>
            {
                character_index += 3;
            }
            b'x' => return None,
            _ => character_index += 1,
        }
    } else {
        character_index += if first < 0x80 {
            1
        } else if first & 0b1110_0000 == 0b1100_0000 {
            2
        } else if first & 0b1111_0000 == 0b1110_0000 {
            3
        } else {
            4
        };
    }
    (bytes.get(character_index) == Some(&b'\'')).then_some(character_index + 1)
}

fn mask_range(masked: &mut [u8], start: usize, end: usize) {
    for index in start..end {
        mask_byte(masked, index);
    }
}

fn mask_byte(masked: &mut [u8], index: usize) {
    if !matches!(masked[index], b'\n' | b'\r') {
        masked[index] = b' ';
    }
}

fn parse_module_declaration(line: &str) -> Option<String> {
    let mut declaration = line.trim();
    declaration = declaration
        .split_once("//")
        .map_or(declaration, |(head, _)| head)
        .trim();
    if let Some(rest) = declaration.strip_prefix("pub ") {
        declaration = rest;
    } else if let Some(rest) = declaration.strip_prefix("pub(") {
        declaration = rest.split_once(')')?.1.trim_start();
    }
    let (name, trailing) = declaration.strip_prefix("mod ")?.split_once(';')?;
    if !trailing.trim().is_empty() {
        return None;
    }
    let name = name.trim();
    let raw_identifier = name.strip_prefix("r#");
    let identifier = raw_identifier.unwrap_or(name);
    if raw_identifier.is_none() && RUST_RESERVED_KEYWORDS.contains(&identifier) {
        return None;
    }
    let mut characters = identifier.chars();
    let first = characters.next()?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(identifier.to_owned())
}

fn conventional_module_paths(declared_in: &Path, name: &str) -> Vec<String> {
    let parent = declared_in.parent().unwrap_or_else(|| Path::new("."));
    let file_name = declared_in.file_name().and_then(|name| name.to_str());
    let base = if matches!(file_name, Some("lib.rs" | "main.rs" | "mod.rs")) {
        parent.to_path_buf()
    } else {
        parent.join(declared_in.file_stem().unwrap_or_default())
    };
    [
        base.join(format!("{name}.rs")),
        base.join(name).join("mod.rs"),
    ]
    .into_iter()
    .map(|path| path.to_string_lossy().into_owned())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_lifecycle::WorkspaceSearchLocation;

    #[test]
    fn module_map_uses_attributed_declarations_and_goal_order() {
        let contents = concat!(
            "// mod comment_only;\n",
            "#[path = \"custom.rs\"]\n",
            "mod custom;\n",
            "pub mod api;\n",
            "pub(crate) mod parser; // module comment\n",
            "pub mod type;\n",
            "mod inline { fn ignored() {} }\n",
        );
        let locations = search_locations("src/lib.rs", contents);

        let map = collect_repository_module_map_with_sources(
            "Fix parser behavior",
            |query| {
                assert_eq!(query, QUERY);
                Ok(locations.clone())
            },
            |path| {
                assert_eq!(path, Path::new("src/lib.rs"));
                Ok(contents.to_owned())
            },
        )
        .expect("collect bounded module map");

        assert_eq!(map.files_examined, 1);
        assert_eq!(map.modules.len(), 3);
        assert_eq!(map.modules[0].name, "parser");
        assert_eq!(map.modules[0].declared_in, "src/lib.rs");
        assert_eq!(map.modules[0].line, 5);
        assert_eq!(map.modules[0].revision.len(), 12);
        assert_eq!(
            map.modules[0]
                .conventional_path_candidates
                .as_ref()
                .expect("conventional module paths"),
            &["src/parser.rs".to_owned(), "src/parser/mod.rs".to_owned()]
        );
        assert_eq!(map.modules[1].name, "custom");
        assert_eq!(map.modules[1].conventional_path_candidates, None);
        assert!(!map.modules.iter().any(|entry| entry.name == "inline"));
        assert!(!map.modules.iter().any(|entry| entry.name == "comment_only"));
        assert!(map.bytes <= MAX_REPOSITORY_MAP_CONTEXT_BYTES);
        assert!(!map.truncated);
    }

    #[test]
    fn module_map_ignores_block_comments_raw_strings_and_inline_children() {
        let contents = concat!(
            "/* module-like text\n",
            "mod block_comment;\n",
            "*/\n",
            "const EXAMPLE: &str = r###\"\n",
            "mod raw_string;\n",
            "\"###;\n",
            "mod outer {\n",
            "    mod inline_child;\n",
            "}\n",
            "pub mod real_child;\n",
        );
        let locations = search_locations("src/lib.rs", contents);
        let map = collect_repository_module_map_with_sources(
            "Locate modules",
            |_| Ok(locations.clone()),
            |_| Ok(contents.to_owned()),
        )
        .expect("comments and nested inline declarations are ignored");

        assert_eq!(map.modules.len(), 1);
        assert_eq!(map.modules[0].name, "real_child");
        assert!(map.modules.iter().all(|entry| {
            !["block_comment", "raw_string", "inline_child"].contains(&entry.name.as_str())
        }));
    }

    #[test]
    fn malformed_unicode_character_escape_does_not_hide_later_modules() {
        assert_eq!(character_literal_end(b"'\\u{not_hex}'", 0), None);
        assert_eq!(character_literal_end(b"'\\u{1234567}'", 0), None);
        assert_eq!(character_literal_end(b"'\\u{123456}'", 0), Some(12));
        let contents = "let invalid = '\\u{not_hex}';\nmod visible;\n";
        let locations = search_locations("src/lib.rs", contents);
        let map = collect_repository_module_map_with_sources(
            "Locate visible module",
            |_| Ok(locations.clone()),
            |_| Ok(contents.to_owned()),
        )
        .expect("invalid escape does not prevent later declarations from being scanned");

        assert_eq!(map.modules.len(), 1);
        assert_eq!(map.modules[0].name, "visible");
    }

    #[test]
    fn module_map_skips_stale_unsafe_and_non_rust_locations() {
        let mut read_count = 0;
        let locations = vec![
            WorkspaceSearchLocation {
                path: PathBuf::from("src/lib.rs"),
                line: 1,
            },
            WorkspaceSearchLocation {
                path: PathBuf::from("../outside.rs"),
                line: 1,
            },
            WorkspaceSearchLocation {
                path: PathBuf::from("/tmp/outside.rs"),
                line: 1,
            },
            WorkspaceSearchLocation {
                path: PathBuf::from("README.md"),
                line: 1,
            },
        ];
        let map = collect_repository_module_map_with_sources(
            "Locate parser",
            |_| Ok(locations.clone()),
            |_| {
                read_count += 1;
                Ok("pub use current;\n".to_owned())
            },
        )
        .expect("unsafe and stale hits are skipped");

        assert_eq!(read_count, 1);
        assert_eq!(map.stale_skipped, 4);
        assert!(map.modules.is_empty());
    }

    #[test]
    fn module_map_bounds_entry_and_file_counts() {
        let contents = (0..MAX_REPOSITORY_MAP_ENTRIES + 1)
            .map(|index| format!("pub mod item_{index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let locations = search_locations("src/lib.rs", &contents);
        let bounded = collect_repository_module_map_with_sources(
            "Locate item_127",
            |_| Ok(locations.clone()),
            |_| Ok(contents.clone()),
        )
        .expect("large module lists are truncated");

        assert!(bounded.modules.len() <= MAX_REPOSITORY_MAP_ENTRIES);
        assert!(bounded.bytes <= MAX_REPOSITORY_MAP_CONTEXT_BYTES);
        assert!(bounded.truncated);
        assert_eq!(bounded.modules[0].name, "item_127");

        let locations = (0..MAX_REPOSITORY_MAP_FILES + 1)
            .map(|index| WorkspaceSearchLocation {
                path: PathBuf::from(format!("src/module_{index}.rs")),
                line: 1,
            })
            .collect::<Vec<_>>();
        let mut reads = 0;
        let file_bounded = collect_repository_module_map_with_sources(
            "Locate module",
            |_| Ok(locations.clone()),
            |_| {
                reads += 1;
                Ok("pub mod nested;\n".to_owned())
            },
        )
        .expect("module map stops at the file limit");
        assert_eq!(file_bounded.files_examined, MAX_REPOSITORY_MAP_FILES);
        assert_eq!(reads, MAX_REPOSITORY_MAP_FILES);
        assert!(file_bounded.truncated);
    }

    #[test]
    fn module_map_stops_after_its_source_byte_budget_plus_one_file_read() {
        const ONE_MIB: usize = 1024 * 1024;
        let contents = format!("pub mod large;\n{}", "x".repeat(ONE_MIB - 15));
        assert_eq!(contents.len(), ONE_MIB);
        let locations = (0..4)
            .map(|index| WorkspaceSearchLocation {
                path: PathBuf::from(format!("src/module_{index}.rs")),
                line: 1,
            })
            .collect::<Vec<_>>();
        let mut reads = 0;
        let map = collect_repository_module_map_with_sources(
            "Locate large module",
            |_| Ok(locations.clone()),
            |_| {
                reads += 1;
                Ok(contents.clone())
            },
        )
        .expect("source byte budget truncates safely");

        assert_eq!(reads, 3);
        assert_eq!(map.files_examined, 3);
        assert_eq!(map.source_bytes_read, 3 * ONE_MIB);
        assert!(map.truncated);
    }

    #[test]
    fn module_map_reports_stale_and_oversized_search_results() {
        let stale = collect_repository_module_map_with_sources(
            "Find parser",
            |_| {
                Ok(vec![WorkspaceSearchLocation {
                    path: PathBuf::from("src/lib.rs"),
                    line: 1,
                }])
            },
            |_| Ok("pub use current;\n".to_owned()),
        )
        .expect("stale module hits are skipped");
        assert_eq!(stale.stale_skipped, 1);
        assert!(stale.modules.is_empty());

        let oversized = collect_repository_module_map_with_sources(
            "Find parser",
            |_| {
                Err(RepositoryContextError::SearchFailed(
                    "search exceeds 16 MiB scan limit".to_owned(),
                ))
            },
            |_| unreachable!("oversized search does not read files"),
        )
        .expect("oversized search degrades to a truncated map");
        assert_eq!(oversized.oversized_skipped, 1);
        assert!(oversized.truncated);
        assert!(oversized.modules.is_empty());
    }

    fn search_locations(path: &str, contents: &str) -> Vec<WorkspaceSearchLocation> {
        contents
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(QUERY))
            .map(|(index, _)| WorkspaceSearchLocation {
                path: PathBuf::from(path),
                line: index + 1,
            })
            .collect()
    }
}
