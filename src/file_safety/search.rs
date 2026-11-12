use super::{
    fstat, is_directory, is_regular_file, is_symlink, list_open_directory_with_limit,
    list_open_directory_with_limit_and_error, open_at, open_workspace_relative, same_inode,
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
const MAX_TREE_DIRECTORY_DEPTH: usize = 16;
const MAX_TREE_DIRECTORY_ENTRIES: usize = 4_096;
const TREE_DIRECTORY_ENTRY_LIMIT_ERROR: &str = "search tree exceeds 4,096 directory entry limit";
const TREE_DIRECTORY_DEPTH_LIMIT_ERROR: &str = "search tree exceeds 16 directory depth limit";

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

pub(crate) fn search_tree_text_files_at(
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
    let mut pending = VecDeque::new();
    pending.push_back((relative_directory, 0, directory));
    let mut locations = Vec::new();
    let mut bytes_scanned = 0_usize;
    let mut entries_considered = 0_usize;

    while let Some((relative_directory, depth, directory)) = pending.pop_front() {
        let directory_fd = directory.as_raw_fd();
        let entries_directory = open_at(
            directory_fd,
            OsStr::new("."),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("unable to search directory"))?;
        let remaining_entries = MAX_TREE_DIRECTORY_ENTRIES - entries_considered;
        let names = list_open_directory_with_limit_and_error(
            File::from(entries_directory),
            Some(remaining_entries),
            TREE_DIRECTORY_ENTRY_LIMIT_ERROR,
        )?;
        entries_considered += names.len();

        for entry in names {
            let status = stat_at(directory_fd, &entry.name)
                .map_err(|_| CliError::new("unable to search directory"))?;
            if is_symlink(&status) {
                continue;
            }
            if is_directory(&status) {
                if depth == MAX_TREE_DIRECTORY_DEPTH {
                    return Err(CliError::new(TREE_DIRECTORY_DEPTH_LIMIT_ERROR));
                }
                let child = open_at(
                    directory_fd,
                    &entry.name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
                .map_err(|_| CliError::new("unable to search directory"))?;
                let opened_status = fstat(child.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                if !same_inode(&status, &opened_status) || !is_directory(&opened_status) {
                    return Err(CliError::new("directory changed while searching"));
                }
                pending.push_back((
                    relative_directory.join(&entry.name),
                    depth + 1,
                    File::from(child),
                ));
                continue;
            }
            if !is_regular_file(&status) {
                continue;
            }

            let size = u64::try_from(status.st_size)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if size > MAX_FILE_SIZE as u64 {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            let remaining_bytes = MAX_SEARCH_BYTES - bytes_scanned;
            if size > remaining_bytes as u64 {
                return Err(CliError::new("search exceeds 16 MiB scan limit"));
            }

            let file = open_at(
                directory_fd,
                &entry.name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            )
            .map_err(|_| CliError::new("unable to read search file"))?;
            let opened_status =
                fstat(file.as_raw_fd()).map_err(|_| CliError::new("unable to read search file"))?;
            if !same_inode(&status, &opened_status) {
                return Err(CliError::new("file changed while searching"));
            }
            if !is_regular_file(&opened_status) {
                continue;
            }
            let opened_size = u64::try_from(opened_status.st_size)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if opened_size > MAX_FILE_SIZE as u64 {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            if opened_size > remaining_bytes as u64 {
                return Err(CliError::new("search exceeds 16 MiB scan limit"));
            }

            let read_limit = MAX_FILE_SIZE.min(remaining_bytes);
            let mut bytes = Vec::with_capacity(opened_size as usize);
            File::from(file)
                .take(read_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if bytes.len() > MAX_FILE_SIZE {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            if bytes.len() > remaining_bytes {
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
    }

    locations.sort_unstable();
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
        search_text_files_at, search_tree_text_files_at, WorkspaceRoot, MAX_FILE_SIZE,
        MAX_SEARCH_BYTES, MAX_SEARCH_LOCATIONS, MAX_SEARCH_QUERY_BYTES, MAX_TREE_DIRECTORY_DEPTH,
    };
    use std::fs;
    use std::io::Write;
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

    fn tree_workspace_root(path: &Path) -> WorkspaceRoot {
        WorkspaceRoot {
            descriptor: fs::File::open(path).unwrap().into(),
            path: fs::canonicalize(path).unwrap(),
        }
    }

    fn tree_temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "krypper-search-tree-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn recursively_returns_hidden_paths_and_ordered_start_lines_without_following_entries() {
        use std::os::unix::net::UnixListener;

        let workspace = std::env::temp_dir().join(format!("kt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(workspace.join("dir")).unwrap();
        fs::write(workspace.join(".hidden"), "needle hidden\n").unwrap();
        fs::write(
            workspace.join("z.txt"),
            "needle first\nNeedle ignored\nneedle third",
        )
        .unwrap();
        fs::write(
            workspace.join("dir/.hidden-child"),
            "needle nested hidden\n",
        )
        .unwrap();
        fs::write(workspace.join("dir/a.txt"), "skip\nneedle nested\n").unwrap();
        symlink(workspace.join("dir/a.txt"), workspace.join("linked.txt")).unwrap();
        symlink(workspace.join("dir"), workspace.join("linked-dir")).unwrap();
        let _socket = UnixListener::bind(workspace.join("s")).unwrap();
        let root = tree_workspace_root(&workspace);

        let locations =
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap();

        assert_eq!(
            locations,
            [
                (PathBuf::from(".hidden"), 1),
                (PathBuf::from("dir/.hidden-child"), 1),
                (PathBuf::from("dir/a.txt"), 2),
                (PathBuf::from("z.txt"), 1),
                (PathBuf::from("z.txt"), 3),
            ]
        );
        assert!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "absent")
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn recursive_search_rejects_empty_and_oversized_queries_and_workspace_escapes() {
        let parent = tree_temp_path();
        let workspace = parent.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let root = tree_workspace_root(&workspace);

        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "")
                .unwrap_err()
                .message(),
            "search query must not be empty"
        );
        assert!(search_tree_text_files_at(
            &root,
            std::ffi::OsStr::new("."),
            &"x".repeat(MAX_SEARCH_QUERY_BYTES)
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            search_tree_text_files_at(
                &root,
                std::ffi::OsStr::new("."),
                &"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)
            )
            .unwrap_err()
            .message(),
            "search query exceeds 4 KiB"
        );
        assert_eq!(
            search_tree_text_files_at(&root, Path::new("../outside").as_os_str(), "needle")
                .unwrap_err()
                .message(),
            "path must stay inside the current directory"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn recursive_search_enforces_file_and_cumulative_byte_limits() {
        let path = tree_temp_path();
        fs::create_dir_all(path.join("nested")).unwrap();
        let root = tree_workspace_root(&path);

        fs::write(path.join("nested/large.txt"), vec![b'x'; MAX_FILE_SIZE]).unwrap();
        assert!(search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "absent").is_ok());
        fs::OpenOptions::new()
            .append(true)
            .open(path.join("nested/large.txt"))
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "absent")
                .unwrap_err()
                .message(),
            "file exceeds 1 MiB"
        );

        fs::remove_file(path.join("nested/large.txt")).unwrap();
        for index in 0..MAX_SEARCH_BYTES / MAX_FILE_SIZE {
            fs::write(
                path.join("nested").join(format!("{index:02}.txt")),
                vec![b'x'; MAX_FILE_SIZE],
            )
            .unwrap();
        }
        assert!(search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "absent").is_ok());
        fs::write(
            path.join("nested")
                .join(format!("{:02}.txt", MAX_SEARCH_BYTES / MAX_FILE_SIZE)),
            vec![b'x'; MAX_FILE_SIZE],
        )
        .unwrap();
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "absent")
                .unwrap_err()
                .message(),
            "search exceeds 16 MiB scan limit"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn recursive_search_enforces_result_location_limit_without_partial_results() {
        let path = tree_temp_path();
        fs::create_dir_all(path.join("nested")).unwrap();
        let root = tree_workspace_root(&path);
        fs::write(
            path.join("nested/results.txt"),
            "needle\n".repeat(MAX_SEARCH_LOCATIONS),
        )
        .unwrap();
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap()
                .len(),
            MAX_SEARCH_LOCATIONS
        );
        fs::OpenOptions::new()
            .append(true)
            .open(path.join("nested/results.txt"))
            .unwrap()
            .write_all(b"needle\n")
            .unwrap();
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "search exceeds 10,000 result locations"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn recursive_search_enforces_global_entry_and_depth_limits() {
        let path = tree_temp_path();
        fs::create_dir_all(path.join("nested")).unwrap();
        let root = tree_workspace_root(&path);
        for index in 0..2048 {
            fs::write(path.join(format!("root-{index:04}")), b"").unwrap();
            fs::write(path.join("nested").join(format!("child-{index:04}")), b"").unwrap();
        }
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "search tree exceeds 4,096 directory entry limit"
        );
        fs::remove_dir_all(&path).unwrap();

        let mut nested = path.clone();
        fs::create_dir_all(&nested).unwrap();
        for depth in 1..=MAX_TREE_DIRECTORY_DEPTH {
            nested.push(format!("d{depth}"));
            fs::create_dir(&nested).unwrap();
        }
        fs::write(nested.join("at-limit.txt"), b"").unwrap();
        let root = tree_workspace_root(&path);
        assert!(search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle").is_ok());
        nested.push(format!("d{}", MAX_TREE_DIRECTORY_DEPTH + 1));
        fs::create_dir(&nested).unwrap();
        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "search tree exceeds 16 directory depth limit"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn recursive_search_rejects_invalid_utf8_after_matches_without_partial_results() {
        let path = tree_temp_path();
        fs::create_dir_all(path.join("nested")).unwrap();
        fs::write(path.join("a.txt"), "needle private text\n").unwrap();
        fs::write(
            path.join("nested/z-invalid"),
            [b"invalid ".as_slice(), &[0xff]].concat(),
        )
        .unwrap();
        let root = tree_workspace_root(&path);

        assert_eq!(
            search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
                .unwrap_err()
                .message(),
            "file is not valid UTF-8"
        );
        fs::remove_dir_all(path).unwrap();
    }
}
