use super::emitter::EventSink;
use super::events::{StreamEnvelope, StreamEvent};
use super::projection::snapshot_positions;

/// A newline-delimited JSON transport for a caller-owned writer.
///
/// The writer is not shared with the runtime. A failed write is surfaced to
/// the caller and does not advance the emitter's sequence counter.
pub struct JsonlSink<W: std::io::Write> {
    writer: W,
}

impl<W: std::io::Write> JsonlSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: std::io::Write> EventSink for JsonlSink<W> {
    type Error = std::io::Error;

    fn send(&mut self, event: &StreamEnvelope) -> Result<(), Self::Error> {
        // Encode a full record before writing; a serialization failure must not
        // leave a partial JSON value in the destination.
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        self.writer.flush()
    }
}

/// Render the same versioned event protocol as human-readable CLI output.
/// This is a presentation-only adapter; it cannot mutate TaskSession or run tools.
pub struct TextSink<W: std::io::Write> {
    writer: W,
}

impl<W: std::io::Write> TextSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: std::io::Write> EventSink for TextSink<W> {
    type Error = std::io::Error;

    fn send(&mut self, envelope: &StreamEnvelope) -> Result<(), Self::Error> {
        match &envelope.event {
            StreamEvent::TextDelta { text, .. } | StreamEvent::ToolOutputDelta { text, .. } => {
                self.writer.write_all(text.as_bytes())?;
            }
            StreamEvent::TurnCompleted { status, .. } => {
                writeln!(self.writer, "\n[turn: {status}]")?;
            }
            StreamEvent::StreamError { code } => {
                writeln!(self.writer, "\n[stream error: {code}]")?;
            }
            _ => {}
        }
        self.writer.flush()
    }
}

/// Emit a read-only TaskSession position as a stable, versioned JSONL line.
/// This deliberately carries no prompts, workspace paths, or tool arguments.
pub fn write_activity_positions<W: std::io::Write + ?Sized>(
    session: &crate::task_lifecycle::TaskSession,
    writer: &mut W,
) -> std::io::Result<()> {
    for position in snapshot_positions(session) {
        serde_json::to_writer(&mut *writer, &position).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}
