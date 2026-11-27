use crate::brain::provider_streaming::provider_text_delta;
use crate::contracts::sse::SseFrame;

use super::emitter::{EmitError, EventSink, StreamEmitter};
use super::events::StreamEvent;

/// Compatibility adapter from provider frames to presentation text events.
/// Unknown events, including tool argument deltas, are ignored.
pub fn emit_provider_text_delta<S: EventSink>(
    emitter: &mut StreamEmitter<S>,
    item_id: &str,
    frame: &SseFrame,
) -> Result<bool, EmitError<S::Error>> {
    let Some(text) = provider_text_delta(frame) else {
        return Ok(false);
    };
    emitter.emit(StreamEvent::TextDelta {
        item_id: item_id.to_owned(),
        text,
    })?;
    Ok(true)
}
