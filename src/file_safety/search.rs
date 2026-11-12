use super::{
    is_regular_file, is_symlink, list_open_directory_with_limit, open_at, open_workspace_relative,
    stat_at, CliError, WorkspaceRoot, MAX_FILE_SIZE, MAX_TASK_DIRECTORY_ENTRIES,
};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_SEARCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEARCH_LOCATIONS: usize = 10_000;

pub(crate) fn search_text_files_at(
    root: &WorkspaceRoot,
    directory_path: &OsStr,
    query: &str,
) -> Result<Vec<(PathBuf, usize)>, CliError> {
    if query.is_empty() {
        return Err(CliError::new("search query must not be empty"));
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(CliError::new("search query exceeds 4 KiB"));
    }

    let mut relative_directory = PathBuf::new();
    for component in Path::new(directory_path).components() {
        match component {
            Component::CurDir => {}
            other => relative_directory.push(other.as_os_str()),
        }
    }

    let directory = open_workspace_relative(root, directory_path, true)?;
    let directory_fd = directory.as_raw_fd();
    let entries_directory = open_at(
        directory_fd,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to search directory"))?;
    let names = list_open_directory_with_limit(
        File::from(entries_directory),
        Some(MAX_TASK_DIRECTORY_ENTRIES),
    )?;
    let mut locations = Vec::new();
    let mut bytes_scanned = 0_usize;

    for entry in names {
        let status = stat_at(directory_fd, &entry.name)
            .map_err(|_| CliError::new("unable to search directory"))?;
        if is_symlink(&status) || !is_regular_file(&status) {
            continue;
        }
        let size = u64::try_from(status.st_size)
            .map_err(|_| CliError::new("unable to read search file"))?;
        if size > MAX_FILE_SIZE as u64 {
            return Err(CliError::new("file exceeds 1 MiB"));
        }

        let remaining = MAX_SEARCH_BYTES - bytes_scanned;
        if size > remaining as u64 {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }

        let file = open_at(
            directory_fd,
            &entry.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("unable to read search file"))?;
        let file = File::from(file);
        let metadata = file
            .metadata()
            .map_err(|_| CliError::new("unable to read search file"))?;
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_FILE_SIZE as u64 {
            return Err(CliError::new("file exceeds 1 MiB"));
        }
        if metadata.len() > remaining as u64 {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }

        let read_limit = MAX_FILE_SIZE.min(remaining);
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(read_limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CliError::new("unable to read search file"))?;
        if bytes.len() > MAX_FILE_SIZE {
            return Err(CliError::new("file exceeds 1 MiB"));
        }
        if bytes.len() > remaining {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }
        bytes_scanned += bytes.len();

        let contents =
            String::from_utf8(bytes).map_err(|_| CliError::new("file is not valid UTF-8"))?;
        append_search_locations(
            &contents,
            query,
            &relative_directory.join(&entry.name),
            &mut locations,
        )?;
    }

    Ok(locations)
}

fn append_search_locations(
    contents: &str,
    query: &str,
    path: &Path,
    locations: &mut Vec<(PathBuf, usize)>,
) -> Result<(), CliError> {
    let query = query.as_bytes();
    let mut prefix = vec![0; query.len()];
    for index in 1..query.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && query[index] != query[matched] {
            matched = prefix[matched - 1];
        }
        if query[index] == query[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }

    let mut matched = 0;
    let mut line = 1;
    let mut recent_lines = VecDeque::with_capacity(query.len());
    let mut last_location_line = None;
    for &byte in contents.as_bytes() {
        recent_lines.push_back(line);
        if recent_lines.len() > query.len() {
            recent_lines.pop_front();
        }

        while matched > 0 && byte != query[matched] {
            matched = prefix[matched - 1];
        }
        if byte == query[matched] {
            matched += 1;
        }
        if matched == query.len() {
            let location_line = recent_lines
                .front()
                .copied()
                .ok_or_else(|| CliError::new("unable to search file"))?;
            if last_location_line != Some(location_line) {
                if locations.len() == MAX_SEARCH_LOCATIONS {
                    return Err(CliError::new("search exceeds 10,000 result locations"));
                }
                locations.push((path.to_path_buf(), location_line));
                last_location_line = Some(location_line);
            }
            matched = prefix[matched - 1];
        }

        if byte == b'\n' {
            line += 1;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        search_text_files_at, WorkspaceRoot, MAX_FILE_SIZE, MAX_SEARCH_BYTES, MAX_SEARCH_LOCATIONS,
        MAX_SEARCH_QUERY_BYTES,
    };
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_SEARCH_TEMP: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn returns_sorted_matching_file_and_line_locations() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("b.txt"), "needle first needle duplicate\n").unwrap();
        fs::write(
            path.join("a.txt"),
            "skip\nneedle second\nneedle third\nNeedle uppercase",
        )
        .unwrap();
        fs::write(path.join(".hidden"), "needle hidden\n").unwrap();
        fs::create_dir(path.join("nested")).unwrap();
        fs::write(path.join("nested/ignored.txt"), "needle nested\n").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        let locations = search_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap();

        assert_eq!(
            locations,
            [
                (PathBuf::from(".hidden"), 1),
                (PathBuf::from("a.txt"), 2),
                (PathBuf::from("a.txt"), 3),
                (PathBuf::from("b.txt"), 1),
            ]
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn reports_lines_for_overlapping_multiline_literal_matches() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-multiline-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "a\na\na").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        let locations = search_text_files_at(&root, std::ffi::OsStr::new("."), "a\na").unwrap();

        assert_eq!(
            locations,
            [
                (PathBuf::from("note.txt"), 1),
                (PathBuf::from("note.txt"), 2)
            ]
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn matches_utf8_literals_and_reports_the_starting_line() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-unicode-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "雪\n猫 and 雪\n猫").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        let locations = search_text_files_at(&root, std::ffi::OsStr::new("."), "雪\n猫").unwrap();

        assert_eq!(
            locations,
            [
                (PathBuf::from("note.txt"), 1),
                (PathBuf::from("note.txt"), 2)
            ]
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rejects_empty_and_oversized_queries() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-query-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "")
                .unwrap_err()
                .message(),
            "search query must not be empty"
        );
        assert_eq!(
            search_text_files_at(
                &root,
                std::ffi::OsStr::new("."),
                &"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)
            )
            .unwrap_err()
            .message(),
            "search query exceeds 4 KiB"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rejects_invalid_utf8_and_oversized_files_without_partial_locations() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-file-errors-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("a.txt"), "needle private text").unwrap();
        fs::write(
            path.join("z-invalid.txt"),
            [b"invalid ".as_slice(), &[0xff]].concat(),
        )
        .unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "file is not valid UTF-8"
        );

        fs::remove_file(path.join("z-invalid.txt")).unwrap();
        fs::write(path.join("z-oversized.txt"), vec![b'x'; MAX_FILE_SIZE + 1]).unwrap();
        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "file exceeds 1 MiB"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn stops_at_cumulative_byte_and_result_location_limits() {
        let path = std::env::temp_dir().join(format!(
            "krypper-search-bounds-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        for index in 0..=(MAX_SEARCH_BYTES / MAX_FILE_SIZE) {
            fs::write(
                path.join(format!("bytes-{index:02}.txt")),
                vec![b'x'; MAX_FILE_SIZE],
            )
            .unwrap();
        }
        fs::write(path.join("z-invalid.txt"), [0xff]).unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };
        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "search exceeds 16 MiB scan limit"
        );

        for entry in fs::read_dir(&path).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let locations = MAX_SEARCH_LOCATIONS + 1;
        fs::write(path.join("a-results.txt"), "needle\n".repeat(locations)).unwrap();
        fs::write(path.join("z-invalid.txt"), [0xff]).unwrap();
        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "search exceeds 10,000 result locations"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn skips_symlinks_and_non_regular_entries_and_rejects_workspace_escapes() {
        let parent = std::env::temp_dir().join(format!(
            "krypper-search-paths-{}-{}",
            std::process::id(),
            NEXT_SEARCH_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = parent.join("workspace");
        fs::create_dir_all(&path).unwrap();
        fs::write(parent.join("outside.txt"), "needle outside").unwrap();
        fs::write(path.join(".hidden"), "needle inside").unwrap();
        fs::create_dir(path.join("nested")).unwrap();
        fs::write(path.join("nested/ignored.txt"), "needle nested").unwrap();
        symlink(parent.join("outside.txt"), path.join("linked.txt")).unwrap();
        fs::create_dir_all(path.join("actual/child")).unwrap();
        fs::write(
            path.join("actual/child/selected.txt"),
            "selected via symlink",
        )
        .unwrap();
        symlink(path.join("actual/child"), path.join("directory-link")).unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        assert_eq!(
            search_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap(),
            [(PathBuf::from(".hidden"), 1)]
        );
        assert_eq!(
            search_text_files_at(&root, Path::new("directory-link").as_os_str(), "selected")
                .unwrap(),
            [(PathBuf::from("directory-link/selected.txt"), 1)]
        );
        assert_eq!(
            search_text_files_at(&root, Path::new("../outside").as_os_str(), "needle")
                .unwrap_err()
                .message(),
            "path must stay inside the current directory"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn result_paths_preserve_parent_components_after_directory_symlinks() {
        let parent = std::env::temp_dir().join(format!(
            "krypper-search-symlink-parent-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let workspace = parent.join("workspace");
        fs::create_dir_all(workspace.join("actual/child")).unwrap();
        fs::create_dir_all(workspace.join("actual/target")).unwrap();
        fs::write(workspace.join("actual/target/match.txt"), "needle here\n").unwrap();
        symlink(
            workspace.join("actual/child"),
            workspace.join("directory-link"),
        )
        .unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&workspace).unwrap().into(),
            path: fs::canonicalize(&workspace).unwrap(),
        };

        let locations = search_text_files_at(
            &root,
            Path::new("directory-link/../target").as_os_str(),
            "needle",
        )
        .unwrap();

        assert_eq!(
            locations,
            [(PathBuf::from("directory-link/../target/match.txt"), 1)]
        );
        fs::remove_dir_all(parent).unwrap();
    }
}
