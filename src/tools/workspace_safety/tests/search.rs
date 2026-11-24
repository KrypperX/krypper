//! Text search and tree-search behaviour.

use super::super::search::{
    search_text_files_at, search_tree_text_files_at, MAX_SEARCH_BYTES, MAX_SEARCH_LOCATIONS,
    MAX_SEARCH_QUERY_BYTES, MAX_TREE_DIRECTORY_DEPTH,
};
use super::super::{
    list_task_directory_names_at,
    types::{DirHandle, MAX_FILE_SIZE},
    WorkspaceRoot,
};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    assert_eq!(
        search_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap(),
        [(PathBuf::from(".hidden"), 1)]
    );
    assert_eq!(
        search_text_files_at(&root, Path::new("directory-link").as_os_str(), "selected").unwrap(),
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
        descriptor: DirHandle::from_file(fs::File::open(&workspace).unwrap()),
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
        descriptor: DirHandle::from_file(fs::File::open(path).unwrap()),
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

    let locations = search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap();

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

#[test]
fn searches_apply_root_and_nested_gitignore_rules_deterministically() {
    let path = tree_temp_path();
    fs::create_dir_all(path.join("nested/build")).unwrap();
    fs::create_dir_all(path.join("build")).unwrap();
    fs::create_dir_all(path.join("cache")).unwrap();
    fs::create_dir_all(path.join(".git")).unwrap();
    fs::write(
        path.join(".gitignore"),
        "*.log\n!keep.log\nbuild/\n**/generated-*.rs\ncache/*\n!cache/keep.txt\n",
    )
    .unwrap();
    fs::write(
        path.join("nested/.gitignore"),
        "keep.log\n!nested-keep.log\n*.tmp\n!keep.tmp\n",
    )
    .unwrap();
    for relative_path in [
        ".hidden",
        "drop.log",
        "keep.log",
        "generated-output.rs",
        "build/compiled.txt",
        "cache/private.txt",
        "cache/keep.txt",
        "nested/.hidden",
        "nested/keep.log",
        "nested/nested-keep.log",
        "nested/drop.tmp",
        "nested/keep.tmp",
        "nested/generated-source.rs",
        "nested/build/compiled.txt",
        ".git/config",
    ] {
        fs::write(path.join(relative_path), "needle\n").unwrap();
    }
    let root = tree_workspace_root(&path);

    let immediate = search_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap();
    assert_eq!(
        immediate,
        [
            (PathBuf::from(".hidden"), 1),
            (PathBuf::from("keep.log"), 1),
        ]
    );
    assert_eq!(
        search_text_files_at(&root, Path::new("nested").as_os_str(), "needle").unwrap(),
        [
            (PathBuf::from("nested/.hidden"), 1),
            (PathBuf::from("nested/keep.tmp"), 1),
            (PathBuf::from("nested/nested-keep.log"), 1),
        ]
    );

    let recursive = search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap();
    assert_eq!(
        recursive,
        [
            (PathBuf::from(".hidden"), 1),
            (PathBuf::from("cache/keep.txt"), 1),
            (PathBuf::from("keep.log"), 1),
            (PathBuf::from("nested/.hidden"), 1),
            (PathBuf::from("nested/keep.tmp"), 1),
            (PathBuf::from("nested/nested-keep.log"), 1),
        ]
    );
    assert_eq!(
        search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap(),
        recursive
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn invalid_gitignore_encoding_or_patterns_fail_without_partial_results() {
    let path = tree_temp_path();
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("a-match.txt"), "needle\n").unwrap();
    fs::write(path.join(".gitignore"), "[\n").unwrap();
    let root = tree_workspace_root(&path);

    for search in [
        search_text_files_at(&root, std::ffi::OsStr::new("."), "needle"),
        search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle"),
    ] {
        assert_eq!(search.unwrap_err().message(), "ignore rules are invalid");
    }

    fs::write(path.join(".gitignore"), "# valid\n").unwrap();
    fs::create_dir_all(path.join("nested")).unwrap();
    fs::write(
        path.join("nested/.gitignore"),
        [b"# valid\n".as_slice(), &[0xff]].concat(),
    )
    .unwrap();
    fs::write(path.join("nested/match.txt"), "needle\n").unwrap();
    assert_eq!(
        search_text_files_at(&root, std::ffi::OsStr::new("."), "needle").unwrap(),
        [(PathBuf::from("a-match.txt"), 1)]
    );
    assert_eq!(
        search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
            .unwrap_err()
            .message(),
        "ignore file is not valid UTF-8"
    );
    assert_eq!(
        search_text_files_at(&root, Path::new("nested").as_os_str(), "needle")
            .unwrap_err()
            .message(),
        "ignore file is not valid UTF-8"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn unreadable_gitignore_fails_without_partial_results() {
    let path = tree_temp_path();
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("a-match.txt"), "needle\n").unwrap();
    let ignore_path = path.join(".gitignore");
    fs::write(&ignore_path, "*.tmp\n").unwrap();
    fs::set_permissions(&ignore_path, fs::Permissions::from_mode(0o000)).unwrap();

    if fs::File::open(&ignore_path).is_ok() {
        fs::remove_dir_all(path).unwrap();
        return;
    }

    let root = tree_workspace_root(&path);
    assert_eq!(
        search_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
            .unwrap_err()
            .message(),
        "unable to read ignore file"
    );
    assert_eq!(
        search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
            .unwrap_err()
            .message(),
        "unable to read ignore file"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn ignore_rules_from_left_directories_do_not_apply_after_parent_components() {
    let path = tree_temp_path();
    fs::create_dir_all(path.join("child")).unwrap();
    fs::write(path.join("child/.gitignore"), "*.txt\n").unwrap();
    fs::write(path.join("child.txt"), "needle\n").unwrap();
    fs::write(path.join("root.txt"), "needle\n").unwrap();
    let root = tree_workspace_root(&path);

    assert_eq!(
        search_text_files_at(&root, Path::new("child/..").as_os_str(), "needle").unwrap(),
        [
            (PathBuf::from("child/../child.txt"), 1),
            (PathBuf::from("child/../root.txt"), 1),
        ]
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn parent_components_select_the_resolved_directory_for_listing_and_search() {
    let path = tree_temp_path();
    fs::create_dir_all(path.join("a/b")).unwrap();
    fs::write(path.join("root.txt"), "needle at root\n").unwrap();
    fs::write(path.join("a/.gitignore"), "b/\n").unwrap();
    fs::write(path.join("a/a.txt"), "needle in a\n").unwrap();
    fs::write(path.join("a/b/b.txt"), "needle in b\n").unwrap();
    let root = tree_workspace_root(&path);

    let cases = [
        (
            "a/b/..",
            vec![".gitignore", "a.txt", "b"],
            vec![PathBuf::from("a/b/../a.txt")],
            vec![PathBuf::from("a/b/../a.txt")],
        ),
        (
            "a/..",
            vec!["a", "root.txt"],
            vec![PathBuf::from("a/../root.txt")],
            vec![
                PathBuf::from("a/../a/a.txt"),
                PathBuf::from("a/../root.txt"),
            ],
        ),
        (
            ".",
            vec!["a", "root.txt"],
            vec![PathBuf::from("root.txt")],
            vec![PathBuf::from("a/a.txt"), PathBuf::from("root.txt")],
        ),
        (
            "a/./b/..",
            vec![".gitignore", "a.txt", "b"],
            vec![PathBuf::from("a/b/../a.txt")],
            vec![PathBuf::from("a/b/../a.txt")],
        ),
    ];

    for (requested, expected_entries, expected_flat, expected_tree) in cases {
        let entries = list_task_directory_names_at(&root, Path::new(requested).as_os_str())
            .unwrap()
            .into_iter()
            .map(|entry| entry.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(entries, expected_entries, "listing {requested:?}");
        assert_eq!(
            search_text_files_at(&root, Path::new(requested).as_os_str(), "needle")
                .unwrap()
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            expected_flat,
            "flat search {requested:?}"
        );
        assert_eq!(
            search_tree_text_files_at(&root, Path::new(requested).as_os_str(), "needle")
                .unwrap()
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            expected_tree,
            "tree search {requested:?}"
        );
    }

    fs::remove_dir_all(path).unwrap();
}

#[test]
fn symlinked_gitignore_fails_closed_without_reading_outside_workspace() {
    let parent = tree_temp_path();
    let path = parent.join("workspace");
    fs::create_dir_all(&path).unwrap();
    let outside_ignore = parent.join("outside.gitignore");
    fs::write(&outside_ignore, "*.txt\n").unwrap();
    fs::write(path.join("a-match.txt"), "needle\n").unwrap();
    symlink(&outside_ignore, path.join(".gitignore")).unwrap();
    let root = tree_workspace_root(&path);

    assert_eq!(
        search_tree_text_files_at(&root, std::ffi::OsStr::new("."), "needle")
            .unwrap_err()
            .message(),
        "unable to read ignore file"
    );
    assert_eq!(fs::read(&outside_ignore).unwrap(), b"*.txt\n");
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn selected_directory_symlink_cannot_expose_git_metadata() {
    let path = tree_temp_path();
    fs::create_dir_all(path.join(".git/objects")).unwrap();
    fs::write(path.join(".git/config"), "needle in git metadata\n").unwrap();
    symlink(path.join(".git"), path.join("metadata-alias")).unwrap();
    let root = tree_workspace_root(&path);

    assert!(
        search_text_files_at(&root, Path::new("metadata-alias").as_os_str(), "needle")
            .unwrap()
            .is_empty()
    );
    assert!(
        search_tree_text_files_at(&root, Path::new("metadata-alias").as_os_str(), "needle")
            .unwrap()
            .is_empty()
    );

    fs::remove_dir_all(path).unwrap();
}
