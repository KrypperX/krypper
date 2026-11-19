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

struct FailingSink;
impl EventSink for FailingSink {
    type Error = &'static str;
    fn send(&mut self, _: &StreamEnvelope) -> Result<(), Self::Error> {
        Err("disconnected")
    }
}

// Simulates transient backpressure without relying on timing or threads.
// A failed send must not acknowledge an envelope or consume its sequence.
struct FailOnceSink {
    attempts: usize,
    delivered: Vec<StreamEnvelope>,
}

impl EventSink for FailOnceSink {
    type Error = &'static str;

    fn send(&mut self, event: &StreamEnvelope) -> Result<(), Self::Error> {
        self.attempts += 1;
        if self.attempts == 1 {
            return Err("backpressure");
        }
        self.delivered.push(event.clone());
        Ok(())
    }
}

/// Regression: accepted envelopes remain contiguous after a rejected send.
/// This does not assert atomic writes or automatic replay by a real sink.
mod events;
mod projection;
mod provider;
mod responses;
mod sink;
mod sse;
mod sse_reader;
