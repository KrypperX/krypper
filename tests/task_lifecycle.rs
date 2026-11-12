use krypper::task_lifecycle::{
    Task, TaskEvent, TaskHistoryEvent, TaskHistoryTransition, TaskSession, TaskState,
    TurnActivitySummary, TurnCompletionError, TurnEvent, TurnOutcome, TurnState,
    TurnTransitionError, WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError,
    WorkspaceWriteError,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-workspace-read-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct RestoreCurrentDir(PathBuf);

impl RestoreCurrentDir {
    fn enter(path: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(previous)
    }
}

impl Drop for RestoreCurrentDir {
    fn drop(&mut self) {
        if let Err(error) = std::env::set_current_dir(&self.0) {
            eprintln!("failed to restore test working directory: {error}");
        }
    }
}

fn with_current_dir<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = RestoreCurrentDir::enter(path);
    operation()
}

fn running_turn(session: &mut TaskSession) -> (Uuid, Uuid) {
    let task = session.create_task("Read a workspace file");
    task.transition(TaskState::Running).unwrap();
    let task_id = task.id();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();
    (task_id, turn_id)
}

fn running_turn_in(session: &mut TaskSession, workspace: &Path) -> (Uuid, Uuid) {
    with_current_dir(workspace, || running_turn(session))
}

type TurnSnapshot = (Uuid, TurnState, Option<TurnOutcome>, Vec<TurnEvent>);
type TaskSnapshot = (
    Uuid,
    TaskState,
    Vec<TaskEvent>,
    Vec<TaskHistoryEvent>,
    Vec<TurnSnapshot>,
);

fn lifecycle_snapshot(session: &TaskSession) -> Vec<TaskSnapshot> {
    session
        .list_tasks()
        .iter()
        .map(|task| {
            (
                task.id(),
                task.state(),
                task.events().to_vec(),
                task.history().to_vec(),
                task.turns()
                    .iter()
                    .map(|turn| {
                        (
                            turn.id(),
                            turn.state(),
                            turn.outcome().cloned(),
                            turn.events().to_vec(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn running_owned_turn_reads_a_workspace_file() {
    let temp = TempDir::new();
    fs::write(temp.path().join("note.txt"), "private task context").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());

    let contents = with_current_dir(temp.path(), || {
        session.read_workspace_file(task_id, turn_id, Path::new("note.txt"))
    });

    assert_eq!(contents, Ok("private task context".to_owned()));
}

#[test]
fn running_owned_turn_workspace_operations_stay_bound_after_chdir() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let other_directory = temp.path().join("other");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&other_directory).unwrap();
    fs::write(workspace.join("note.txt"), "task workspace").unwrap();
    fs::write(other_directory.join("note.txt"), "process workspace").unwrap();
    fs::write(other_directory.join("other.txt"), "other").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);

    let (read, entries, create) = with_current_dir(&other_directory, || {
        (
            session.read_workspace_file(task_id, turn_id, Path::new("note.txt")),
            session.list_workspace_directory(task_id, turn_id, Path::new(".")),
            session.create_workspace_file(task_id, turn_id, Path::new("created.txt"), "bound"),
        )
    });

    assert_eq!(read, Ok("task workspace".to_owned()));
    assert_eq!(entries, Ok(vec!["note.txt".into()]));
    assert_eq!(create, Ok(()));
    assert_eq!(
        fs::read_to_string(workspace.join("created.txt")).unwrap(),
        "bound"
    );
    assert!(!other_directory.join("created.txt").exists());
}

#[test]
fn tasks_created_in_different_directories_use_isolated_workspaces() {
    let temp = TempDir::new();
    let first_workspace = temp.path().join("first");
    let second_workspace = temp.path().join("second");
    let other_directory = temp.path().join("other");
    fs::create_dir(&first_workspace).unwrap();
    fs::create_dir(&second_workspace).unwrap();
    fs::create_dir(&other_directory).unwrap();
    fs::write(first_workspace.join("identity.txt"), "first").unwrap();
    fs::write(second_workspace.join("identity.txt"), "second").unwrap();
    fs::write(other_directory.join("identity.txt"), "other").unwrap();
    let mut session = TaskSession::new();
    let (first_task, first_turn) = running_turn_in(&mut session, &first_workspace);
    let (second_task, second_turn) = running_turn_in(&mut session, &second_workspace);

    let results = with_current_dir(&other_directory, || {
        (
            session.read_workspace_file(first_task, first_turn, Path::new("identity.txt")),
            session.read_workspace_file(second_task, second_turn, Path::new("identity.txt")),
            session.list_workspace_directory(first_task, first_turn, Path::new(".")),
            session.list_workspace_directory(second_task, second_turn, Path::new(".")),
            session.create_workspace_file(
                first_task,
                first_turn,
                Path::new("created.txt"),
                "first",
            ),
            session.create_workspace_file(
                second_task,
                second_turn,
                Path::new("created.txt"),
                "second",
            ),
        )
    });

    assert_eq!(results.0, Ok("first".to_owned()));
    assert_eq!(results.1, Ok("second".to_owned()));
    assert_eq!(results.2, Ok(vec!["identity.txt".into()]));
    assert_eq!(results.3, Ok(vec!["identity.txt".into()]));
    assert_eq!(results.4, Ok(()));
    assert_eq!(results.5, Ok(()));
    assert_eq!(
        fs::read_to_string(first_workspace.join("created.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(second_workspace.join("created.txt")).unwrap(),
        "second"
    );
    assert!(!other_directory.join("created.txt").exists());
}

#[test]
fn workspace_resolution_failures_leave_lifecycle_and_files_unchanged() {
    let temp = TempDir::new();
    let removed_workspace = temp.path().join("removed");
    fs::create_dir(&removed_workspace).unwrap();
    fs::write(temp.path().join("sentinel.txt"), "unchanged").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = with_current_dir(&removed_workspace, || {
        fs::remove_dir(&removed_workspace).unwrap();
        running_turn(&mut session)
    });
    let before = lifecycle_snapshot(&session);

    let read = session.read_workspace_file(task_id, turn_id, Path::new("missing.txt"));
    assert!(read.unwrap_err().to_string().contains("task workspace"));
    assert_eq!(lifecycle_snapshot(&session), before);
    let list = session.list_workspace_directory(task_id, turn_id, Path::new("."));
    assert!(list.unwrap_err().to_string().contains("task workspace"));
    assert_eq!(lifecycle_snapshot(&session), before);
    let create = session.create_workspace_file(task_id, turn_id, Path::new("created.txt"), "text");
    assert!(create.unwrap_err().to_string().contains("task workspace"));
    assert_eq!(lifecycle_snapshot(&session), before);
    assert_eq!(
        fs::read_to_string(temp.path().join("sentinel.txt")).unwrap(),
        "unchanged"
    );
    assert!(!temp.path().join("created.txt").exists());
}

#[test]
fn workspace_read_requires_an_existing_task_owned_running_turn() {
    let temp = TempDir::new();
    fs::write(temp.path().join("note.txt"), "context").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);
    let (foreign_task_id, foreign_turn_id) = {
        let foreign_task = session.create_task("Foreign task");
        foreign_task.transition(TaskState::Running).unwrap();
        let task_id = foreign_task.id();
        let turn_id = foreign_task.create_turn().unwrap();
        foreign_task
            .transition_turn(turn_id, TurnState::Running)
            .unwrap();
        (task_id, turn_id)
    };
    let pending_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    let missing_task_id = Uuid::new_v4();
    let missing_turn_id = Uuid::new_v4();

    with_current_dir(temp.path(), || {
        let before = lifecycle_snapshot(&session);
        assert!(matches!(
            session.read_workspace_file(missing_task_id, turn_id, Path::new("note.txt")),
            Err(WorkspaceReadError::TaskNotFound { task_id }) if task_id == missing_task_id
        ));
        assert!(matches!(
            session.read_workspace_file(task_id, foreign_turn_id, Path::new("note.txt")),
            Err(WorkspaceReadError::ForeignTurn {
                task_id: requested_task_id,
                turn_id,
                owner_task_id,
            }) if requested_task_id == task_id
                && turn_id == foreign_turn_id
                && owner_task_id == foreign_task_id
        ));
        assert!(matches!(
            session.read_workspace_file(task_id, missing_turn_id, Path::new("note.txt")),
            Err(WorkspaceReadError::TurnNotFound {
                task_id: actual_task_id,
                turn_id,
            }) if actual_task_id == task_id && turn_id == missing_turn_id
        ));
        assert!(matches!(
            session.read_workspace_file(task_id, pending_turn_id, Path::new("note.txt")),
            Err(WorkspaceReadError::TurnNotRunning {
                turn_id,
                state: TurnState::Pending
            }) if turn_id == pending_turn_id
        ));
        assert_eq!(lifecycle_snapshot(&session), before);
    });
}

#[test]
fn workspace_read_follows_only_symlinks_that_stay_inside_the_workspace() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let other_directory = temp.path().join("other");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&other_directory).unwrap();
    fs::write(workspace.join("note.txt"), "inside").unwrap();
    fs::write(outside.join("note.txt"), "outside").unwrap();
    symlink("note.txt", workspace.join("inside-link")).unwrap();
    symlink(
        workspace.join("note.txt"),
        workspace.join("inside-absolute-link"),
    )
    .unwrap();
    symlink(outside.join("note.txt"), workspace.join("outside-link")).unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);

    with_current_dir(&other_directory, || {
        assert_eq!(
            session.read_workspace_file(task_id, turn_id, Path::new("inside-link")),
            Ok("inside".to_owned())
        );
        assert_eq!(
            session.read_workspace_file(task_id, turn_id, Path::new("inside-absolute-link")),
            Ok("inside".to_owned())
        );
        let outside_link = session
            .read_workspace_file(task_id, turn_id, Path::new("outside-link"))
            .unwrap_err();
        assert!(outside_link
            .to_string()
            .contains("path must stay inside the current directory"));
        let traversal = session
            .read_workspace_file(task_id, turn_id, Path::new("../outside/note.txt"))
            .unwrap_err();
        assert!(traversal
            .to_string()
            .contains("path must stay inside the current directory"));
        let absolute = session
            .read_workspace_file(task_id, turn_id, &workspace.join("note.txt"))
            .unwrap_err();
        assert!(absolute
            .to_string()
            .contains("absolute paths are not allowed"));
    });
}

#[test]
fn failed_workspace_reads_report_file_errors_without_changing_lifecycle_history() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("directory")).unwrap();
    fs::write(temp.path().join("invalid.txt"), [0xff]).unwrap();
    fs::write(temp.path().join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let paths_and_messages = [
        ("missing.txt", "file not found"),
        ("directory", "not a regular file"),
        ("invalid.txt", "file is not valid UTF-8"),
        ("large.txt", "file exceeds 1 MiB"),
    ];

    with_current_dir(temp.path(), || {
        for (path, message) in paths_and_messages {
            let before = lifecycle_snapshot(&session);
            let error = session
                .read_workspace_file(task_id, turn_id, Path::new(path))
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(lifecycle_snapshot(&session), before);
        }
    });
}

#[test]
fn running_owned_turn_lists_immediate_entries_as_native_names_in_cli_order() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(workspace.join("nested")).unwrap();
    fs::write(workspace.join(".hidden"), "hidden").unwrap();
    fs::write(workspace.join("a"), "a").unwrap();
    fs::write(workspace.join("z"), "z").unwrap();
    fs::write(workspace.join("nested/child-only"), "child").unwrap();
    fs::write(outside.join("outside-only"), "outside").unwrap();
    let non_utf8 = OsString::from_vec(b"bad-\xff".to_vec());
    fs::write(workspace.join(&non_utf8), "native name").unwrap();
    symlink("nested", workspace.join("entry-link")).unwrap();
    symlink(&outside, workspace.join("outside-link")).unwrap();

    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);
    let before = lifecycle_snapshot(&session);

    let entries = with_current_dir(&workspace, || {
        session.list_workspace_directory(task_id, turn_id, Path::new("."))
    });

    let expected = vec![
        OsString::from(".hidden"),
        OsString::from("a"),
        non_utf8,
        OsString::from("entry-link"),
        OsString::from("nested"),
        OsString::from("outside-link"),
        OsString::from("z"),
    ];
    assert_eq!(entries, Ok(expected.clone()));
    assert!(expected.iter().all(|name| workspace.join(name).exists()));
    assert!(!expected.contains(&OsString::from(".")));
    assert!(!expected.contains(&OsString::from("..")));
    assert!(!expected.contains(&OsString::from("child-only")));
    assert!(!expected.contains(&OsString::from("outside-only")));
    assert_eq!(lifecycle_snapshot(&session), before);
}

#[test]
fn running_owned_turn_lists_all_1024_immediate_entries() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join(".hidden"), "hidden").unwrap();
    for index in 0..1023 {
        fs::write(workspace.join(format!("entry-{index:04}")), "").unwrap();
    }

    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);
    let entries = with_current_dir(&workspace, || {
        session.list_workspace_directory(task_id, turn_id, Path::new("."))
    })
    .unwrap();

    assert_eq!(entries.len(), 1024);
    assert!(entries.contains(&".hidden".into()));
    assert!(entries.contains(&"entry-0000".into()));
    assert!(entries.contains(&"entry-1022".into()));
    assert!(!entries.contains(&".".into()));
    assert!(!entries.contains(&"..".into()));
}

#[test]
fn workspace_listing_requires_an_existing_owned_running_turn() {
    let temp = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let pending_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    let failed_turn_id = {
        let task = session.get_task_mut(task_id).unwrap();
        let turn_id = task.create_turn().unwrap();
        task.transition_turn(turn_id, TurnState::Running).unwrap();
        task.transition_turn(turn_id, TurnState::Failed).unwrap();
        turn_id
    };
    let (foreign_task_id, foreign_turn_id) = {
        let foreign_task = session.create_task("Foreign task");
        foreign_task.transition(TaskState::Running).unwrap();
        let task_id = foreign_task.id();
        let turn_id = foreign_task.create_turn().unwrap();
        foreign_task
            .transition_turn(turn_id, TurnState::Running)
            .unwrap();
        (task_id, turn_id)
    };
    let missing_task_id = Uuid::new_v4();
    let missing_turn_id = Uuid::new_v4();
    let before = lifecycle_snapshot(&session);

    with_current_dir(temp.path(), || {
        assert!(matches!(
            session.list_workspace_directory(missing_task_id, turn_id, Path::new(".")),
            Err(WorkspaceListError::TaskNotFound { task_id }) if task_id == missing_task_id
        ));
        assert!(matches!(
            session.list_workspace_directory(task_id, foreign_turn_id, Path::new(".")),
            Err(WorkspaceListError::ForeignTurn {
                task_id: requested_task_id,
                turn_id,
                owner_task_id,
            }) if requested_task_id == task_id
                && turn_id == foreign_turn_id
                && owner_task_id == foreign_task_id
        ));
        assert!(matches!(
            session.list_workspace_directory(task_id, missing_turn_id, Path::new(".")),
            Err(WorkspaceListError::TurnNotFound {
                task_id: actual_task_id,
                turn_id,
            }) if actual_task_id == task_id && turn_id == missing_turn_id
        ));
        for (turn_id, state) in [
            (pending_turn_id, TurnState::Pending),
            (failed_turn_id, TurnState::Failed),
        ] {
            assert!(matches!(
                session.list_workspace_directory(task_id, turn_id, Path::new(".")),
                Err(WorkspaceListError::TurnNotRunning {
                    turn_id: actual_turn_id,
                    state: actual_state,
                }) if actual_turn_id == turn_id && actual_state == state
            ));
        }
        assert_eq!(lifecycle_snapshot(&session), before);
    });
}

#[test]
fn workspace_listing_uses_task_workspace_resolution_and_preserves_history_on_failure() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let other_directory = temp.path().join("other");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&other_directory).unwrap();
    fs::create_dir(workspace.join("nested")).unwrap();
    fs::write(workspace.join("nested/entry"), "inside").unwrap();
    fs::write(workspace.join("file"), "not a directory").unwrap();
    fs::write(outside.join("entry"), "outside").unwrap();
    symlink("nested", workspace.join("inside-link")).unwrap();
    symlink(&outside, workspace.join("outside-link")).unwrap();

    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);
    let inside_entries = with_current_dir(&other_directory, || {
        session.list_workspace_directory(task_id, turn_id, Path::new("inside-link"))
    });
    assert_eq!(inside_entries, Ok(vec!["entry".into()]));

    let invalid_paths = [
        (PathBuf::from("missing"), "file not found"),
        (PathBuf::from("file"), "not a directory"),
        (
            PathBuf::from("../outside"),
            "path must stay inside the current directory",
        ),
        (
            PathBuf::from("outside-link"),
            "path must stay inside the current directory",
        ),
        (workspace.clone(), "absolute paths are not allowed"),
    ];
    with_current_dir(&other_directory, || {
        for (path, message) in invalid_paths {
            let before = lifecycle_snapshot(&session);
            let error = session
                .list_workspace_directory(task_id, turn_id, &path)
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(lifecycle_snapshot(&session), before);
        }
    });
}

#[test]
fn running_owned_turn_creates_a_workspace_file_without_changing_lifecycle_history() {
    let temp = TempDir::new();
    fs::create_dir(temp.path().join("notes")).unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let before = lifecycle_snapshot(&session);

    let result = with_current_dir(temp.path(), || {
        session.create_workspace_file(task_id, turn_id, Path::new("notes/context.txt"), "résumé")
    });

    assert_eq!(result, Ok(()));
    assert_eq!(
        fs::read_to_string(temp.path().join("notes/context.txt")).unwrap(),
        "résumé"
    );
    assert_eq!(lifecycle_snapshot(&session), before);
}

#[test]
fn running_owned_turn_replaces_one_exact_workspace_match_without_history_changes() {
    let temp = TempDir::new();
    fs::write(temp.path().join("note.txt"), "before exact match after").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let before = lifecycle_snapshot(&session);

    let result = session.replace_workspace_text(
        task_id,
        turn_id,
        Path::new("note.txt"),
        "exact match",
        "new text",
    );

    assert_eq!(result, Ok("before new text after".len()));
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).unwrap(),
        "before new text after"
    );
    assert_eq!(lifecycle_snapshot(&session), before);
}

#[test]
fn failed_workspace_text_replacements_preserve_the_file_and_lifecycle_history() {
    let temp = TempDir::new();
    let replacement_text = "PRIVATE_REPLACEMENT_83";
    fs::write(temp.path().join("note.txt"), "aaaa").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let before = lifecycle_snapshot(&session);

    for (match_text, expected_error) in [
        ("missing", "match text was not found"),
        ("aa", "match text is ambiguous"),
    ] {
        let error = session
            .replace_workspace_text(
                task_id,
                turn_id,
                Path::new("note.txt"),
                match_text,
                replacement_text,
            )
            .unwrap_err();
        assert!(matches!(error, WorkspaceReplaceError::File { .. }));
        assert!(error.to_string().contains(expected_error));
        assert!(!error.to_string().contains(match_text));
        assert!(!error.to_string().contains(replacement_text));
        assert_eq!(
            fs::read_to_string(temp.path().join("note.txt")).unwrap(),
            "aaaa"
        );
        assert_eq!(lifecycle_snapshot(&session), before);
    }
}

#[test]
fn workspace_text_replacement_requires_a_task_owned_running_turn() {
    let temp = TempDir::new();
    fs::write(temp.path().join("note.txt"), "original").unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, temp.path());
    let (foreign_task_id, foreign_turn_id) = with_current_dir(temp.path(), || {
        let task = session.create_task("Foreign task");
        task.transition(TaskState::Running).unwrap();
        let task_id = task.id();
        let turn_id = task.create_turn().unwrap();
        task.transition_turn(turn_id, TurnState::Running).unwrap();
        (task_id, turn_id)
    });
    let pending_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    let missing_task_id = Uuid::new_v4();
    let missing_turn_id = Uuid::new_v4();
    let before = lifecycle_snapshot(&session);

    assert!(matches!(
        session.replace_workspace_text(
            missing_task_id,
            turn_id,
            Path::new("note.txt"),
            "original",
            "changed"
        ),
        Err(WorkspaceReplaceError::TaskTurn(WorkspaceReadError::TaskNotFound { task_id }))
            if task_id == missing_task_id
    ));
    assert!(matches!(
        session.replace_workspace_text(
            task_id,
            foreign_turn_id,
            Path::new("note.txt"),
            "original",
            "changed"
        ),
        Err(WorkspaceReplaceError::TaskTurn(WorkspaceReadError::ForeignTurn {
            task_id: requested_task_id,
            turn_id,
            owner_task_id,
        })) if requested_task_id == task_id
            && turn_id == foreign_turn_id
            && owner_task_id == foreign_task_id
    ));
    assert!(matches!(
        session.replace_workspace_text(
            task_id,
            missing_turn_id,
            Path::new("note.txt"),
            "original",
            "changed"
        ),
        Err(WorkspaceReplaceError::TaskTurn(WorkspaceReadError::TurnNotFound {
            task_id: actual_task_id,
            turn_id,
        })) if actual_task_id == task_id && turn_id == missing_turn_id
    ));
    assert!(matches!(
        session.replace_workspace_text(
            task_id,
            pending_turn_id,
            Path::new("note.txt"),
            "original",
            "changed"
        ),
        Err(WorkspaceReplaceError::TaskTurn(WorkspaceReadError::TurnNotRunning {
            turn_id,
            state: TurnState::Pending
        })) if turn_id == pending_turn_id
    ));
    assert_eq!(lifecycle_snapshot(&session), before);
    assert_eq!(
        fs::read_to_string(temp.path().join("note.txt")).unwrap(),
        "original"
    );
}

#[test]
fn workspace_file_creation_requires_an_existing_task_owned_running_turn() {
    let temp = TempDir::new();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);
    let (foreign_task_id, foreign_turn_id) = {
        let foreign_task = session.create_task("Foreign task");
        foreign_task.transition(TaskState::Running).unwrap();
        let task_id = foreign_task.id();
        let turn_id = foreign_task.create_turn().unwrap();
        foreign_task
            .transition_turn(turn_id, TurnState::Running)
            .unwrap();
        (task_id, turn_id)
    };
    let pending_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    let missing_task_id = Uuid::new_v4();
    let missing_turn_id = Uuid::new_v4();

    with_current_dir(temp.path(), || {
        let before = lifecycle_snapshot(&session);
        assert!(matches!(
            session.create_workspace_file(
                missing_task_id,
                turn_id,
                Path::new("missing.txt"),
                "text"
            ),
            Err(WorkspaceWriteError::TaskNotFound { task_id }) if task_id == missing_task_id
        ));
        assert!(matches!(
            session.create_workspace_file(
                task_id,
                foreign_turn_id,
                Path::new("foreign.txt"),
                "text"
            ),
            Err(WorkspaceWriteError::ForeignTurn {
                task_id: requested_task_id,
                turn_id,
                owner_task_id,
            }) if requested_task_id == task_id
                && turn_id == foreign_turn_id
                && owner_task_id == foreign_task_id
        ));
        assert!(matches!(
            session.create_workspace_file(
                task_id,
                missing_turn_id,
                Path::new("missing-turn.txt"),
                "text"
            ),
            Err(WorkspaceWriteError::TurnNotFound {
                task_id: actual_task_id,
                turn_id,
            }) if actual_task_id == task_id && turn_id == missing_turn_id
        ));
        assert!(matches!(
            session.create_workspace_file(
                task_id,
                pending_turn_id,
                Path::new("pending.txt"),
                "text"
            ),
            Err(WorkspaceWriteError::TurnNotRunning {
                turn_id,
                state: TurnState::Pending
            }) if turn_id == pending_turn_id
        ));
        assert_eq!(lifecycle_snapshot(&session), before);
        assert!(!temp.path().join("missing.txt").exists());
        assert!(!temp.path().join("foreign.txt").exists());
        assert!(!temp.path().join("missing-turn.txt").exists());
        assert!(!temp.path().join("pending.txt").exists());
    });
}

#[test]
fn failed_workspace_file_creations_preserve_files_and_lifecycle_history() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    let other_directory = temp.path().join("other");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&other_directory).unwrap();
    fs::write(workspace.join("existing.txt"), "keep this").unwrap();
    fs::write(workspace.join("not-a-directory"), "file").unwrap();
    fs::write(outside.join("sentinel.txt"), "outside stays unchanged").unwrap();
    symlink(&outside, workspace.join("outside-link")).unwrap();
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn_in(&mut session, &workspace);
    let oversized = "x".repeat(1024 * 1024 + 1);
    let absolute_path = workspace.join("absolute.txt");

    with_current_dir(&other_directory, || {
        let before = lifecycle_snapshot(&session);
        let failures = [
            (
                Path::new("../outside/sentinel.txt"),
                "text",
                "path must stay inside the current directory",
            ),
            (
                absolute_path.as_path(),
                "text",
                "absolute paths are not allowed",
            ),
            (
                Path::new("outside-link/created.txt"),
                "text",
                "symlink paths are not allowed",
            ),
            (
                Path::new("missing-parent/created.txt"),
                "text",
                "parent directory not found",
            ),
            (
                Path::new("not-a-directory/created.txt"),
                "text",
                "parent is not a directory",
            ),
            (
                Path::new("existing.txt"),
                "replacement",
                "file already exists",
            ),
            (
                Path::new("oversized.txt"),
                oversized.as_str(),
                "text exceeds 1 MiB",
            ),
        ];

        for (path, text, message) in failures {
            let error = session
                .create_workspace_file(task_id, turn_id, path, text)
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(lifecycle_snapshot(&session), before);
        }
    });

    assert_eq!(
        fs::read_to_string(workspace.join("existing.txt")).unwrap(),
        "keep this"
    );
    assert_eq!(
        fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "outside stays unchanged"
    );
    assert!(!outside.join("created.txt").exists());
    assert!(!workspace.join("missing-parent/created.txt").exists());
    assert!(!workspace.join("not-a-directory/created.txt").exists());
    assert!(!workspace.join("oversized.txt").exists());
}

#[test]
fn new_task_keeps_description_and_starts_pending_with_empty_read_only_history() {
    let task = Task::new("Review the release notes");
    assert_eq!(task.description(), "Review the release notes");
    assert_eq!(task.state(), TaskState::Pending);
    assert!(task.events().is_empty());
}

#[test]
fn task_completes_a_running_turn_with_a_success_outcome() {
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);
    let outcome = TurnOutcome::Success {
        result: "completed result".to_owned(),
    };

    assert_eq!(
        session
            .get_task_mut(task_id)
            .unwrap()
            .complete_turn(turn_id, outcome.clone()),
        Ok(())
    );

    let task = session.get_task(task_id).unwrap();
    assert_eq!(task.state(), TaskState::Running);
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(turn.state(), TurnState::Succeeded);
    assert_eq!(turn.outcome(), Some(&outcome));
    assert_eq!(
        turn.events().last().unwrap().previous_state(),
        TurnState::Running
    );
    assert_eq!(
        turn.events().last().unwrap().new_state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Succeeded,
        }
    );
}

#[test]
fn completed_turn_can_be_followed_by_a_new_turn_on_the_same_running_task() {
    let mut session = TaskSession::new();
    let (task_id, first_turn_id) = running_turn(&mut session);
    session
        .complete_turn(
            task_id,
            first_turn_id,
            TurnOutcome::Success {
                result: "continued".to_owned(),
            },
        )
        .unwrap();

    let task = session.get_task_mut(task_id).unwrap();
    assert_eq!(task.state(), TaskState::Running);
    let second_turn_id = task.create_turn().unwrap();
    task.transition_turn(second_turn_id, TurnState::Running)
        .unwrap();
    task.complete_turn(
        second_turn_id,
        TurnOutcome::Success {
            result: "finished".to_owned(),
        },
    )
    .unwrap();
    task.transition(TaskState::Succeeded).unwrap();

    let task = session.get_task(task_id).unwrap();
    assert_eq!(task.state(), TaskState::Succeeded);
    assert_eq!(task.turns().len(), 2);
    assert_eq!(
        task.get_turn(first_turn_id).unwrap().state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.get_turn(second_turn_id).unwrap().state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.history()
            .iter()
            .map(TaskHistoryEvent::transition)
            .collect::<Vec<_>>(),
        [
            TaskHistoryTransition::Task {
                previous_state: TaskState::Pending,
                new_state: TaskState::Running,
            },
            TaskHistoryTransition::TurnCreated {
                turn_id: first_turn_id,
                initial_state: TurnState::Pending,
            },
            TaskHistoryTransition::Turn {
                turn_id: first_turn_id,
                previous_state: TurnState::Pending,
                new_state: TurnState::Running,
            },
            TaskHistoryTransition::Turn {
                turn_id: first_turn_id,
                previous_state: TurnState::Running,
                new_state: TurnState::Succeeded,
            },
            TaskHistoryTransition::TurnCreated {
                turn_id: second_turn_id,
                initial_state: TurnState::Pending,
            },
            TaskHistoryTransition::Turn {
                turn_id: second_turn_id,
                previous_state: TurnState::Pending,
                new_state: TurnState::Running,
            },
            TaskHistoryTransition::Turn {
                turn_id: second_turn_id,
                previous_state: TurnState::Running,
                new_state: TurnState::Succeeded,
            },
            TaskHistoryTransition::Task {
                previous_state: TaskState::Running,
                new_state: TaskState::Succeeded,
            },
        ]
    );
}

#[test]
fn task_session_completes_a_running_turn_with_a_failure_outcome() {
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);
    let outcome = TurnOutcome::Failure {
        detail: "operation failed".to_owned(),
    };

    assert_eq!(
        session.complete_turn(task_id, turn_id, outcome.clone()),
        Ok(())
    );

    let turn = session
        .get_task(task_id)
        .unwrap()
        .get_turn(turn_id)
        .unwrap();
    assert_eq!(turn.state(), TurnState::Failed);
    assert_eq!(turn.outcome(), Some(&outcome));
    assert_eq!(
        turn.events().last().unwrap().previous_state(),
        TurnState::Running
    );
    assert_eq!(turn.events().last().unwrap().new_state(), TurnState::Failed);
}

#[test]
fn invalid_turn_completion_returns_typed_errors_without_mutating_lifecycle_or_outcomes() {
    let mut session = TaskSession::new();
    let (task_id, running_turn_id) = running_turn(&mut session);
    let pending_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    let completed_turn_id = session
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    session
        .get_task_mut(task_id)
        .unwrap()
        .transition_turn(completed_turn_id, TurnState::Running)
        .unwrap();
    session
        .complete_turn(
            task_id,
            completed_turn_id,
            TurnOutcome::Success {
                result: "preserved".to_owned(),
            },
        )
        .unwrap();
    let (foreign_task_id, foreign_turn_id) = running_turn(&mut session);
    let missing_task_id = Uuid::new_v4();
    let missing_turn_id = Uuid::new_v4();
    let before = lifecycle_snapshot(&session);

    assert!(matches!(
        session.complete_turn(
            missing_task_id,
            running_turn_id,
            TurnOutcome::Success {
                result: "unused".to_owned(),
            }
        ),
        Err(TurnCompletionError::TaskNotFound { task_id }) if task_id == missing_task_id
    ));
    assert_eq!(lifecycle_snapshot(&session), before);

    assert!(matches!(
        session.complete_turn(
            task_id,
            missing_turn_id,
            TurnOutcome::Failure {
                detail: "unused".to_owned(),
            }
        ),
        Err(TurnCompletionError::TurnNotFound {
            task_id: actual_task_id,
            turn_id,
        }) if actual_task_id == task_id && turn_id == missing_turn_id
    ));
    assert_eq!(lifecycle_snapshot(&session), before);

    assert!(matches!(
        session.complete_turn(
            task_id,
            foreign_turn_id,
            TurnOutcome::Failure {
                detail: "unused".to_owned(),
            }
        ),
        Err(TurnCompletionError::ForeignTurn {
            task_id: requested_task_id,
            turn_id,
            owner_task_id,
        }) if requested_task_id == task_id
            && turn_id == foreign_turn_id
            && owner_task_id == foreign_task_id
    ));
    assert_eq!(lifecycle_snapshot(&session), before);

    for (turn_id, state) in [
        (pending_turn_id, TurnState::Pending),
        (completed_turn_id, TurnState::Succeeded),
    ] {
        let error = session
            .complete_turn(
                task_id,
                turn_id,
                TurnOutcome::Failure {
                    detail: "unused".to_owned(),
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            TurnCompletionError::TurnNotRunning { turn_id, state }
        );
        assert!(error.to_string().contains("not running"));
        assert_eq!(lifecycle_snapshot(&session), before);
    }
}

#[test]
fn state_only_terminal_transitions_and_cancellation_do_not_create_outcomes() {
    for terminal_state in [TurnState::Succeeded, TurnState::Failed] {
        let mut task = Task::new("State-only completion");
        task.transition(TaskState::Running).unwrap();
        let turn_id = task.create_turn().unwrap();
        task.transition_turn(turn_id, TurnState::Running).unwrap();

        task.transition_turn(turn_id, terminal_state).unwrap();

        assert_eq!(task.get_turn(turn_id).unwrap().outcome(), None);
    }

    let mut task = Task::new("Cancelled task");
    task.transition(TaskState::Running).unwrap();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();
    let explicitly_cancelled_turn_id = task.create_turn().unwrap();
    task.transition_turn(explicitly_cancelled_turn_id, TurnState::Cancelled)
        .unwrap();
    task.cancel().unwrap();

    assert_eq!(
        task.get_turn(turn_id).unwrap().state(),
        TurnState::Cancelled
    );
    assert_eq!(task.get_turn(turn_id).unwrap().outcome(), None);
    assert_eq!(
        task.get_turn(explicitly_cancelled_turn_id)
            .unwrap()
            .outcome(),
        None
    );
}

#[test]
fn turn_creation_is_allowed_for_pending_and_running_tasks_in_creation_order() {
    for task_state in [TaskState::Pending, TaskState::Running] {
        let mut task = Task::new("A task");
        if task_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        let task_id = task.id();

        let first_turn_id = task.create_turn().unwrap();
        let second_turn_id = task.create_turn().unwrap();

        assert_eq!(
            task.turns()
                .iter()
                .map(|turn| turn.id())
                .collect::<Vec<_>>(),
            vec![first_turn_id, second_turn_id]
        );
        assert_eq!(
            task.history().len(),
            if task_state == TaskState::Running {
                3
            } else {
                2
            }
        );
        let first_creation_sequence = if task_state == TaskState::Running {
            2
        } else {
            1
        };
        assert_eq!(
            task.history()
                .iter()
                .filter_map(|event| match event.transition() {
                    TaskHistoryTransition::TurnCreated {
                        turn_id,
                        initial_state,
                    } => Some((event.task_id(), event.sequence(), turn_id, initial_state,)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    task_id,
                    first_creation_sequence,
                    first_turn_id,
                    TurnState::Pending,
                ),
                (
                    task_id,
                    first_creation_sequence + 1,
                    second_turn_id,
                    TurnState::Pending,
                ),
            ]
        );
        for turn in task.turns() {
            assert_eq!(turn.task_id(), task_id);
            assert_eq!(turn.state(), TurnState::Pending);
            assert!(turn.events().is_empty());
        }
    }
}

#[test]
fn turn_creation_rejects_terminal_tasks_without_mutating_state_or_history() {
    for terminal_state in [
        TaskState::Succeeded,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();
        let completed_turn_id = task.create_turn().unwrap();
        let cancelled_turn_id = task.create_turn().unwrap();
        task.transition_turn(completed_turn_id, TurnState::Running)
            .unwrap();
        task.transition_turn(completed_turn_id, TurnState::Succeeded)
            .unwrap();
        task.transition_turn(cancelled_turn_id, TurnState::Cancelled)
            .unwrap();
        task.transition(terminal_state).unwrap();

        let task_state_before = task.state();
        let task_events_before = task.events().to_vec();
        let history_before = task.history().to_vec();
        let turns_before = task
            .turns()
            .iter()
            .map(|turn| {
                (
                    turn.id(),
                    turn.task_id(),
                    turn.state(),
                    turn.events().to_vec(),
                )
            })
            .collect::<Vec<_>>();

        let error = task.create_turn().unwrap_err();

        assert_eq!(error.task_state(), terminal_state);
        assert_eq!(
            error.to_string(),
            format!("cannot create a turn for task in terminal state '{terminal_state}'")
        );
        assert_eq!(task.state(), task_state_before);
        assert_eq!(task.events(), task_events_before);
        assert_eq!(task.history(), history_before);
        assert_eq!(task.turns().len(), turns_before.len());
        assert_eq!(
            task.turns()
                .iter()
                .map(|turn| {
                    (
                        turn.id(),
                        turn.task_id(),
                        turn.state(),
                        turn.events().to_vec(),
                    )
                })
                .collect::<Vec<_>>(),
            turns_before
        );
    }
}

#[test]
fn task_ids_are_unique_and_stable() {
    let mut first = Task::new("First task");
    let second = Task::new("Second task");
    let id = first.id();
    first.transition(TaskState::Running).unwrap();
    first.transition(TaskState::Succeeded).unwrap();
    assert_ne!(id, second.id());
    assert_eq!(first.id(), id);
}

#[test]
fn valid_transitions_record_ordered_task_bound_events() {
    let cases = [
        (TaskState::Pending, TaskState::Running),
        (TaskState::Pending, TaskState::Cancelled),
        (TaskState::Running, TaskState::Succeeded),
        (TaskState::Running, TaskState::Failed),
        (TaskState::Running, TaskState::Cancelled),
    ];
    for (initial, next) in cases {
        let mut task = Task::new("A task");
        if initial == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        let previous = task.state();
        task.transition(next).unwrap();
        let event = task.events().last().unwrap();
        assert_eq!(task.state(), next);
        assert_eq!(
            task.events().len(),
            if initial == TaskState::Running { 2 } else { 1 }
        );
        assert_eq!(event.task_id(), task.id());
        assert_eq!(event.previous_state(), previous);
        assert_eq!(event.new_state(), next);
        assert_eq!(event.sequence(), task.events().len() as u64);
    }
}

#[test]
fn combined_history_records_interleaved_task_and_turn_transitions() {
    let mut task = Task::new("A task");
    let task_id = task.id();
    task.transition(TaskState::Running).unwrap();
    let first_turn_id = task.create_turn().unwrap();
    task.transition_turn(first_turn_id, TurnState::Running)
        .unwrap();

    let second_turn_id = task.create_turn().unwrap();
    task.transition_turn(second_turn_id, TurnState::Cancelled)
        .unwrap();
    task.transition_turn(first_turn_id, TurnState::Succeeded)
        .unwrap();
    task.transition(TaskState::Succeeded).unwrap();

    assert_eq!(task.history().len(), 7);

    assert_eq!(
        task.history()
            .iter()
            .map(|event| (event.task_id(), event.sequence(), event.transition()))
            .collect::<Vec<_>>(),
        vec![
            (
                task_id,
                1,
                TaskHistoryTransition::Task {
                    previous_state: TaskState::Pending,
                    new_state: TaskState::Running,
                },
            ),
            (
                task_id,
                2,
                TaskHistoryTransition::TurnCreated {
                    turn_id: first_turn_id,
                    initial_state: TurnState::Pending,
                },
            ),
            (
                task_id,
                3,
                TaskHistoryTransition::Turn {
                    turn_id: first_turn_id,
                    previous_state: TurnState::Pending,
                    new_state: TurnState::Running,
                },
            ),
            (
                task_id,
                4,
                TaskHistoryTransition::TurnCreated {
                    turn_id: second_turn_id,
                    initial_state: TurnState::Pending,
                },
            ),
            (
                task_id,
                5,
                TaskHistoryTransition::Turn {
                    turn_id: second_turn_id,
                    previous_state: TurnState::Pending,
                    new_state: TurnState::Cancelled,
                },
            ),
            (
                task_id,
                6,
                TaskHistoryTransition::Turn {
                    turn_id: first_turn_id,
                    previous_state: TurnState::Running,
                    new_state: TurnState::Succeeded,
                },
            ),
            (
                task_id,
                7,
                TaskHistoryTransition::Task {
                    previous_state: TaskState::Running,
                    new_state: TaskState::Succeeded,
                },
            ),
        ]
    );

    assert_eq!(task.events()[0].sequence(), 1);
    assert_eq!(task.events()[1].sequence(), 2);
    assert_eq!(
        task.get_turn(first_turn_id).unwrap().events()[1].sequence(),
        2
    );
    assert_eq!(
        task.get_turn(second_turn_id).unwrap().events()[0].sequence(),
        1
    );

    let mut copied_history = task.history().to_vec();
    copied_history.clear();
    assert_eq!(task.history().len(), 7);
}

#[test]
fn invalid_task_and_turn_transitions_leave_every_history_and_state_unchanged() {
    let mut task = Task::new("A task");
    task.transition(TaskState::Running).unwrap();
    let active_turn_id = task.create_turn().unwrap();
    task.transition_turn(active_turn_id, TurnState::Running)
        .unwrap();

    let task_state = task.state();
    let task_events = task.events().to_vec();
    let history = task.history().to_vec();
    let turns = task
        .turns()
        .iter()
        .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
        .collect::<Vec<_>>();

    assert!(task.transition(TaskState::Succeeded).is_err());
    assert_eq!(task.state(), task_state);
    assert_eq!(task.events(), task_events);
    assert_eq!(task.history(), history);
    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
            .collect::<Vec<_>>(),
        turns
    );

    assert!(task
        .transition_turn(active_turn_id, TurnState::Pending)
        .is_err());
    assert_eq!(task.state(), task_state);
    assert_eq!(task.events(), task_events);
    assert_eq!(task.history(), history);
    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
            .collect::<Vec<_>>(),
        turns
    );
}

#[test]
fn invalid_transitions_leave_state_and_history_unchanged() {
    let cases = [
        (&[][..], TaskState::Succeeded),
        (&[][..], TaskState::Failed),
        (&[TaskState::Running][..], TaskState::Pending),
        (
            &[TaskState::Running, TaskState::Succeeded][..],
            TaskState::Failed,
        ),
        (
            &[TaskState::Running, TaskState::Failed][..],
            TaskState::Cancelled,
        ),
        (&[TaskState::Cancelled][..], TaskState::Running),
    ];

    for (prefix, invalid) in cases {
        let mut task = Task::new("A task");
        for state in prefix {
            task.transition(*state).unwrap();
        }
        let previous_state = task.state();
        let previous_events = task.events().to_vec();

        assert!(task.transition(invalid).is_err());
        assert_eq!(task.state(), previous_state);
        assert_eq!(task.events(), previous_events);
    }
}

#[test]
fn terminal_task_transitions_reject_active_turns_without_changing_histories() {
    let cases = [
        (TaskState::Pending, TurnState::Pending, TaskState::Cancelled),
        (TaskState::Running, TurnState::Pending, TaskState::Succeeded),
        (TaskState::Running, TurnState::Pending, TaskState::Failed),
        (TaskState::Running, TurnState::Pending, TaskState::Cancelled),
        (TaskState::Running, TurnState::Running, TaskState::Succeeded),
        (TaskState::Running, TurnState::Running, TaskState::Failed),
        (TaskState::Running, TurnState::Running, TaskState::Cancelled),
    ];

    for (task_state, active_turn_state, next) in cases {
        let mut task = Task::new("A task");
        let active_turn_id = task.create_turn().unwrap();
        let completed_turn_id = task.create_turn().unwrap();

        if task_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        if active_turn_state == TurnState::Running {
            task.transition_turn(active_turn_id, TurnState::Running)
                .unwrap();
        }
        if task_state == TaskState::Running {
            task.transition_turn(completed_turn_id, TurnState::Running)
                .unwrap();
            task.transition_turn(completed_turn_id, TurnState::Succeeded)
                .unwrap();
        }

        let previous_task_state = task.state();
        let previous_task_events = task.events().to_vec();
        let previous_turns: Vec<_> = task
            .turns()
            .iter()
            .map(|turn| (turn.state(), turn.events().to_vec()))
            .collect();

        let error = match task.transition(next) {
            Err(error) => error,
            Ok(()) => panic!("task transitioned to {next} while a turn was active"),
        };

        assert_eq!(error.from(), task_state);
        assert_eq!(error.to(), next);
        assert_eq!(
            error.blocking_turn(),
            Some((active_turn_id, active_turn_state))
        );
        let message = error.to_string();
        assert!(message.contains(active_turn_state.as_str()), "{message}");
        assert!(message.contains(&active_turn_id.to_string()), "{message}");
        assert_eq!(task.state(), previous_task_state);
        assert_eq!(task.events(), previous_task_events);
        let current_turns: Vec<_> = task
            .turns()
            .iter()
            .map(|turn| (turn.state(), turn.events().to_vec()))
            .collect();
        assert_eq!(current_turns, previous_turns);
    }
}

#[test]
fn terminal_task_transitions_succeed_after_all_turns_finish() {
    for next in [
        TaskState::Succeeded,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();

        let succeeded_turn = task.create_turn().unwrap();
        task.transition_turn(succeeded_turn, TurnState::Running)
            .unwrap();
        task.transition_turn(succeeded_turn, TurnState::Succeeded)
            .unwrap();

        let failed_turn = task.create_turn().unwrap();
        task.transition_turn(failed_turn, TurnState::Running)
            .unwrap();
        task.transition_turn(failed_turn, TurnState::Failed)
            .unwrap();

        let cancelled_turn = task.create_turn().unwrap();
        task.transition_turn(cancelled_turn, TurnState::Cancelled)
            .unwrap();

        task.transition(next).unwrap();
        assert_eq!(task.state(), next);
        assert_eq!(task.events().last().unwrap().new_state(), next);
    }
}

#[test]
fn separate_tasks_keep_events_isolated() {
    let mut first = Task::new("First task");
    let mut second = Task::new("Second task");
    first.transition(TaskState::Running).unwrap();
    first.create_turn().unwrap();
    second.create_turn().unwrap();
    assert_eq!(first.events().len(), 1);
    assert!(second.events().is_empty());
    assert_eq!(first.events()[0].task_id(), first.id());
    assert_ne!(first.events()[0].task_id(), second.id());
    assert_eq!(
        first.history()[0].transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Pending,
            new_state: TaskState::Running,
        }
    );
    assert_eq!(first.history()[0].task_id(), first.id());
    assert_eq!(first.history().len(), 2);
    assert_eq!(first.history()[1].task_id(), first.id());
    assert_eq!(
        first.history()[1].transition(),
        TaskHistoryTransition::TurnCreated {
            turn_id: first.turns()[0].id(),
            initial_state: TurnState::Pending,
        }
    );
    assert_eq!(second.history().len(), 1);
    assert_eq!(second.history()[0].task_id(), second.id());
    assert_eq!(
        second.history()[0].transition(),
        TaskHistoryTransition::TurnCreated {
            turn_id: second.turns()[0].id(),
            initial_state: TurnState::Pending,
        }
    );
}

#[test]
fn sessions_have_unique_stable_ids_and_start_empty() {
    let first = TaskSession::new();
    let second = TaskSession::new();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.list_tasks().len(), 0);
}

#[test]
fn sessions_create_find_and_list_tasks_in_creation_order() {
    let mut session = TaskSession::new();
    let first_id = session.create_task("First task").id();
    let second_id = session.create_task("Second task").id();
    let first = session.get_task(first_id).unwrap();
    assert_eq!(first.description(), "First task");
    assert_eq!(first.state(), TaskState::Pending);
    assert_eq!(session.list_tasks()[0].id(), first_id);
    assert_eq!(session.list_tasks()[1].id(), second_id);
    assert!(session.get_task(uuid::Uuid::nil()).is_none());
}

#[test]
fn task_lookup_and_events_are_isolated_between_sessions() {
    let mut first_session = TaskSession::new();
    let mut second_session = TaskSession::new();
    let first_id = first_session.create_task("A task").id();
    let second_id = second_session.create_task("A task").id();
    first_session
        .get_task_mut(first_id)
        .unwrap()
        .transition(TaskState::Running)
        .unwrap();

    assert_ne!(first_id, second_id);
    assert!(first_session.get_task(second_id).is_none());
    assert!(second_session.get_task(first_id).is_none());
    assert_eq!(first_session.get_task(first_id).unwrap().events().len(), 1);
    assert!(second_session
        .get_task(second_id)
        .unwrap()
        .events()
        .is_empty());
}

#[test]
fn tasks_create_stable_pending_turns_in_creation_order() {
    let mut first_task = Task::new("First task");
    let mut second_task = Task::new("Second task");
    let first_task_id = first_task.id();
    let second_task_id = second_task.id();

    let first_turn_id = first_task.create_turn().unwrap();
    let second_turn_id = first_task.create_turn().unwrap();
    let other_turn_id = second_task.create_turn().unwrap();

    assert_ne!(first_task_id, second_task_id);
    assert_ne!(first_turn_id, second_turn_id);
    assert_ne!(first_turn_id, other_turn_id);

    let turns = first_task.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].id(), first_turn_id);
    assert_eq!(turns[1].id(), second_turn_id);
    assert_eq!(turns[0].task_id(), first_task_id);
    assert_eq!(turns[1].task_id(), first_task_id);
    assert_eq!(turns[0].state(), TurnState::Pending);
    assert_eq!(turns[1].state(), TurnState::Pending);
    assert!(turns[0].events().is_empty());
    assert!(turns[1].events().is_empty());
    assert_eq!(
        first_task.get_turn(first_turn_id).unwrap().id(),
        first_turn_id
    );
    assert!(first_task.get_turn(other_turn_id).is_none());
    assert_eq!(
        first_task.transition_turn(other_turn_id, TurnState::Running),
        Err(TurnTransitionError::UnknownTurn {
            turn_id: other_turn_id
        })
    );

    first_task.transition(TaskState::Running).unwrap();
    first_task
        .transition_turn(first_turn_id, TurnState::Running)
        .unwrap();
    assert_eq!(first_task.turns()[0].id(), first_turn_id);
    assert_eq!(first_task.turns()[0].state(), TurnState::Running);
}

#[test]
fn pending_turn_cannot_start_until_its_task_is_running() {
    let mut session = TaskSession::new();
    let task = session.create_task("A pending task");
    let task_id = task.id();
    let turn_id = task.create_turn().unwrap();
    let before = lifecycle_snapshot(&session);

    let error = session
        .get_task_mut(task_id)
        .unwrap()
        .transition_turn(turn_id, TurnState::Running)
        .unwrap_err();

    assert_eq!(
        error,
        TurnTransitionError::TaskNotRunning {
            task_id,
            task_state: TaskState::Pending,
            turn_id,
            turn_state: TurnState::Pending,
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "cannot start turn '{turn_id}' in state 'pending' while owning task '{task_id}' is 'pending'"
        )
    );
    assert_eq!(lifecycle_snapshot(&session), before);
}

#[test]
fn pending_turn_can_start_after_its_task_starts() {
    let mut task = Task::new("A task");
    let turn_id = task.create_turn().unwrap();

    task.transition(TaskState::Running).unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();

    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(task.get_turn(turn_id).unwrap().state(), TurnState::Running);
    assert_eq!(
        task.get_turn(turn_id).unwrap().events()[0].previous_state(),
        TurnState::Pending
    );
}

#[test]
fn pending_turn_can_be_cancelled_while_its_task_is_pending() {
    let mut task = Task::new("A pending task");
    let turn_id = task.create_turn().unwrap();

    task.transition_turn(turn_id, TurnState::Cancelled).unwrap();

    assert_eq!(task.state(), TaskState::Pending);
    assert_eq!(
        task.get_turn(turn_id).unwrap().state(),
        TurnState::Cancelled
    );
    assert_eq!(
        task.get_turn(turn_id).unwrap().events()[0].previous_state(),
        TurnState::Pending
    );
}

#[test]
fn valid_turn_transitions_record_ordered_immutable_events() {
    let cases = [
        (vec![], TurnState::Running),
        (vec![], TurnState::Cancelled),
        (vec![TurnState::Running], TurnState::Succeeded),
        (vec![TurnState::Running], TurnState::Failed),
        (vec![TurnState::Running], TurnState::Cancelled),
    ];

    for (prefix, next) in cases {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();
        let task_id = task.id();
        let turn_id = task.create_turn().unwrap();
        let mut expected_states = prefix;
        expected_states.push(next);

        for state in &expected_states {
            task.transition_turn(turn_id, *state).unwrap();
        }

        let turn = task.get_turn(turn_id).unwrap();
        let events = turn.events();
        assert_eq!(events.len(), expected_states.len());
        let mut previous_state = TurnState::Pending;
        for (index, (event, new_state)) in events
            .iter()
            .zip(expected_states.iter().copied())
            .enumerate()
        {
            assert_eq!(event.task_id(), task_id);
            assert_eq!(event.turn_id(), turn_id);
            assert_eq!(event.sequence(), index as u64 + 1);
            assert_eq!(event.previous_state(), previous_state);
            assert_eq!(event.new_state(), new_state);
            previous_state = new_state;
        }
        assert_eq!(turn.state(), next);

        let mut copied_history = turn.events().to_vec();
        copied_history.clear();
        assert_eq!(turn.events().len(), expected_states.len());
    }
}

#[test]
fn invalid_turn_transitions_leave_state_and_history_unchanged() {
    let cases: &[(&[TurnState], TurnState)] = &[
        (&[], TurnState::Pending),
        (&[], TurnState::Succeeded),
        (&[], TurnState::Failed),
        (&[TurnState::Running], TurnState::Pending),
        (&[TurnState::Running], TurnState::Running),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Pending,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Running,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Succeeded,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Failed,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Cancelled,
        ),
        (&[TurnState::Running, TurnState::Failed], TurnState::Pending),
        (&[TurnState::Running, TurnState::Failed], TurnState::Running),
        (
            &[TurnState::Running, TurnState::Failed],
            TurnState::Succeeded,
        ),
        (&[TurnState::Running, TurnState::Failed], TurnState::Failed),
        (
            &[TurnState::Running, TurnState::Failed],
            TurnState::Cancelled,
        ),
        (&[TurnState::Cancelled], TurnState::Pending),
        (&[TurnState::Cancelled], TurnState::Running),
        (&[TurnState::Cancelled], TurnState::Succeeded),
        (&[TurnState::Cancelled], TurnState::Failed),
        (&[TurnState::Cancelled], TurnState::Cancelled),
    ];

    for (prefix, invalid) in cases {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();
        let turn_id = task.create_turn().unwrap();
        for state in *prefix {
            task.transition_turn(turn_id, *state).unwrap();
        }
        let turn = task.get_turn(turn_id).unwrap();
        let previous_state = turn.state();
        let previous_events = turn.events().to_vec();

        let error = task.transition_turn(turn_id, *invalid).unwrap_err();
        assert_eq!(
            error,
            TurnTransitionError::InvalidTransition {
                from: previous_state,
                to: *invalid,
            }
        );
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), previous_state);
        assert_eq!(turn.events(), previous_events);
    }
}

#[test]
fn turns_keep_state_and_history_isolated_between_tasks() {
    let mut first_task = Task::new("First task");
    let mut second_task = Task::new("Second task");
    let first_task_id = first_task.id();
    let second_task_id = second_task.id();
    let first_turn_id = first_task.create_turn().unwrap();
    first_task.create_turn().unwrap();
    let other_turn_id = second_task.create_turn().unwrap();

    first_task.transition(TaskState::Running).unwrap();
    first_task
        .transition_turn(first_turn_id, TurnState::Running)
        .unwrap();

    assert_eq!(first_task.turns()[0].events()[0].task_id(), first_task_id);
    assert_eq!(first_task.turns()[0].events()[0].turn_id(), first_turn_id);
    assert!(first_task.turns()[1].events().is_empty());
    assert!(second_task.turns()[0].events().is_empty());
    assert!(first_task.get_turn(other_turn_id).is_none());
    assert!(second_task.get_turn(first_turn_id).is_none());
    assert_eq!(second_task.turns()[0].task_id(), second_task_id);
    assert_eq!(second_task.turns()[0].id(), other_turn_id);
}

#[test]
fn task_cancel_cancels_active_turns_before_task_and_preserves_terminal_turns() {
    let mut task = Task::new("A task");
    task.transition(TaskState::Running).unwrap();

    let pending_turn_id = task.create_turn().unwrap();
    let running_turn_id = task.create_turn().unwrap();
    let succeeded_turn_id = task.create_turn().unwrap();
    let failed_turn_id = task.create_turn().unwrap();
    let cancelled_turn_id = task.create_turn().unwrap();

    task.transition_turn(running_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(succeeded_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(succeeded_turn_id, TurnState::Succeeded)
        .unwrap();
    task.transition_turn(failed_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(failed_turn_id, TurnState::Failed)
        .unwrap();
    task.transition_turn(cancelled_turn_id, TurnState::Cancelled)
        .unwrap();

    let task_events_before = task.events().to_vec();
    let succeeded_events_before = task.get_turn(succeeded_turn_id).unwrap().events().to_vec();
    let failed_events_before = task.get_turn(failed_turn_id).unwrap().events().to_vec();
    let cancelled_events_before = task.get_turn(cancelled_turn_id).unwrap().events().to_vec();

    task.cancel().unwrap();

    assert_eq!(task.state(), TaskState::Cancelled);
    assert_eq!(task.history().len(), 15);
    assert_eq!(
        task.history()[12].transition(),
        TaskHistoryTransition::Turn {
            turn_id: pending_turn_id,
            previous_state: TurnState::Pending,
            new_state: TurnState::Cancelled,
        }
    );
    assert_eq!(
        task.history()[13].transition(),
        TaskHistoryTransition::Turn {
            turn_id: running_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Cancelled,
        }
    );
    assert_eq!(
        task.history()[14].transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Running,
            new_state: TaskState::Cancelled,
        }
    );
    assert_eq!(task.history()[12].sequence(), 13);
    assert_eq!(task.history()[13].sequence(), 14);
    assert_eq!(task.history()[14].sequence(), 15);
    assert_eq!(task.events().len(), task_events_before.len() + 1);
    let task_event = task.events().last().unwrap();
    assert_eq!(task_event.task_id(), task.id());
    assert_eq!(task_event.sequence(), task_events_before.len() as u64 + 1);
    assert_eq!(task_event.previous_state(), TaskState::Running);
    assert_eq!(task_event.new_state(), TaskState::Cancelled);

    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| turn.id())
            .collect::<Vec<_>>(),
        vec![
            pending_turn_id,
            running_turn_id,
            succeeded_turn_id,
            failed_turn_id,
            cancelled_turn_id,
        ]
    );
    for (turn_id, previous_state, previous_event_count) in [
        (pending_turn_id, TurnState::Pending, 0),
        (running_turn_id, TurnState::Running, 1),
    ] {
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), TurnState::Cancelled);
        let event = turn.events().last().unwrap();
        assert_eq!(event.task_id(), task.id());
        assert_eq!(event.turn_id(), turn_id);
        assert_eq!(event.sequence(), previous_event_count + 1);
        assert_eq!(event.previous_state(), previous_state);
        assert_eq!(event.new_state(), TurnState::Cancelled);
    }

    assert_eq!(
        task.get_turn(succeeded_turn_id).unwrap().state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.get_turn(succeeded_turn_id).unwrap().events(),
        succeeded_events_before
    );
    assert_eq!(
        task.get_turn(failed_turn_id).unwrap().state(),
        TurnState::Failed
    );
    assert_eq!(
        task.get_turn(failed_turn_id).unwrap().events(),
        failed_events_before
    );
    assert_eq!(
        task.get_turn(cancelled_turn_id).unwrap().state(),
        TurnState::Cancelled
    );
    assert_eq!(
        task.get_turn(cancelled_turn_id).unwrap().events(),
        cancelled_events_before
    );
}

#[test]
fn session_cancellation_records_metadata_before_consistent_terminal_history() {
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);
    let activity = [
        TurnActivitySummary::FileRead { bytes_read: 16 },
        TurnActivitySummary::FileCreated { bytes_written: 15 },
        TurnActivitySummary::Cancelled,
    ];

    session
        .cancel_turn_with_activity(task_id, turn_id, &activity)
        .unwrap();

    let task = session.get_task(task_id).unwrap();
    let turn = task.get_turn(turn_id).unwrap();
    assert_eq!(task.state(), TaskState::Cancelled);
    assert_eq!(turn.state(), TurnState::Cancelled);
    assert_eq!(turn.outcome(), None);
    assert_eq!(turn.activity_history(), activity);
    assert_eq!(
        task.activity_history()
            .iter()
            .map(|event| event.summary())
            .collect::<Vec<_>>(),
        activity
    );
    assert_eq!(
        task.history()[task.history().len() - 2].transition(),
        TaskHistoryTransition::Turn {
            turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Cancelled,
        }
    );
    assert_eq!(
        task.history().last().unwrap().transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Running,
            new_state: TaskState::Cancelled,
        }
    );
}

#[test]
fn task_cancel_cancels_pending_turns_of_a_pending_task() {
    let mut task = Task::new("A pending task");
    let first_turn_id = task.create_turn().unwrap();
    let second_turn_id = task.create_turn().unwrap();
    let task_id = task.id();

    task.cancel().unwrap();

    assert_eq!(task.state(), TaskState::Cancelled);
    assert_eq!(task.events().len(), 1);
    assert_eq!(task.events()[0].task_id(), task_id);
    assert_eq!(task.events()[0].sequence(), 1);
    assert_eq!(task.events()[0].previous_state(), TaskState::Pending);
    assert_eq!(task.events()[0].new_state(), TaskState::Cancelled);
    assert_eq!(task.turns()[0].id(), first_turn_id);
    assert_eq!(task.turns()[1].id(), second_turn_id);
    for (sequence, turn_id) in [(1, first_turn_id), (1, second_turn_id)] {
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), TurnState::Cancelled);
        assert_eq!(turn.events().len(), 1);
        assert_eq!(turn.events()[0].task_id(), task_id);
        assert_eq!(turn.events()[0].turn_id(), turn_id);
        assert_eq!(turn.events()[0].sequence(), sequence);
        assert_eq!(turn.events()[0].previous_state(), TurnState::Pending);
        assert_eq!(turn.events()[0].new_state(), TurnState::Cancelled);
    }
}

#[test]
fn task_cancel_succeeds_for_tasks_without_turns() {
    for initial_state in [TaskState::Pending, TaskState::Running] {
        let mut task = Task::new("A task without turns");
        if initial_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }

        task.cancel().unwrap();

        assert_eq!(task.state(), TaskState::Cancelled);
        assert_eq!(
            task.events().last().unwrap().previous_state(),
            initial_state
        );
        assert_eq!(
            task.events().last().unwrap().new_state(),
            TaskState::Cancelled
        );
        assert!(task.turns().is_empty());
    }
}

#[test]
fn task_cancel_rejects_terminal_tasks_without_mutating_task_or_turn_history() {
    for (terminal_state, turn_terminal_state) in [
        (TaskState::Succeeded, TurnState::Succeeded),
        (TaskState::Failed, TurnState::Failed),
        (TaskState::Cancelled, TurnState::Cancelled),
    ] {
        let mut task = Task::new("A terminal task");
        let active_turn_id = task.create_turn().unwrap();
        if terminal_state == TaskState::Cancelled {
            task.cancel().unwrap();
        } else {
            task.transition(TaskState::Running).unwrap();
            task.transition_turn(active_turn_id, TurnState::Running)
                .unwrap();
            task.transition_turn(active_turn_id, turn_terminal_state)
                .unwrap();
            task.transition(terminal_state).unwrap();
        }

        let task_state_before = task.state();
        let task_events_before = task.events().to_vec();
        let turn_state_before = task.get_turn(active_turn_id).unwrap().state();
        let turn_events_before = task.get_turn(active_turn_id).unwrap().events().to_vec();

        let error = task.cancel().unwrap_err();

        assert_eq!(error.from(), terminal_state);
        assert_eq!(error.to(), TaskState::Cancelled);
        assert_eq!(error.blocking_turn(), None);
        assert!(error.to_string().contains("cannot transition task"));
        assert_eq!(task.state(), task_state_before);
        assert_eq!(task.events(), task_events_before);
        let turn = task.get_turn(active_turn_id).unwrap();
        assert_eq!(turn.state(), turn_state_before);
        assert_eq!(turn.events(), turn_events_before);
    }
}
