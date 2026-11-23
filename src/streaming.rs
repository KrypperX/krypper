//! Opt-in, transport-neutral streaming envelopes.
//!
//! This module does not own task state or grant tool permissions. Consumers
//! project existing runtime observations into events and deliver them using
//! their own transport. No event is persisted by this module.

mod anthropic;
mod emitter;
mod events;
mod projection;
mod responses;
mod sink;
mod sse;

pub(crate) use anthropic::{AnthropicAssemblyError, AnthropicFinalActions};
pub use emitter::{EmitError, EventSink, StreamEmitter};
pub use events::{ItemKind, StreamEnvelope, StreamEvent};
pub use projection::{
    emit_activity_positions, emit_changed_activity_positions, snapshot_positions, ActivityPosition,
};
pub use responses::{
    emit_provider_text_delta, ArgumentsError, ResponsesArguments, ResponsesCallBatch,
    ResponsesFinalActions,
};
pub use sink::{write_activity_positions, JsonlSink, TextSink};
pub use sse::{
    consume_sse, provider_text_delta, SseDecodeError, SseDecoder, SseFrame, SseReadError,
};

#[cfg(test)]
mod tests;
