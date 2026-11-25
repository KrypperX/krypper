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

#[cfg(unix)]
#[test]
fn replacement_preserves_permission_bits_and_replaces_only_the_named_hardlink() {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-metadata-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let target = path.join("note.txt");
    let other_link = path.join("note-link.txt");
    fs::write(&target, "before old after").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    fs::hard_link(&target, &other_link).unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "old", "new").unwrap();

    assert_eq!(fs::read_to_string(&target).unwrap(), "before new after");
    assert_eq!(fs::read_to_string(&other_link).unwrap(), "before old after");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    fs::remove_dir_all(path).unwrap();
}

#[cfg(unix)]
#[test]
fn staged_replacement_stays_private_until_it_is_committed() {
    use std::os::unix::fs::PermissionsExt;

    use super::super::replace::{
        read_file_contents_at_for_verification, replace_file_contents_atomically,
        ReplacementOperations,
    };
    use super::super::syscalls::{
        rename_file_at, set_mode, stat_at, unlink_created_file, RenameFileError,
    };
    use super::super::types::OriginalFileState;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-private-stage-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let target = path.join("note.txt");
    fs::write(&target, "original").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let parent = DirHandle::from_file(fs::File::open(&path).unwrap());
    let original_status = stat_at(&parent, std::ffi::OsStr::new("note.txt")).unwrap();

    let mut operations = ReplacementOperations {
        writer: super::write_to_file,
        committer: |parent: &DirHandle,
                    temporary_name: &std::ffi::OsStr,
                    target_name: &std::ffi::OsStr| {
            let staged =
                stat_at(parent, temporary_name).map_err(|_| RenameFileError::NotCommitted)?;
            assert_eq!(staged.mode.bits() & !0o600, 0);
            rename_file_at(parent, temporary_name, target_name)
        },
        readback: read_file_contents_at_for_verification,
        cleanup: unlink_created_file,
        mode_setter: set_mode,
    };
    replace_file_contents_atomically(
        &parent,
        std::ffi::OsStr::new("note.txt"),
        OriginalFileState {
            status: original_status,
            contents: b"original",
        },
        b"replacement",
        &mut operations,
    )
    .unwrap();

    assert_eq!(fs::read_to_string(target).unwrap(), "replacement");
    assert_eq!(
        fs::metadata(path.join("note.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    fs::remove_dir_all(path).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn replacement_rejects_an_existing_advisory_lock() {
    use super::super::syscalls::try_lock_file_exclusive;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-locked-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let target = path.join("note.txt");
    fs::write(&target, "original contents").unwrap();
    let lock = fs::File::open(&target).unwrap();
    try_lock_file_exclusive(&lock).unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    let error = replace_text_file_at(
        &root,
        std::ffi::OsStr::new("note.txt"),
        "original",
        "replacement",
    )
    .unwrap_err();

    assert_eq!(error.message(), "file is locked by another replacement");
    assert_eq!(fs::read_to_string(&target).unwrap(), "original contents");
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    drop(lock);
    fs::remove_dir_all(path).unwrap();
}

#[cfg(unix)]
#[test]
fn replacement_rejects_in_process_concurrent_mutation_of_the_same_file() {
    use std::sync::mpsc;
    use std::time::Duration;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-in-process-lock-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "first|second").unwrap();

    let worker_path = path.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (continue_tx, continue_rx) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let root = WorkspaceRoot {
            descriptor: DirHandle::from_file(fs::File::open(&worker_path).unwrap()),
            path: fs::canonicalize(&worker_path).unwrap(),
        };
        let mut waited = false;
        replace_text_file_at_with_writer(
            &root,
            std::ffi::OsStr::new("note.txt"),
            "first",
            "ONE",
            move |file, bytes| {
                if !waited {
                    entered_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                    waited = true;
                }
                super::write_to_file(file, bytes)
            },
        )
    });

    if entered_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let _ = continue_tx.send(());
        let _ = worker.join();
        panic!("first replacement did not reach its staged write");
    }

    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };
    let error =
        replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "second", "TWO").unwrap_err();
    assert_eq!(error.message(), "file is locked by another replacement");

    continue_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "ONE|second"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn replacement_does_not_sweep_unowned_lookalike_temporary_files() {
    let path = std::env::temp_dir().join(format!(
        "krypper-replace-stale-temp-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "before old after").unwrap();
    let stale_name = ".krypper-replace-00000000-0000-4000-8000-000000000000";
    fs::write(path.join(stale_name), "leave this file for inspection").unwrap();
    let root = WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(&path).unwrap()),
        path: fs::canonicalize(&path).unwrap(),
    };

    replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "old", "new").unwrap();

    assert_eq!(
        fs::read_to_string(path.join(stale_name)).unwrap(),
        "leave this file for inspection"
    );
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "before new after"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn staged_file_cleanup_refuses_to_remove_a_replaced_name() {
    use super::super::syscalls::{stat_at, unlink_created_file};

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-cleanup-identity-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let parent = DirHandle::from_file(fs::File::open(&path).unwrap());
    let temporary_name = std::ffi::OsStr::new(".krypper-replace-test-id");
    fs::write(path.join(temporary_name), "staged").unwrap();
    let staged = stat_at(&parent, temporary_name).unwrap();
    fs::rename(path.join(temporary_name), path.join("held-staged-inode")).unwrap();
    fs::write(path.join(temporary_name), "different file").unwrap();

    let error = unlink_created_file(&parent, temporary_name, Some(&staged.identity)).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        fs::read_to_string(path.join(temporary_name)).unwrap(),
        "different file"
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
fn replacement_reports_when_staged_file_cleanup_fails() {
    use super::super::replace::{
        read_file_contents_at_for_verification, replace_file_contents_atomically,
        ReplacementOperations,
    };
    use super::super::syscalls::{rename_file_at, set_mode, stat_at};
    use super::super::types::OriginalFileState;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-cleanup-failure-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("note.txt"), "original contents").unwrap();
    let parent = DirHandle::from_file(fs::File::open(&path).unwrap());
    let original_status = stat_at(&parent, std::ffi::OsStr::new("note.txt")).unwrap();

    let mut operations = ReplacementOperations {
        writer: |file: &mut std::fs::File, bytes: &[u8]| {
            let partial = super::write_to_file(file, &bytes[..3])?;
            if partial != 3 {
                return Ok(partial);
            }
            Err(io::Error::other("simulated write failure"))
        },
        committer: rename_file_at,
        readback: read_file_contents_at_for_verification,
        cleanup: |_: &DirHandle,
                  _: &std::ffi::OsStr,
                  identity: Option<&super::super::types::FileIdentity>| {
            assert!(identity.is_some());
            Err(io::Error::other("simulated cleanup failure"))
        },
        mode_setter: set_mode,
    };
    let error = replace_file_contents_atomically(
        &parent,
        std::ffi::OsStr::new("note.txt"),
        OriginalFileState {
            status: original_status,
            contents: b"original contents",
        },
        b"replacement contents",
        &mut operations,
    )
    .unwrap_err();

    assert_eq!(
        error.message(),
        "replacement failed and staged file cleanup also failed"
    );
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "original contents"
    );
    assert_eq!(fs::read_dir(&path).unwrap().count(), 2);
    fs::remove_dir_all(path).unwrap();
}

#[cfg(unix)]
#[test]
fn replacement_reports_commit_when_post_rename_mode_restore_fails() {
    use std::os::unix::fs::PermissionsExt;

    use super::super::replace::{
        read_file_contents_at_for_verification, replace_file_contents_atomically,
        ReplacementOperations,
    };
    use super::super::syscalls::{rename_file_at, stat_at, unlink_created_file};
    use super::super::types::OriginalFileState;

    let path = std::env::temp_dir().join(format!(
        "krypper-replace-mode-failure-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let target = path.join("note.txt");
    fs::write(&target, "original").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let parent = DirHandle::from_file(fs::File::open(&path).unwrap());
    let original_status = stat_at(&parent, std::ffi::OsStr::new("note.txt")).unwrap();

    let mut operations = ReplacementOperations {
        writer: super::write_to_file,
        committer: rename_file_at,
        readback: read_file_contents_at_for_verification,
        cleanup: unlink_created_file,
        mode_setter: |_file: &std::fs::File, _mode: super::super::types::CreateMode| {
            Err(io::Error::other("simulated mode restore failure"))
        },
    };

    let error = replace_file_contents_atomically(
        &parent,
        std::ffi::OsStr::new("note.txt"),
        OriginalFileState {
            status: original_status,
            contents: b"original",
        },
        b"replacement",
        &mut operations,
    )
    .unwrap_err();

    assert_eq!(
        error.message(),
        "workspace file replacement committed but could not be verified; inspect it before retrying"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "replacement");
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777 & !0o600,
        0
    );
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
