use super::events::{StreamEnvelope, StreamEvent};

/// Synchronous, fallible sink. The producer decides how to respond to
/// cancellation, disconnection, and backpressure; this sink never runs tools.
pub trait EventSink {
    type Error;
    fn send(&mut self, event: &StreamEnvelope) -> Result<(), Self::Error>;
}

/// Assigns stream-local sequence numbers only after a successful sink write.
///
/// # Invariants
/// - The first successfully delivered envelope has sequence 1.
/// - A rejected delivery does not consume its sequence number; a subsequent
///   successful delivery may reuse that number.
/// - A failed writer can still have partially emitted bytes, so this is not
///   an atomic-delivery, replay, or exactly-once guarantee.
/// - Sequence overflow fails closed rather than wrapping.
///
/// These numbers are independent of TaskSession lifecycle/activity positions.
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
        Self {
            sink,
            next_sequence: 1,
        }
    }

    /// Delivers an event and returns its sequence number on success.
    ///
    /// # Errors
    /// Returns `SequenceExhausted` before writing when no next sequence can
    /// be reserved, or `Sink` when the downstream consumer rejects delivery.
    /// In either case the next sequence remains unchanged.
    pub fn emit(&mut self, event: StreamEvent) -> Result<u64, EmitError<S::Error>> {
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(EmitError::SequenceExhausted)?;
        let sequence = self.next_sequence;
        self.sink
            .send(&StreamEnvelope {
                version: 1,
                sequence,
                event,
            })
            .map_err(EmitError::Sink)?;
        self.next_sequence = next;
        Ok(sequence)
    }

    /// Returns the underlying sink without performing another write.
    pub fn into_inner(self) -> S {
        self.sink
    }
}

#[cfg(test)]
impl<S> StreamEmitter<S> {
    pub(super) fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}
