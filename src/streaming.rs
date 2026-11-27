//! Opt-in, transport-neutral streaming envelopes.
//!
//! This module does not own task state or grant tool permissions. Consumers
//! project existing runtime observations into events and deliver them using
//! their own transport. No event is persisted by this module.

mod emitter;
mod events;
mod projection;
mod provider;
mod sink;

pub use crate::brain::provider_streaming::{
    provider_text_delta, ArgumentsError, ResponsesArguments, ResponsesCallBatch,
    ResponsesFinalActions,
};
pub use crate::contracts::sse::{consume_sse, SseDecodeError, SseDecoder, SseFrame, SseReadError};
pub use emitter::{EmitError, EventSink, StreamEmitter};
pub use events::{ItemKind, StreamEnvelope, StreamEvent};
pub use projection::{
    emit_activity_positions, emit_changed_activity_positions, snapshot_positions, ActivityPosition,
};
pub use provider::emit_provider_text_delta;
pub use sink::{write_activity_positions, JsonlSink, TextSink};

#[cfg(test)]
mod tests;
