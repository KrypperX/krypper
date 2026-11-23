pub(crate) const MAX_PATCH_BYTES: usize = 32 * 1024;
const MAX_PATCH_HUNKS: usize = 128;

#[derive(Debug)]
pub(crate) struct PatchResult {
    pub(crate) contents: String,
    pub(crate) before_diff: String,
    pub(crate) after_diff: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineEnding {
    Lf,
    CrLf,
    None,
}

impl LineEnding {
    fn text(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::None => "",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileLine {
    text: String,
    ending: LineEnding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PatchLine<'a> {
    Context(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HunkHeader {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

pub(crate) fn apply_unified_diff(
    path: &str,
    original: &str,
    patch: &str,
) -> Result<PatchResult, &'static str> {
    if patch.is_empty() {
        return Err("patch must not be empty");
    }
    if patch.len() > MAX_PATCH_BYTES {
        return Err("patch exceeds 32 KiB");
    }
    if patch.contains('\0') || path.contains(['\0', '\n', '\r']) {
        return Err("patch contains an invalid path or byte");
    }

    let lines = split_patch_lines(patch);
    if lines.len() < 3 || lines[0] != format!("--- a/{path}") || lines[1] != format!("+++ b/{path}")
    {
        return Err("patch must contain one file header matching the requested path");
    }

    let original_lines = split_file_lines(original);
    let final_newline = original.ends_with('\n');
    let default_ending = original_lines
        .iter()
        .map(|line| line.ending)
        .find(|ending| *ending != LineEnding::None)
        .unwrap_or(LineEnding::Lf);
    let mut output_lines = Vec::with_capacity(original_lines.len());
    let mut before_diff = String::new();
    let mut after_diff = String::new();
    let mut before_first = true;
    let mut after_first = true;
    let mut source_cursor = 0;
    let mut output_line_count = 0_usize;
    let mut previous_hunk: Option<(usize, usize)> = None;
    let mut has_change = false;
    let mut hunk_count = 0;
    let mut index = 2;

    while index < lines.len() {
        if hunk_count == MAX_PATCH_HUNKS {
            return Err("patch contains too many hunks");
        }
        let header = parse_hunk_header(lines[index])?;
        index += 1;
        hunk_count += 1;

        let mut hunk_lines = Vec::new();
        while index < lines.len() && !lines[index].starts_with("@@") {
            let line = lines[index];
            let (prefix, text) = line.split_at_checked(1).ok_or("invalid hunk line")?;
            let text = text.strip_suffix('\r').unwrap_or(text);
            hunk_lines.push(match prefix {
                " " => PatchLine::Context(text),
                "-" => PatchLine::Remove(text),
                "+" => PatchLine::Add(text),
                _ => return Err("unsupported unified diff line"),
            });
            index += 1;
        }
        if hunk_lines.is_empty() {
            return Err("patch hunk must not be empty");
        }

        let actual_old_count = hunk_lines
            .iter()
            .filter(|line| !matches!(line, PatchLine::Add(_)))
            .count();
        let actual_new_count = hunk_lines
            .iter()
            .filter(|line| !matches!(line, PatchLine::Remove(_)))
            .count();
        if actual_old_count != header.old_count || actual_new_count != header.new_count {
            return Err("patch hunk line counts do not match its header");
        }
        if !hunk_lines
            .iter()
            .any(|line| matches!(line, PatchLine::Add(_) | PatchLine::Remove(_)))
        {
            return Err("patch hunk does not change any lines");
        }
        has_change = true;

        let old_index = if header.old_count == 0 {
            header.old_start
        } else {
            header
                .old_start
                .checked_sub(1)
                .ok_or("invalid old hunk location")?
        };
        if old_index < source_cursor || old_index > original_lines.len() {
            return Err("patch hunks overlap or are outside the file");
        }
        if previous_hunk.is_some_and(|(start, count)| {
            start == old_index && (count == 0 || header.old_count == 0)
        }) {
            return Err("patch contains ambiguous hunks at the same location");
        }

        let unchanged = &original_lines[source_cursor..old_index];
        output_line_count = output_line_count
            .checked_add(unchanged.len())
            .ok_or("patch line count overflow")?;
        output_lines.extend(unchanged.iter().cloned());

        let expected_new_start = if header.new_count == 0 {
            output_line_count
        } else {
            output_line_count
                .checked_add(1)
                .ok_or("patch line count overflow")?
        };
        if header.new_start != expected_new_start {
            return Err("patch new hunk location does not match its contents");
        }

        let mut source_index = old_index;
        for line in hunk_lines {
            match line {
                PatchLine::Context(text) => {
                    let source_line = matching_source_line(&original_lines, source_index, text)?;
                    append_evidence_line(&mut before_diff, &mut before_first, text);
                    append_evidence_line(&mut after_diff, &mut after_first, text);
                    output_lines.push(source_line.clone());
                    source_index += 1;
                    output_line_count += 1;
                }
                PatchLine::Remove(text) => {
                    matching_source_line(&original_lines, source_index, text)?;
                    append_evidence_line(&mut before_diff, &mut before_first, text);
                    source_index += 1;
                }
                PatchLine::Add(text) => {
                    append_evidence_line(&mut after_diff, &mut after_first, text);
                    output_lines.push(FileLine {
                        text: text.to_owned(),
                        ending: default_ending,
                    });
                    output_line_count += 1;
                }
            }
        }

        if source_index - old_index != header.old_count {
            return Err("patch hunk did not consume its declared old lines");
        }
        source_cursor = source_index;
        previous_hunk = Some((old_index, header.old_count));
    }

    if hunk_count == 0 || !has_change {
        return Err("patch must contain at least one changed hunk");
    }
    output_lines.extend(original_lines[source_cursor..].iter().cloned());
    normalize_final_line_ending(&mut output_lines, final_newline, default_ending);

    let contents = serialize_file_lines(&output_lines);
    if contents == original {
        return Err("patch does not change the file");
    }

    Ok(PatchResult {
        contents,
        before_diff,
        after_diff,
    })
}

fn split_patch_lines(patch: &str) -> Vec<&str> {
    patch
        .split_inclusive('\n')
        .map(|line| {
            let line = line.strip_suffix('\n').unwrap_or(line);
            line.strip_suffix('\r').unwrap_or(line)
        })
        .collect()
}

fn split_file_lines(contents: &str) -> Vec<FileLine> {
    let mut lines = Vec::new();
    let mut remaining = contents;
    while let Some(newline) = remaining.find('\n') {
        let line = &remaining[..newline];
        let (text, ending) = match line.strip_suffix('\r') {
            Some(text) => (text, LineEnding::CrLf),
            None => (line, LineEnding::Lf),
        };
        lines.push(FileLine {
            text: text.to_owned(),
            ending,
        });
        remaining = &remaining[newline + 1..];
    }
    if !remaining.is_empty() {
        lines.push(FileLine {
            text: remaining.to_owned(),
            ending: LineEnding::None,
        });
    }
    lines
}

fn parse_hunk_header(line: &str) -> Result<HunkHeader, &'static str> {
    let contents = line
        .strip_prefix("@@ ")
        .ok_or("unsupported unified diff header")?;
    let close = contents
        .find(" @@")
        .ok_or("invalid unified diff hunk header")?;
    let mut ranges = contents[..close].split_whitespace();
    let (old_start, old_count) = parse_range(
        ranges
            .next()
            .and_then(|range| range.strip_prefix('-'))
            .ok_or("invalid old hunk range")?,
    )?;
    let (new_start, new_count) = parse_range(
        ranges
            .next()
            .and_then(|range| range.strip_prefix('+'))
            .ok_or("invalid new hunk range")?,
    )?;
    if ranges.next().is_some()
        || (old_count > 0 && old_start == 0)
        || (new_count > 0 && new_start == 0)
    {
        return Err("invalid unified diff hunk range");
    }
    Ok(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

fn parse_range(range: &str) -> Result<(usize, usize), &'static str> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, count),
        None => (range, "1"),
    };
    if count.contains(',') {
        return Err("invalid unified diff line range");
    }
    let start = start
        .parse::<usize>()
        .map_err(|_| "invalid unified diff line range")?;
    let count = count
        .parse::<usize>()
        .map_err(|_| "invalid unified diff line range")?;
    Ok((start, count))
}

fn matching_source_line<'a>(
    lines: &'a [FileLine],
    index: usize,
    expected: &str,
) -> Result<&'a FileLine, &'static str> {
    let line = lines
        .get(index)
        .ok_or("patch context is missing at its declared location")?;
    if line.text != expected {
        return Err("patch context is stale at its declared location");
    }
    Ok(line)
}

fn append_evidence_line(target: &mut String, first: &mut bool, line: &str) {
    if !*first {
        target.push('\n');
    }
    target.push_str(line);
    *first = false;
}

fn normalize_final_line_ending(
    lines: &mut [FileLine],
    final_newline: bool,
    default_ending: LineEnding,
) {
    let last_index = lines.len().saturating_sub(1);
    lines.iter_mut().enumerate().for_each(|(index, line)| {
        if index < last_index && line.ending == LineEnding::None {
            line.ending = default_ending;
        }
        if index == last_index {
            if final_newline && line.ending == LineEnding::None {
                line.ending = default_ending;
            } else if !final_newline {
                line.ending = LineEnding::None;
            }
        }
    });
}

fn serialize_file_lines(lines: &[FileLine]) -> String {
    let mut contents = String::new();
    lines.iter().for_each(|line| {
        contents.push_str(&line.text);
        contents.push_str(line.ending.text());
    });
    contents
}

#[cfg(test)]
mod tests {
    use super::apply_unified_diff;
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::os::fd::RawFd;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn applies_exact_hunks_and_preserves_crlf_and_unrelated_lines() {
        let before = "top\r\nkeep\r\nold\r\nbottom\r\n";
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -2,2 +2,2 @@\n keep\n-old\n+new\n";

        let result = apply_unified_diff("note.txt", before, patch).unwrap();

        assert_eq!(result.contents, "top\r\nkeep\r\nnew\r\nbottom\r\n");
        assert_eq!(result.before_diff, "keep\nold");
        assert_eq!(result.after_diff, "keep\nnew");
    }

    #[test]
    fn preserves_lf_and_a_missing_final_newline() {
        let result = apply_unified_diff(
            "note.txt",
            "one\nold",
            "--- a/note.txt\n+++ b/note.txt\n@@ -2 +2 @@\n-old\n+new\n",
        )
        .unwrap();

        assert_eq!(result.contents, "one\nnew");
    }

    #[test]
    fn rejects_stale_context_at_its_declared_location_without_fuzzy_search() {
        let result = apply_unified_diff(
            "note.txt",
            "first\nrepeated\nrepeated\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-repeated\n+changed\n",
        );

        assert_eq!(
            result.unwrap_err(),
            "patch context is stale at its declared location"
        );
    }

    #[test]
    fn rejects_missing_hunk_lines_and_inconsistent_new_locations() {
        let missing = apply_unified_diff(
            "note.txt",
            "only\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -2 +2 @@\n-old\n+new\n",
        );
        let inconsistent = apply_unified_diff(
            "note.txt",
            "old\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +9 @@\n-old\n+new\n",
        );

        assert!(missing.is_err());
        assert_eq!(
            inconsistent.unwrap_err(),
            "patch new hunk location does not match its contents"
        );
    }

    #[test]
    fn rejects_overlapping_and_ambiguous_hunks() {
        let overlapping = apply_unified_diff(
            "note.txt",
            "a\nb\nc\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -2 +2 @@\n-b\n+C\n",
        );
        let ambiguous = apply_unified_diff(
            "note.txt",
            "a\nb\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1,0 +2 @@\n+one\n@@ -1,0 +3 @@\n+two\n",
        );

        assert!(overlapping.unwrap_err().contains("overlap"));
        assert_eq!(
            ambiguous.unwrap_err(),
            "patch contains ambiguous hunks at the same location"
        );
    }

    #[test]
    fn rejects_create_delete_multifile_binary_and_no_newline_forms() {
        let create = apply_unified_diff(
            "note.txt",
            "old\n",
            "--- /dev/null\n+++ b/note.txt\n@@ -0,0 +1 @@\n+new\n",
        );
        let delete = apply_unified_diff(
            "note.txt",
            "old\n",
            "--- a/note.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
        );
        let multi_file = apply_unified_diff(
            "note.txt",
            "old\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n--- a/other.txt\n+++ b/other.txt\n",
        );
        let binary = apply_unified_diff("note.txt", "old\n", "GIT binary patch\n");
        let missing_newline = apply_unified_diff(
            "note.txt",
            "old",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n",
        );

        assert!(create.is_err());
        assert!(delete.is_err());
        assert!(multi_file.is_err());
        assert!(binary.is_err());
        assert!(missing_newline.is_err());
    }

    #[test]
    fn rejects_oversized_patch_before_parsing() {
        let patch = "x".repeat(32 * 1024 + 1);

        assert_eq!(
            apply_unified_diff("note.txt", "old\n", &patch).unwrap_err(),
            "patch exceeds 32 KiB"
        );
    }

    #[test]
    fn applies_patch_atomically_with_permissions_and_bounded_evidence() {
        let (directory, root) = create_workspace("apply");
        let file = directory.join("source file.txt");
        let outside = directory.join("outside.txt");
        let original = b"top\r\nkeep\r\nold\r\nbottom\r\n";
        fs::write(&file, original).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&outside, "canary").unwrap();
        let patch =
            "--- a/source file.txt\n+++ b/source file.txt\n@@ -2,2 +2,2 @@\n keep\n-old\n+new\n";

        let result =
            super::super::apply_patch_file_at(&root, OsStr::new("source file.txt"), patch).unwrap();

        assert_eq!(
            fs::read(&file).unwrap(),
            b"top\r\nkeep\r\nnew\r\nbottom\r\n"
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "canary");
        assert_eq!(result.bytes_written, fs::read(&file).unwrap().len());
        assert_eq!(result.before_diff, "keep\nold");
        assert_eq!(result.after_diff, "keep\nnew");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_unauthorized_paths_invalid_utf8_and_oversized_patches_without_changes() {
        let (directory, root) = create_workspace("boundaries");
        let file = directory.join("note.txt");
        let outside = directory.join("outside.txt");
        fs::write(&file, "old\n").unwrap();
        fs::write(&outside, "canary").unwrap();
        symlink(&outside, directory.join("link.txt")).unwrap();
        fs::write(directory.join("invalid.txt"), [0xff, 0xfe]).unwrap();
        let valid = |path: &str| format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n");
        let oversized = "x".repeat(super::MAX_PATCH_BYTES + 1);

        assert!(super::super::apply_patch_file_at(
            &root,
            OsStr::new("../outside.txt"),
            &valid("../outside.txt")
        )
        .is_err());
        assert!(super::super::apply_patch_file_at(
            &root,
            outside.as_os_str(),
            &valid(outside.to_str().unwrap())
        )
        .is_err());
        assert!(super::super::apply_patch_file_at(
            &root,
            OsStr::new("link.txt"),
            &valid("link.txt")
        )
        .is_err());
        assert!(super::super::apply_patch_file_at(
            &root,
            OsStr::new("invalid.txt"),
            &valid("invalid.txt")
        )
        .is_err());
        assert!(
            super::super::apply_patch_file_at(&root, OsStr::new("note.txt"), &oversized).is_err()
        );

        let rejected_patches = [
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-stale\n+new\n",
            "--- /dev/null\n+++ b/note.txt\n@@ -0,0 +1 @@\n+created\n",
            "--- a/note.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n--- a/other.txt\n+++ b/other.txt\n",
            "GIT binary patch\n",
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n",
        ];
        for patch in rejected_patches {
            assert!(
                super::super::apply_patch_file_at(&root, OsStr::new("note.txt"), patch).is_err()
            );
            assert_eq!(fs::read(&file).unwrap(), b"old\n");
            assert_eq!(fs::read(&outside).unwrap(), b"canary");
        }

        assert_eq!(fs::read(&file).unwrap(), b"old\n");
        assert_eq!(fs::read(&outside).unwrap(), b"canary");
        assert_eq!(
            fs::read(directory.join("invalid.txt")).unwrap(),
            [0xff, 0xfe]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_write_commit_and_readback_never_report_patch_success() {
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let (directory, root) = create_workspace("failures");
        let file = directory.join("note.txt");
        fs::write(&file, "old\n").unwrap();

        let write_failure = super::super::apply_patch_file_at_with_ops(
            &root,
            OsStr::new("note.txt"),
            patch,
            |_fd: RawFd, _bytes: &[u8]| Err(io::Error::other("simulated write failure")),
            super::super::rename_file_at,
            super::super::read_file_contents_at_for_verification,
        );
        assert!(write_failure.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"old\n");

        let commit_failure = super::super::apply_patch_file_at_with_ops(
            &root,
            OsStr::new("note.txt"),
            patch,
            super::super::write_to_fd,
            |_fd: RawFd, _temporary: &OsStr, _target: &OsStr| {
                Err(io::Error::other("simulated uncertain commit"))
            },
            super::super::read_file_contents_at_for_verification,
        );
        assert!(commit_failure.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"old\n");

        let readback_failure = super::super::apply_patch_file_at_with_ops(
            &root,
            OsStr::new("note.txt"),
            patch,
            super::super::write_to_fd,
            |fd: RawFd, temporary: &OsStr, target: &OsStr| {
                super::super::rename_file_at(fd, temporary, target)?;
                Err(io::Error::other("simulated lost commit response"))
            },
            super::super::read_file_contents_at_for_verification,
        );
        assert!(readback_failure.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"new\n");

        fs::write(&file, "old\n").unwrap();
        let uncertain_readback = super::super::apply_patch_file_at_with_ops(
            &root,
            OsStr::new("note.txt"),
            patch,
            super::super::write_to_fd,
            super::super::rename_file_at,
            |_fd: RawFd, _target: &OsStr| {
                Err(super::super::CliError::new("simulated readback failure"))
            },
        );
        assert!(uncertain_readback.is_err());
        assert_eq!(fs::read(&file).unwrap(), b"new\n");

        fs::remove_dir_all(directory).unwrap();
    }

    fn create_workspace(label: &str) -> (PathBuf, super::super::WorkspaceRoot) {
        let directory = std::env::temp_dir().join(format!(
            "krypper-patch-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = super::super::capture_workspace_root_at(&directory).unwrap();
        (directory, root)
    }
}
