use super::*;
use krypper::task_lifecycle::TaskSessionCheckpointError;

#[test]
fn task_session_checkpoint_round_trip_preserves_lifecycle_and_allows_a_fresh_turn() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut session = TaskSession::new();
    let task = session.create_task("continue the bounded task");
    task.transition(TaskState::Running).unwrap();
    let task_id = task.id();
    let old_turn_id = task.create_turn().unwrap();
    task.transition_turn(old_turn_id, TurnState::Running)
        .unwrap();
    session
        .complete_turn(
            task_id,
            old_turn_id,
            TurnOutcome::Success {
                result: "first turn complete".to_owned(),
            },
        )
        .unwrap();

    let original = session.get_task(task_id).unwrap();
    let original_events = original.events().to_vec();
    let original_history = original.history().to_vec();
    let original_turn = original.get_turn(old_turn_id).unwrap();
    let original_turn_events = original_turn.events().to_vec();
    let original_outcome = original_turn.outcome().cloned();
    let bytes = session.export_checkpoint().unwrap();

    let mut restored = TaskSession::restore_checkpoint(&bytes).unwrap();
    assert_eq!(restored.id(), session.id());
    let restored_task = restored.get_task(task_id).unwrap();
    assert_eq!(restored_task.description(), "continue the bounded task");
    assert_eq!(restored_task.state(), TaskState::Running);
    assert_eq!(restored_task.events(), original_events);
    assert_eq!(restored_task.history(), original_history);
    let restored_turn = restored_task.get_turn(old_turn_id).unwrap();
    assert_eq!(restored_turn.state(), TurnState::Succeeded);
    assert_eq!(restored_turn.events(), original_turn_events);
    assert_eq!(restored_turn.outcome().cloned(), original_outcome);

    let new_turn_id = restored
        .get_task_mut(task_id)
        .unwrap()
        .create_turn()
        .unwrap();
    assert_ne!(new_turn_id, old_turn_id);
    restored
        .get_task_mut(task_id)
        .unwrap()
        .transition_turn(new_turn_id, TurnState::Running)
        .unwrap();
    assert_eq!(
        restored
            .get_task(task_id)
            .unwrap()
            .get_turn(new_turn_id)
            .unwrap()
            .state(),
        TurnState::Running
    );
}

#[test]
fn task_session_checkpoint_preserves_manual_terminal_turns_without_outcomes() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut session = TaskSession::new();
    let task = session.create_task("manually transitioned task");
    task.transition(TaskState::Running).unwrap();
    let task_id = task.id();
    let turn_id = task.create_turn().unwrap();
    task.transition_turn(turn_id, TurnState::Running).unwrap();
    task.transition_turn(turn_id, TurnState::Succeeded).unwrap();

    let restored = TaskSession::restore_checkpoint(&session.export_checkpoint().unwrap()).unwrap();
    let restored_turn = restored
        .get_task(task_id)
        .unwrap()
        .get_turn(turn_id)
        .unwrap();
    assert_eq!(restored_turn.state(), TurnState::Succeeded);
    assert_eq!(restored_turn.outcome(), None);
}

#[test]
fn task_session_checkpoint_export_rejects_a_running_turn() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut session = TaskSession::new();
    let (task_id, turn_id) = running_turn(&mut session);

    let error = session.export_checkpoint().unwrap_err();

    assert!(matches!(
        error,
        TaskSessionCheckpointError::RunningTurn {
            task_id: error_task_id,
            turn_id: error_turn_id
        } if error_task_id == task_id && error_turn_id == turn_id
    ));
    assert_eq!(
        session
            .get_task(task_id)
            .unwrap()
            .get_turn(turn_id)
            .unwrap()
            .state(),
        TurnState::Running
    );
}

#[test]
fn task_session_checkpoint_export_rejects_oversized_text() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut session = TaskSession::new();
    session.create_task("x".repeat(1_048_577));

    assert_eq!(
        session.export_checkpoint(),
        Err(TaskSessionCheckpointError::TooLarge)
    );
}

#[test]
fn task_session_checkpoint_restore_rejects_malformed_unsupported_oversized_and_duplicate_data() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(TaskSession::restore_checkpoint(b"{").is_err());

    let mut session = TaskSession::new();
    session.create_task("first task");
    session.create_task("second task");
    let bytes = session.export_checkpoint().unwrap();

    let mut unsupported: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    unsupported["version"] = serde_json::json!(2);
    let unsupported = serde_json::to_vec(&unsupported).unwrap();
    assert!(TaskSession::restore_checkpoint(&unsupported).is_err());

    let mut duplicate_ids: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let first_id = duplicate_ids["tasks"][0]["id"].clone();
    duplicate_ids["tasks"][1]["id"] = first_id;
    let duplicate_ids = serde_json::to_vec(&duplicate_ids).unwrap();
    assert!(TaskSession::restore_checkpoint(&duplicate_ids).is_err());

    assert!(TaskSession::restore_checkpoint(&vec![b' '; 1_048_577]).is_err());
}

#[test]
fn task_session_checkpoint_restore_rejects_inconsistent_history() {
    let _lock = CURRENT_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut session = TaskSession::new();
    let task = session.create_task("running task");
    task.transition(TaskState::Running).unwrap();
    let bytes = session.export_checkpoint().unwrap();

    let mut inconsistent: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    inconsistent["tasks"][0]["history"][0]["sequence"] = serde_json::json!(9);
    let inconsistent = serde_json::to_vec(&inconsistent).unwrap();

    assert!(TaskSession::restore_checkpoint(&inconsistent).is_err());
}

#[test]
fn task_session_checkpoint_restore_requires_the_original_workspace() {
    let original_workspace = TempDir::new();
    let other_workspace = TempDir::new();
    let bytes = with_current_dir(original_workspace.path(), || {
        let mut session = TaskSession::new();
        session.create_task("workspace-bound task");
        session.export_checkpoint().unwrap()
    });

    let restored = with_current_dir(other_workspace.path(), || {
        TaskSession::restore_checkpoint(&bytes)
    });

    assert!(restored.is_err());
}

#[test]
fn task_session_checkpoint_restore_rejects_a_replaced_workspace_at_the_same_path() {
    let parent = TempDir::new();
    let workspace_path = parent.path().join("workspace");
    let replacement_path = parent.path().join("replacement");
    let moved_path = parent.path().join("moved-workspace");
    fs::create_dir(&workspace_path).unwrap();
    fs::create_dir(&replacement_path).unwrap();
    let bytes = with_current_dir(&workspace_path, || {
        let mut session = TaskSession::new();
        session.create_task("workspace identity task");
        session.export_checkpoint().unwrap()
    });

    fs::rename(&workspace_path, &moved_path).unwrap();
    fs::rename(&replacement_path, &workspace_path).unwrap();
    let restored = with_current_dir(&workspace_path, || TaskSession::restore_checkpoint(&bytes));

    assert!(matches!(
        restored,
        Err(TaskSessionCheckpointError::WorkspaceMismatch { .. })
    ));
}

#[test]
fn task_session_checkpoint_export_rejects_replaced_workspace_identity_at_the_same_path() {
    let parent = TempDir::new();
    let workspace_path = parent.path().join("workspace");
    let replacement_path = parent.path().join("replacement");
    let moved_path = parent.path().join("moved-workspace");
    fs::create_dir(&workspace_path).unwrap();
    fs::create_dir(&replacement_path).unwrap();

    let mut session = TaskSession::new();
    with_current_dir(&workspace_path, || {
        session.create_task("original workspace identity");
    });

    fs::rename(&workspace_path, &moved_path).unwrap();
    fs::rename(&replacement_path, &workspace_path).unwrap();
    with_current_dir(&workspace_path, || {
        session.create_task("replacement workspace identity");
    });

    assert!(matches!(
        session.export_checkpoint(),
        Err(TaskSessionCheckpointError::WorkspaceMismatch { .. })
    ));
}

#[test]
fn task_session_checkpoint_export_rejects_tasks_bound_to_different_workspaces() {
    let first_workspace = TempDir::new();
    let second_workspace = TempDir::new();
    let mut session = TaskSession::new();
    with_current_dir(first_workspace.path(), || {
        session.create_task("first workspace task");
    });
    with_current_dir(second_workspace.path(), || {
        session.create_task("second workspace task");
    });

    assert!(session.export_checkpoint().is_err());
}

#[test]
fn restoring_a_checkpoint_does_not_repeat_workspace_file_creation() {
    let workspace = TempDir::new();
    let payload = "checkpoint-create-payload-sentinel";
    with_current_dir(workspace.path(), || {
        let mut session = TaskSession::new();
        let (task_id, turn_id) = running_turn(&mut session);
        session
            .create_workspace_file(task_id, turn_id, Path::new("created.txt"), payload)
            .unwrap();
        session
            .complete_turn(
                task_id,
                turn_id,
                TurnOutcome::Success {
                    result: "created once".to_owned(),
                },
            )
            .unwrap();

        let bytes = session.export_checkpoint().unwrap();
        assert!(!bytes
            .windows(payload.len())
            .any(|window| window == payload.as_bytes()));
        let restored = TaskSession::restore_checkpoint(&bytes).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("created.txt")).unwrap(),
            payload
        );
        assert_eq!(restored.list_tasks().len(), 1);
    });
}
