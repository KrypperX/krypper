use krypper::{
    core::{lifecycle as core_lifecycle, task as core_task},
    task_lifecycle as compatibility,
};

#[test]
fn core_task_preserves_public_type_identity_and_transitions() {
    fn same_type<T: 'static, U: 'static>() {
        assert_eq!(std::any::TypeId::of::<T>(), std::any::TypeId::of::<U>());
    }

    same_type::<core_task::Task, core_lifecycle::Task>();
    same_type::<core_task::Task, compatibility::Task>();
    same_type::<core_task::Turn, core_lifecycle::Turn>();
    same_type::<core_task::Turn, compatibility::Turn>();
    same_type::<core_task::TaskState, core_lifecycle::TaskState>();
    same_type::<core_task::TaskState, compatibility::TaskState>();
    same_type::<core_task::TurnState, core_lifecycle::TurnState>();
    same_type::<core_task::TurnState, compatibility::TurnState>();
    same_type::<core_task::TurnOutcome, core_lifecycle::TurnOutcome>();
    same_type::<core_task::TurnOutcome, compatibility::TurnOutcome>();
    same_type::<core_task::TaskEvent, core_lifecycle::TaskEvent>();
    same_type::<core_task::TaskEvent, compatibility::TaskEvent>();
    same_type::<core_task::TaskHistoryEvent, core_lifecycle::TaskHistoryEvent>();
    same_type::<core_task::TaskHistoryEvent, compatibility::TaskHistoryEvent>();
    same_type::<core_task::TaskHistoryTransition, core_lifecycle::TaskHistoryTransition>();
    same_type::<core_task::TaskHistoryTransition, compatibility::TaskHistoryTransition>();
    same_type::<core_task::TaskActivityEvent, core_lifecycle::TaskActivityEvent>();
    same_type::<core_task::TaskActivityEvent, compatibility::TaskActivityEvent>();
    same_type::<core_task::TransitionError, core_lifecycle::TransitionError>();
    same_type::<core_task::TransitionError, compatibility::TransitionError>();
    same_type::<core_task::TurnCreationError, core_lifecycle::TurnCreationError>();
    same_type::<core_task::TurnCreationError, compatibility::TurnCreationError>();
    same_type::<core_task::TurnEvent, core_lifecycle::TurnEvent>();
    same_type::<core_task::TurnEvent, compatibility::TurnEvent>();
    same_type::<core_task::TurnActivityAction, core_lifecycle::TurnActivityAction>();
    same_type::<core_task::TurnActivityAction, compatibility::TurnActivityAction>();
    same_type::<core_task::TurnActivitySummary, core_lifecycle::TurnActivitySummary>();
    same_type::<core_task::TurnActivitySummary, compatibility::TurnActivitySummary>();
    same_type::<core_task::TurnCompletionError, core_lifecycle::TurnCompletionError>();
    same_type::<core_task::TurnCompletionError, compatibility::TurnCompletionError>();
    same_type::<core_task::TurnTransitionError, core_lifecycle::TurnTransitionError>();
    same_type::<core_task::TurnTransitionError, compatibility::TurnTransitionError>();

    let mut task: core_task::Task = core_lifecycle::Task::new("exercise task module");
    assert_eq!(task.description(), "exercise task module");
    assert_eq!(task.state(), core_task::TaskState::Pending);

    task.transition(core_task::TaskState::Running)
        .expect("task starts");
    let turn_id = task.create_turn().expect("turn is created");
    task.transition_turn(turn_id, core_task::TurnState::Running)
        .expect("turn starts");
    task.complete_turn(
        turn_id,
        core_task::TurnOutcome::Success {
            result: "done".to_owned(),
        },
    )
    .expect("turn completes");
    task.transition(core_task::TaskState::Succeeded)
        .expect("task completes");

    let compatibility_task: &compatibility::Task = &task;
    assert_eq!(
        compatibility_task.state(),
        compatibility::TaskState::Succeeded
    );
    let turn: &core_task::Turn = task.get_turn(turn_id).expect("turn is owned");
    let compatibility_turn: &compatibility::Turn = turn;
    assert_eq!(turn.id(), turn_id);
    assert_eq!(turn.state(), core_task::TurnState::Succeeded);
    assert_eq!(turn.events().len(), 2);
    assert_eq!(
        compatibility_turn.outcome(),
        Some(&compatibility::TurnOutcome::Success {
            result: "done".to_owned(),
        })
    );
}

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
