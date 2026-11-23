use std::fs;
use std::io;
use std::sync::atomic::Ordering;

use super::super::replace::{replace_text_file_at, replace_text_file_at_with_writer};
use super::super::types::{DirHandle, WorkspaceRoot, MAX_FILE_SIZE};
use super::NEXT_TEMP;

#[test]
fn replacement_changes_only_one_exact_match_in_an_existing_file() {
    let path = std::env::temp_dir().join(format!(
        "krypper-replace-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "before exact match after").unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    let result = replace_text_file_at(
        &root,
        std::ffi::OsStr::new("note.txt"),
        "exact match",
        "new text",
    );

    assert_eq!(result.unwrap(), "before new text after".len());
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "before new text after"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn replacement_accepts_a_result_at_the_one_mib_limit() {
    let path = std::env::temp_dir().join(format!(
        "krypper-replace-limit-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "old").unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };
    let replacement = "x".repeat(MAX_FILE_SIZE);

    let result = replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "old", &replacement);

    assert_eq!(result.unwrap(), MAX_FILE_SIZE);
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        replacement
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn replacement_rejects_empty_missing_and_ambiguous_matches_without_changes() {
    let path = std::env::temp_dir().join(format!(
        "krypper-replace-matches-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "aaaa").unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    for (match_text, expected_error) in [
        ("", "match text must not be empty"),
        ("missing", "match text was not found"),
        ("aa", "match text is ambiguous"),
    ] {
        let error = replace_text_file_at(
            &root,
            std::ffi::OsStr::new("note.txt"),
            match_text,
            "replacement",
        )
        .unwrap_err();
        assert_eq!(error.message(), expected_error);
        assert_eq!(fs::read_to_string(path.join("note.txt")).unwrap(), "aaaa");
    }

    fs::remove_dir_all(path).unwrap();
}

#[test]
fn failed_replacement_keeps_the_original_and_removes_its_partial_stage() {
    let path = std::env::temp_dir().join(format!(
        "krypper-replace-write-failure-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "before exact match after").unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };
    let mut writes = 0;

    let result = replace_text_file_at_with_writer(
        &root,
        std::ffi::OsStr::new("note.txt"),
        "exact match",
        "a much longer replacement",
        |file, bytes| {
            writes += 1;
            if writes == 1 {
                super::write_to_file(file, &bytes[..3])
            } else {
                Err(io::Error::other("simulated disk failure"))
            }
        },
    );

    assert_eq!(
        result.unwrap_err().message(),
        "unable to stage file replacement"
    );
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "before exact match after"
    );
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn replacement_rejects_unsafe_paths_non_regular_files_and_oversize_content() {
    use std::os::unix::fs::symlink;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-safety-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let outside = path.join("outside");
    let workspace = path.join("workspace");
    fs::create_dir(&path).unwrap();
    fs::create_dir(&workspace).unwrap();
    fs::write(&outside, "outside text").unwrap();
    fs::write(workspace.join("note.txt"), "regular text").unwrap();
    fs::write(workspace.join("large.txt"), vec![b'x'; MAX_FILE_SIZE + 1]).unwrap();
    fs::write(workspace.join("not-a-directory"), "file").unwrap();
    fs::create_dir(workspace.join("directory")).unwrap();
    symlink("note.txt", workspace.join("linked.txt")).unwrap();
    symlink(".", workspace.join("linked-parent")).unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&workspace).unwrap()),
        path: fs::canonicalize(&workspace).unwrap(),
    };
    let too_long = "x".repeat(MAX_FILE_SIZE + 1);
    let fits_but_overflows = "x".repeat(MAX_FILE_SIZE);
    let cases = [
        (
            std::ffi::OsStr::new("../outside"),
            "text",
            "replacement",
            "path must stay inside the current directory",
        ),
        (
            std::ffi::OsStr::new("linked.txt"),
            "text",
            "replacement",
            "symlink paths are not allowed",
        ),
        (
            std::ffi::OsStr::new("linked-parent/note.txt"),
            "text",
            "replacement",
            "symlink paths are not allowed",
        ),
        (
            std::ffi::OsStr::new("missing-parent/new.txt"),
            "text",
            "replacement",
            "parent directory not found",
        ),
        (
            std::ffi::OsStr::new("not-a-directory/new.txt"),
            "text",
            "replacement",
            "parent is not a directory",
        ),
        (
            std::ffi::OsStr::new("directory"),
            "text",
            "replacement",
            "not a regular file",
        ),
        (
            std::ffi::OsStr::new("large.txt"),
            "x",
            "replacement",
            "file exceeds 1 MiB",
        ),
        (
            std::ffi::OsStr::new("note.txt"),
            "text",
            fits_but_overflows.as_str(),
            "replacement would exceed 1 MiB",
        ),
        (
            std::ffi::OsStr::new("note.txt"),
            "text",
            too_long.as_str(),
            "replacement text exceeds 1 MiB",
        ),
    ];

    for (file, match_text, replacement_text, expected_error) in cases {
        let error = replace_text_file_at(&root, file, match_text, replacement_text).unwrap_err();
        assert_eq!(error.message(), expected_error);
    }
    assert_eq!(
        fs::read_to_string(workspace.join("note.txt")).unwrap(),
        "regular text"
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside text");
    assert_eq!(fs::read_dir(&workspace).unwrap().count(), 6);
    assert!(!workspace.join("missing-parent").exists());
    fs::remove_dir_all(path).unwrap();
}
