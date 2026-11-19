use serde::{Deserialize, Serialize};

use super::emitter::{EmitError, EventSink, StreamEmitter};
use super::events::StreamEvent;

/// Read-only projection of the authoritative TaskSession snapshot.
///
/// Lifecycle and activity positions are independent. This function does not
/// synthesize a total ordering or copy any workspace or model payloads.
pub fn snapshot_positions(session: &crate::task_lifecycle::TaskSession) -> Vec<ActivityPosition> {
    session
        .snapshot()
        .tasks()
        .iter()
        .map(|task| {
            ActivityPosition::new(
                task.id().to_string(),
                task.lifecycle_sequence_high_water(),
                task.activity_sequence_high_water(),
            )
        })
        .collect()
}

/// Projects existing metadata-only task positions into versioned stream events.
///
/// # Invariants
/// - Only reads the authoritative TaskSession snapshot.
/// - Lifecycle and activity high-water marks are independent.
/// - Does not emit goals, prompts, tool arguments, paths, or secrets.
/// - Sink failure stops projection without changing task or checkpoint state.
/// - Stream-local sequence numbers are not history cursors or replay tokens.
///
/// # Errors
/// Propagates sequence exhaustion or sink delivery failure.
pub fn emit_activity_positions<S: EventSink>(
    session: &crate::task_lifecycle::TaskSession,
    emitter: &mut StreamEmitter<S>,
) -> Result<(), EmitError<S::Error>> {
    for position in snapshot_positions(session) {
        emitter.emit(StreamEvent::ActivityPositionUpdated {
            task_id: position.task_id,
            lifecycle_position: position.lifecycle_position,
            activity_position: position.activity_position,
        })?;
    }
    Ok(())
}

/// Emits only task positions that differ from a previously observed snapshot.
///
/// This is a read-only *snapshot comparison*, not history replay. It does not
/// infer missing events, combine independent histories, or create a cursor.
/// Consumers must retain their own prior positions; no runtime state is stored.
///
/// A task absent from `previous` is emitted even when both positions are zero.
/// A changed position is emitted with the existing stream-local sequence.
/// The caller must not advance its observed snapshot after a sink failure.
///
/// # Errors
/// Propagates sink failures or sequence exhaustion without mutating TaskSession.
pub fn emit_changed_activity_positions<S: EventSink>(
    session: &crate::task_lifecycle::TaskSession,
    previous: &[ActivityPosition],
    emitter: &mut StreamEmitter<S>,
) -> Result<(), EmitError<S::Error>> {
    for current in snapshot_positions(session) {
        let unchanged = previous.iter().any(|old| {
            old.task_id == current.task_id
                && old.lifecycle_position == current.lifecycle_position
                && old.activity_position == current.activity_position
        });
        if !unchanged {
            emitter.emit(StreamEvent::ActivityPositionUpdated {
                task_id: current.task_id,
                lifecycle_position: current.lifecycle_position,
                activity_position: current.activity_position,
            })?;
        }
    }
    Ok(())
}

/// A metadata-only projection from an already-authoritative runtime record.
/// Deliberately excludes any user text, tool arguments and workspace data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPosition {
    pub task_id: String,
    pub lifecycle_position: u64,
    pub activity_position: u64,
}

impl ActivityPosition {
    pub fn new(task_id: String, lifecycle_position: u64, activity_position: u64) -> Self {
        Self {
            task_id,
            lifecycle_position,
            activity_position,
        }
    }
}
