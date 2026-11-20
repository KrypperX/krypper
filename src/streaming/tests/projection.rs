use super::super::*;
use super::*;

#[test]
fn activity_projection_reuses_snapshot_positions_without_mutating_session() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    session.create_task("private goal never emitted");
    session.create_task("another private goal");
    let before = session.snapshot();
    let positions = snapshot_positions(&session);
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    emit_activity_positions(&session, &mut emitter).unwrap();
    let events = emitter.into_inner().0;
    assert_eq!(events.len(), positions.len());
    for (index, (event, position)) in events.iter().zip(positions.iter()).enumerate() {
        assert_eq!(event.version, 1);
        assert_eq!(event.sequence, index as u64 + 1);
        assert_eq!(
            event.event,
            StreamEvent::ActivityPositionUpdated {
                task_id: position.task_id.clone(),
                lifecycle_position: position.lifecycle_position,
                activity_position: position.activity_position,
            }
        );
        let serialized = serde_json::to_string(event).unwrap();
        assert!(!serialized.contains("private goal"));
    }
    assert_eq!(session.snapshot(), before);
}

#[test]
fn activity_projection_stops_on_sink_failure_without_mutating_session() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    session.create_task("private goal");
    let before = session.snapshot();
    let mut emitter = StreamEmitter::new(FailingSink);
    assert_eq!(
        emit_activity_positions(&session, &mut emitter),
        Err(EmitError::Sink("disconnected"))
    );
    assert_eq!(session.snapshot(), before);
}

#[test]
fn unchanged_snapshot_emits_no_duplicate_position_events() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    session.create_task("private");
    let positions = snapshot_positions(&session);
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    emit_changed_activity_positions(&session, &positions, &mut emitter).unwrap();
    assert!(emitter.into_inner().0.is_empty());
}

#[test]
fn new_task_position_is_emitted_even_with_zero_high_water_marks() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    let previous = snapshot_positions(&session);
    session.create_task("private");
    let before = session.snapshot();
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    emit_changed_activity_positions(&session, &previous, &mut emitter).unwrap();
    let events = emitter.into_inner().0;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(session.snapshot(), before);
    assert!(!serde_json::to_string(&events).unwrap().contains("private"));
}

#[test]
fn changed_projection_failure_does_not_mutate_authoritative_snapshot() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    session.create_task("private");
    let before = session.snapshot();
    let mut emitter = StreamEmitter::new(FailingSink);
    assert_eq!(
        emit_changed_activity_positions(&session, &[], &mut emitter),
        Err(EmitError::Sink("disconnected"))
    );
    assert_eq!(session.snapshot(), before);
}
