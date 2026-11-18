//! Opt-in, transport-neutral streaming envelopes.
//!
//! This module does not own task state or grant tool permissions. Consumers
//! project existing runtime observations into events and deliver them using
//! their own transport. No event is persisted by this module.
use serde::{Deserialize, Serialize};

/// A stream-local monotonic sequence. It is not a TaskSession history position.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEnvelope {
    pub version: u8,
    pub sequence: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

/// The presentation protocol; payloads must be sanitized before emission.
/// Do not include prompts, secrets, raw workspace paths, or tool arguments
/// in metadata events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    TurnStarted { task_id: String, turn_id: String },
    ItemStarted { item_id: String, kind: ItemKind },
    TextDelta { item_id: String, text: String },
    ToolOutputDelta { item_id: String, text: String },
    ItemCompleted { item_id: String },
    TurnCompleted { task_id: String, turn_id: String, status: String },
    StreamError { code: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Message,
    Tool,
}

/// Synchronous, fallible sink. The producer decides how to respond to
/// cancellation, disconnection, and backpressure; this sink never runs tools.
pub trait EventSink {
    type Error;
    fn send(&mut self, event: &StreamEnvelope) -> Result<(), Self::Error>;
}

/// Assigns sequence numbers only after a successful sink write.
/// Sequence overflow is reported instead of silently wrapping.
pub struct StreamEmitter<S> {
    sink: S,
    next_sequence: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EmitError<E> {
    SequenceExhausted,
    Sink(E),
}

impl<S: EventSink> StreamEmitter<S> {
    pub fn new(sink: S) -> Self {
        Self { sink, next_sequence: 1 }
    }

    pub fn emit(&mut self, event: StreamEvent) -> Result<u64, EmitError<S::Error>> {
        let next = self.next_sequence.checked_add(1).ok_or(EmitError::SequenceExhausted)?;
        let sequence = self.next_sequence;
        self.sink.send(&StreamEnvelope { version: 1, sequence, event })
            .map_err(EmitError::Sink)?;
        self.next_sequence = next;
        Ok(sequence)
    }

    pub fn into_inner(self) -> S { self.sink }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink(Vec<StreamEnvelope>);
    impl EventSink for RecordingSink {
        type Error = ();
        fn send(&mut self, event: &StreamEnvelope) -> Result<(), ()> {
            self.0.push(event.clone());
            Ok(())
        }
    }

    #[test]
    fn events_are_ordered_and_json_round_trip() {
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        assert_eq!(emitter.emit(StreamEvent::ItemStarted {
            item_id: "i".into(), kind: ItemKind::Message
        }), Ok(1));
        assert_eq!(emitter.emit(StreamEvent::TextDelta {
            item_id: "i".into(), text: "hello".into()
        }), Ok(2));
        let sink = emitter.into_inner();
        assert_eq!(sink.0.len(), 2);
        let json = serde_json::to_string(&sink.0[1]).unwrap();
        assert_eq!(serde_json::from_str::<StreamEnvelope>(&json).unwrap(), sink.0[1]);
    }

    struct FailingSink;
    impl EventSink for FailingSink {
        type Error = &'static str;
        fn send(&mut self, _: &StreamEnvelope) -> Result<(), Self::Error> { Err("disconnected") }
    }

    #[test]
    fn failed_delivery_is_not_acknowledged() {
        let mut emitter = StreamEmitter::new(FailingSink);
        assert_eq!(emitter.emit(StreamEvent::StreamError { code: "failed".into() }),
                   Err(EmitError::Sink("disconnected")));
    }
}
