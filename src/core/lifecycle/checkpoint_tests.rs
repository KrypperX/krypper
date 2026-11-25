use super::*;
use crate::CURRENT_DIR_TEST_LOCK;
use proptest::prelude::*;
use std::fs;

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
    let _lock = CURRENT_DIR_TEST_LOCK
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

fn checkpoint_description() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..32)
        .prop_map(|characters| characters.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn generated_task_snapshots_survive_checkpoint_round_trip(
        descriptions in prop::collection::vec(checkpoint_description(), 0..8),
    ) {
        let workspace = TempWorkspace::new();
        let mut session = TaskSession::new_in_workspace(&workspace.0).unwrap();
        for description in descriptions {
            session.create_task(description);
        }
        let expected = session.snapshot();
        let bytes = session.export_checkpoint().unwrap();
        prop_assert!(bytes.len() <= MAX_TASK_SESSION_CHECKPOINT_BYTES);

        let restored = TaskSession::restore_checkpoint_in_workspace(&bytes, &workspace.0).unwrap();
        prop_assert_eq!(restored.snapshot(), expected.clone());
        for task in expected.tasks() {
            let before = session.get_task(task.id()).unwrap();
            let after = restored.get_task(task.id()).unwrap();
            prop_assert_eq!(after.description(), before.description());
        }
    }

    #[test]
    fn generated_turn_histories_and_outcomes_survive_checkpoint_restore(
        description in checkpoint_description(),
        outcomes in prop::collection::vec(any::<bool>(), 0..6),
        pending_turn in any::<bool>(),
        terminal_state in 0u8..4,
    ) {
        let workspace = TempWorkspace::new();
        let mut session = TaskSession::new_in_workspace(&workspace.0).unwrap();
        let task_id = session.create_task(description).id();
        session
            .get_task_mut(task_id)
            .unwrap()
            .transition(TaskState::Running)
            .unwrap();

        for (index, succeeded) in outcomes.into_iter().enumerate() {
            let task = session.get_task_mut(task_id).unwrap();
            let turn_id = task.create_turn().unwrap();
            task.transition_turn(turn_id, TurnState::Running).unwrap();
            let outcome = if succeeded {
                TurnOutcome::Success {
                    result: format!("generated result {index}"),
                }
            } else {
                TurnOutcome::Failure {
                    detail: format!("generated failure {index}"),
                }
            };
            let activity = [
                TurnActivitySummary::ProcessFinished {
                    succeeded,
                    output_bytes: index,
                },
                if succeeded {
                    TurnActivitySummary::Completed
                } else {
                    TurnActivitySummary::BudgetExhausted { actions: index }
                },
            ];
            session
                .complete_turn_with_activity(task_id, turn_id, outcome, &activity)
                .unwrap();
        }

        if terminal_state == 0 {
            if pending_turn {
                session
                    .get_task_mut(task_id)
                    .unwrap()
                    .create_turn()
                    .unwrap();
            }
        } else {
            let terminal_state = match terminal_state {
                1 => TaskState::Succeeded,
                2 => TaskState::Failed,
                _ => TaskState::Cancelled,
            };
            session
                .get_task_mut(task_id)
                .unwrap()
                .transition(terminal_state)
                .unwrap();
        }

        let expected_snapshot = session.snapshot();
        let expected_task = session.get_task(task_id).unwrap();
        let bytes = session.export_checkpoint().unwrap();
        let restored = TaskSession::restore_checkpoint_in_workspace(&bytes, &workspace.0).unwrap();
        let restored_task = restored.get_task(task_id).unwrap();

        prop_assert_eq!(restored.snapshot(), expected_snapshot);
        prop_assert_eq!(restored_task.events(), expected_task.events());
        prop_assert_eq!(restored_task.history(), expected_task.history());
        prop_assert_eq!(restored_task.activity_history(), expected_task.activity_history());
        prop_assert_eq!(restored_task.turns().len(), expected_task.turns().len());
        for (before, after) in expected_task.turns().iter().zip(restored_task.turns()) {
            prop_assert_eq!(after.id(), before.id());
            prop_assert_eq!(after.state(), before.state());
            prop_assert_eq!(after.outcome(), before.outcome());
            prop_assert_eq!(after.events(), before.events());
            prop_assert_eq!(after.activity_history(), before.activity_history());
        }

        let mut document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        document["tasks"][0]["history"][0]["sequence"] = serde_json::json!(0);
        let corrupted = serde_json::to_vec(&document).unwrap();
        prop_assert_eq!(
            TaskSession::restore_checkpoint_in_workspace(&corrupted, &workspace.0).unwrap_err(),
            TaskSessionCheckpointError::InvalidCheckpoint,
        );
    }

    #[test]
    fn arbitrary_bounded_checkpoint_bytes_fail_closed_or_reexport_validly(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let workspace = TempWorkspace::new();
        if let Ok(restored) = TaskSession::restore_checkpoint_in_workspace(&bytes, &workspace.0) {
            let expected = restored.snapshot();
            let canonical = restored.export_checkpoint().unwrap();
            let round_trip =
                TaskSession::restore_checkpoint_in_workspace(&canonical, &workspace.0).unwrap();
            prop_assert_eq!(round_trip.snapshot(), expected);
        }
    }

    #[test]
    fn restore_rejects_generated_duplicate_task_identifiers(task_count in 2usize..8) {
        let workspace = TempWorkspace::new();
        let mut session = TaskSession::new_in_workspace(&workspace.0).unwrap();
        for index in 0..task_count {
            session.create_task(format!("generated task {index}"));
        }
        let bytes = session.export_checkpoint().unwrap();
        let mut document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tasks = document["tasks"].as_array_mut().unwrap();
        let first_id = tasks[0]["id"].clone();
        tasks[1]["id"] = first_id;
        let corrupted = serde_json::to_vec(&document).unwrap();

        let result = TaskSession::restore_checkpoint_in_workspace(&corrupted, &workspace.0);
        prop_assert_eq!(result.unwrap_err(), TaskSessionCheckpointError::InvalidCheckpoint);
    }
}
