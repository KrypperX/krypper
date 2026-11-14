use krypper::{core::lifecycle as core_lifecycle, task_lifecycle as compatibility};

#[test]
fn core_lifecycle_preserves_compatibility_types_and_snapshot_behavior() {
    let mut session: compatibility::TaskSession = core_lifecycle::TaskSession::new();
    let task = session.create_task("exercise the relocated lifecycle");
    let task_id = task.id();
    task.transition(core_lifecycle::TaskState::Running)
        .expect("task transition succeeds");
    let turn_id = task.create_turn().expect("turn creation succeeds");
    task.transition_turn(turn_id, core_lifecycle::TurnState::Running)
        .expect("turn transition succeeds");

    let snapshot: core_lifecycle::TaskSessionSnapshot = session.snapshot();
    let compatibility_snapshot: &compatibility::TaskSessionSnapshot = &snapshot;
    let task_snapshot = &compatibility_snapshot.tasks()[0];

    assert_eq!(snapshot.id(), session.id());
    assert_eq!(task_snapshot.id(), task_id);
    assert_eq!(task_snapshot.state(), compatibility::TaskState::Running);
    assert_eq!(task_snapshot.lifecycle_sequence_high_water(), 3);
    assert_eq!(task_snapshot.activity_sequence_high_water(), 0);
    assert_eq!(task_snapshot.turns()[0].id(), turn_id);
    assert_eq!(
        task_snapshot.turns()[0].state(),
        compatibility::TurnState::Running
    );
}

#[test]
fn core_and_compatibility_paths_share_checkpoint_types_and_round_trip() {
    assert_eq!(
        core_lifecycle::MAX_TASK_SESSION_CHECKPOINT_BYTES,
        compatibility::MAX_TASK_SESSION_CHECKPOINT_BYTES
    );

    let mut session = core_lifecycle::TaskSession::new();
    let task = session.create_task("checkpoint the relocated lifecycle");
    let task_id = task.id();
    task.transition(core_lifecycle::TaskState::Running)
        .expect("task starts");
    task.transition(core_lifecycle::TaskState::Succeeded)
        .expect("task completes");

    let checkpoint = session.export_checkpoint().expect("checkpoint exports");
    let restored: core_lifecycle::TaskSession =
        compatibility::TaskSession::restore_checkpoint(&checkpoint)
            .expect("checkpoint restores through compatibility path");
    let snapshot: compatibility::TaskSessionSnapshot = restored.snapshot();

    assert_eq!(snapshot.id(), session.id());
    assert_eq!(snapshot.tasks()[0].id(), task_id);
    assert_eq!(
        snapshot.tasks()[0].state(),
        compatibility::TaskState::Succeeded
    );
    assert_eq!(snapshot.tasks()[0].lifecycle_sequence_high_water(), 2);
    assert_eq!(snapshot.tasks()[0].activity_sequence_high_water(), 0);
}
