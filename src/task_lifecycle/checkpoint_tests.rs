use super::*;
use std::fs;
use std::sync::Mutex;

static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("krypper-checkpoint-workspace-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct RestoreCurrentDir(PathBuf);

impl RestoreCurrentDir {
    fn enter(path: &std::path::Path) -> Self {
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

#[test]
fn checkpoint_preserves_only_metadata_for_workspace_activity() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace = TempWorkspace::new();
    let _restore = RestoreCurrentDir::enter(&workspace.0);

    let read_secret = "read_excerpt_sentinel";
    let query_secret = "search_query_sentinel";
    let replacement_match = "replacement_match_sentinel";
    let replacement_secret = "replacement_payload_sentinel";
    let create_secret = "create_payload_sentinel";
    fs::write(
        "source.txt",
        format!("{read_secret} {query_secret} {replacement_match}"),
    )
    .unwrap();

    let mut session = TaskSession::new();
    let task = session.create_task("preserve metadata activity");
    task.transition(TaskState::Running).unwrap();
    let task_id = task.id();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();

    let read_result = session
        .read_workspace_file(task_id, turn_id, Path::new("source.txt"))
        .unwrap();
    assert!(read_result.contains(read_secret));
    assert_eq!(
        session
            .search_workspace_text(task_id, turn_id, Path::new("."), query_secret)
            .unwrap()
            .len(),
        1
    );
    session
        .create_workspace_file(task_id, turn_id, Path::new("created.txt"), create_secret)
        .unwrap();
    session
        .replace_workspace_text(
            task_id,
            turn_id,
            Path::new("source.txt"),
            replacement_match,
            replacement_secret,
        )
        .unwrap();

    let activity = [
        TurnActivitySummary::FileRead {
            bytes_read: read_result.len(),
        },
        TurnActivitySummary::FileRangeRead {
            start_line: 1,
            end_line: 1,
            bytes_returned: read_result.len(),
        },
        TurnActivitySummary::TextSearched { location_count: 1 },
        TurnActivitySummary::FileCreated {
            bytes_written: create_secret.len(),
        },
        TurnActivitySummary::TextReplaced {
            bytes_written: replacement_secret.len(),
        },
        TurnActivitySummary::BudgetExhausted { actions: 5 },
    ];
    session
        .complete_turn_with_activity(
            task_id,
            turn_id,
            TurnOutcome::Failure {
                detail: "action budget exhausted".to_owned(),
            },
            &activity,
        )
        .unwrap();

    let bytes = session.export_checkpoint().unwrap();
    for secret in [
        read_secret,
        query_secret,
        replacement_match,
        replacement_secret,
        create_secret,
    ] {
        assert!(!bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
    }

    let restored = TaskSession::restore_checkpoint(&bytes).unwrap();
    let restored_task = restored.get_task(task_id).unwrap();
    let restored_turn = restored_task.get_turn(turn_id).unwrap();
    assert_eq!(restored_task.description(), "preserve metadata activity");
    assert_eq!(
        restored_turn.outcome(),
        Some(&TurnOutcome::Failure {
            detail: "action budget exhausted".to_owned()
        })
    );
    assert_eq!(restored_turn.activity_history(), activity);
    let restored_summaries: Vec<_> = restored_task
        .activity_history()
        .iter()
        .map(TaskActivityEvent::summary)
        .collect();
    assert_eq!(restored_summaries, activity);
    assert!(restored_task
        .activity_history()
        .iter()
        .enumerate()
        .all(|(index, event)| event.sequence() == index as u64 + 1));
    assert_eq!(fs::read_to_string("created.txt").unwrap(), create_secret);
    assert_eq!(
        fs::read_to_string("source.txt").unwrap(),
        format!("{read_secret} {query_secret} {replacement_secret}")
    );
}
