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
    TurnStarted {
        task_id: String,
        turn_id: String,
    },
    ItemStarted {
        item_id: String,
        kind: ItemKind,
    },
    TextDelta {
        item_id: String,
        text: String,
    },
    ToolOutputDelta {
        item_id: String,
        text: String,
    },
    ItemCompleted {
        item_id: String,
    },
    TurnCompleted {
        task_id: String,
        turn_id: String,
        status: String,
    },
    StreamError {
        code: String,
    },
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
        Self {
            sink,
            next_sequence: 1,
        }
    }

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

    pub fn into_inner(self) -> S {
        self.sink
    }
}

/// A complete server-sent event frame, without provider-specific interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental, bounded SSE framing. Does not execute or authorize model actions.
pub struct SseDecoder {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    limit: usize,
    frame_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseDecodeError {
    FrameTooLarge,
    InvalidUtf8,
}

impl SseDecoder {
    pub fn new(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            event: None,
            data: Vec::new(),
            limit,
            frame_bytes: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, SseDecodeError> {
        let mut frames = Vec::new();
        for &byte in chunk {
            if self.frame_bytes >= self.limit {
                self.pending.clear();
                self.data.clear();
                self.event = None;
                self.frame_bytes = 0;
                return Err(SseDecodeError::FrameTooLarge);
            }
            self.frame_bytes += 1;
            self.pending.push(byte);
            if byte != b'\n' {
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                self.frame_bytes = 0;
                if !self.data.is_empty() {
                    frames.push(SseFrame {
                        event: self.event.take(),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                } else {
                    self.event = None;
                }
                continue;
            }
            if line.first() == Some(&b':') {
                continue;
            }
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(_) => {
                    self.data.clear();
                    self.event = None;
                    self.frame_bytes = 0;
                    return Err(SseDecodeError::InvalidUtf8);
                }
            };
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        Ok(frames)
    }
}

/// Read SSE frames directly from a streaming HTTP response body or any reader.
///
/// Frames are delivered immediately as they arrive; the entire response is
/// never buffered. The callback's error stops consumption (for cancellation,
/// sink backpressure, or a disconnected consumer). Callers must validate
/// provider-specific event payloads before using them for model decisions.
pub fn consume_sse<R, F, E>(
    reader: &mut R,
    max_frame_bytes: usize,
    mut on_frame: F,
) -> Result<(), SseReadError<E>>
where
    R: std::io::Read + ?Sized,
    F: FnMut(SseFrame) -> Result<(), E>,
{
    let mut decoder = SseDecoder::new(max_frame_bytes);
    let mut buffer = [0u8; 4096];
    loop {
        let count = reader.read(&mut buffer).map_err(SseReadError::Io)?;
        if count == 0 {
            return Ok(());
        }
        for frame in decoder
            .push(&buffer[..count])
            .map_err(SseReadError::Decode)?
        {
            on_frame(frame).map_err(SseReadError::Consumer)?;
        }
    }
}

#[derive(Debug)]
pub enum SseReadError<E> {
    Io(std::io::Error),
    Decode(SseDecodeError),
    Consumer(E),
}

/// A provider-neutral text delta extracted from a recognized SSE event.
/// Tool-call argument deltas are deliberately excluded: they must be
/// assembled and validated by the provider's existing action parser.
pub fn provider_text_delta(frame: &SseFrame) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&frame.data).ok()?;
    match frame.event.as_deref() {
        Some("response.output_text.delta") => value
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Some("content_block_delta") => {
            let delta = value.get("delta")?;
            if delta.get("type")?.as_str()? != "text_delta" {
                return None;
            }
            delta
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }
        _ => None,
    }
}

/// Bounded, provider-native argument fragments. This is data only: callers must
/// validate the completed JSON and authorize any resulting action separately.
pub struct ResponsesArguments {
    output_index: Option<u64>,
    bytes: String,
    max_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArgumentsError {
    InvalidEvent,
    UnexpectedIndex,
    TooLarge,
    InvalidJson,
}

impl ResponsesArguments {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            output_index: None,
            bytes: String::new(),
            max_bytes,
        }
    }

    /// Accept only Responses function-call argument deltas for one output item.
    pub fn push(&mut self, frame: &SseFrame) -> Result<bool, ArgumentsError> {
        if frame.event.as_deref() != Some("response.function_call_arguments.delta") {
            return Ok(false);
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| ArgumentsError::InvalidEvent)?;
        let index = value
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ArgumentsError::InvalidEvent)?;
        let delta = value
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .ok_or(ArgumentsError::InvalidEvent)?;
        if self.output_index.is_some_and(|existing| existing != index) {
            return Err(ArgumentsError::UnexpectedIndex);
        }
        if delta.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            return Err(ArgumentsError::TooLarge);
        }
        self.output_index = Some(index);
        self.bytes.push_str(delta);
        Ok(true)
    }

    /// Parse only after a provider completion event; parsing never executes tools.
    pub fn parse_completed(&self) -> Result<serde_json::Value, ArgumentsError> {
        if self.output_index.is_none() {
            return Err(ArgumentsError::InvalidEvent);
        }
        serde_json::from_str(&self.bytes).map_err(|_| ArgumentsError::InvalidJson)
    }
}

/// Collect interleaved Responses function-call argument fragments by output index.
/// This is a bounded data buffer, never a tool executor.
pub struct ResponsesCallBatch {
    calls: std::collections::BTreeMap<u64, ResponsesArguments>,
    max_calls: usize,
    max_bytes_per_call: usize,
    completed: std::collections::BTreeSet<u64>,
    call_ids: std::collections::BTreeMap<u64, String>,
}

impl ResponsesCallBatch {
    pub fn new(max_calls: usize, max_bytes_per_call: usize) -> Self {
        Self {
            calls: std::collections::BTreeMap::new(),
            max_calls,
            max_bytes_per_call,
            completed: std::collections::BTreeSet::new(),
            call_ids: std::collections::BTreeMap::new(),
        }
    }

    pub fn push(&mut self, frame: &SseFrame) -> Result<bool, ArgumentsError> {
        if frame.event.as_deref() != Some("response.function_call_arguments.delta") {
            return Ok(false);
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| ArgumentsError::InvalidEvent)?;
        let index = value
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ArgumentsError::InvalidEvent)?;
        if !self.calls.contains_key(&index) && self.calls.len() >= self.max_calls {
            return Err(ArgumentsError::TooLarge);
        }
        if self.completed.contains(&index) {
            return Err(ArgumentsError::InvalidEvent);
        }
        self.calls
            .entry(index)
            .or_insert_with(|| ResponsesArguments::new(self.max_bytes_per_call))
            .push(frame)
    }

    /// Correlate a provider function-call output item with its stable call ID.
    /// Reject mismatched IDs and unknown output indexes.
    pub fn register_call(&mut self, frame: &SseFrame) -> Result<bool, ArgumentsError> {
        if frame.event.as_deref() != Some("response.output_item.added") {
            return Ok(false);
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| ArgumentsError::InvalidEvent)?;
        let item = value.get("item").ok_or(ArgumentsError::InvalidEvent)?;
        if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
            return Ok(false);
        }
        let index = value
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ArgumentsError::InvalidEvent)?;
        let call_id = item
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(ArgumentsError::InvalidEvent)?;
        if self.call_ids.values().any(|existing| existing == call_id) {
            return Err(ArgumentsError::InvalidEvent);
        }
        if self.call_ids.contains_key(&index) {
            return Err(ArgumentsError::UnexpectedIndex);
        }
        if self.call_ids.len() >= self.max_calls {
            return Err(ArgumentsError::TooLarge);
        }
        self.call_ids.insert(index, call_id.to_owned());
        Ok(true)
    }

    /// Mark a known output item complete only when the provider sends its
    /// function-call arguments completion event. Does not execute the call.
    pub fn complete(&mut self, frame: &SseFrame) -> Result<bool, ArgumentsError> {
        if frame.event.as_deref() != Some("response.function_call_arguments.done") {
            return Ok(false);
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|_| ArgumentsError::InvalidEvent)?;
        let index = value
            .get("output_index")
            .and_then(serde_json::Value::as_u64)
            .ok_or(ArgumentsError::InvalidEvent)?;
        let arguments = self
            .calls
            .get(&index)
            .ok_or(ArgumentsError::UnexpectedIndex)?;
        if self.completed.contains(&index) {
            return Err(ArgumentsError::InvalidEvent);
        }
        arguments.parse_completed()?;
        self.completed.insert(index);
        Ok(true)
    }

    pub fn completed_arguments(&self) -> Result<Vec<(u64, serde_json::Value)>, ArgumentsError> {
        self.completed
            .iter()
            .map(|index| {
                let arguments = self
                    .calls
                    .get(index)
                    .ok_or(ArgumentsError::UnexpectedIndex)?;
                Ok((*index, arguments.parse_completed()?))
            })
            .collect()
    }

    /// Parse buffered arguments; a caller must separately verify completion,
    /// correlate call IDs and authorize the resulting action.
    pub fn parsed(&self) -> Result<Vec<(u64, serde_json::Value)>, ArgumentsError> {
        self.calls
            .iter()
            .map(|(index, arguments)| Ok((*index, arguments.parse_completed()?)))
            .collect()
    }
}

/// Project a recognized provider SSE text delta into the caller's event stream.
/// Unknown events (including tool argument deltas) are ignored. Sink failures
/// propagate to the caller so it can stop reading instead of buffering forever.
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
        serde_json::to_writer(&mut self.writer, event).map_err(std::io::Error::other)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }
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
        assert_eq!(
            emitter.emit(StreamEvent::ItemStarted {
                item_id: "i".into(),
                kind: ItemKind::Message
            }),
            Ok(1)
        );
        assert_eq!(
            emitter.emit(StreamEvent::TextDelta {
                item_id: "i".into(),
                text: "hello".into()
            }),
            Ok(2)
        );
        let sink = emitter.into_inner();
        assert_eq!(sink.0.len(), 2);
        let json = serde_json::to_string(&sink.0[1]).unwrap();
        assert_eq!(
            serde_json::from_str::<StreamEnvelope>(&json).unwrap(),
            sink.0[1]
        );
    }

    #[test]
    fn jsonl_sink_emits_one_parseable_line_per_event() {
        let mut emitter = StreamEmitter::new(JsonlSink::new(Vec::<u8>::new()));
        emitter
            .emit(StreamEvent::ItemStarted {
                item_id: "message".into(),
                kind: ItemKind::Message,
            })
            .unwrap();
        emitter
            .emit(StreamEvent::ItemCompleted {
                item_id: "message".into(),
            })
            .unwrap();
        let bytes = emitter.into_inner().into_inner();
        let lines: Vec<_> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        let first: StreamEnvelope = serde_json::from_slice(lines[0]).unwrap();
        let second: StreamEnvelope = serde_json::from_slice(lines[1]).unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
    }

    #[test]
    fn sse_decoder_handles_split_crlf_and_multiple_data_lines() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\r\n")
            .unwrap()
            .is_empty());
        assert!(decoder.push(b"data: one\r\ndata: tw").unwrap().is_empty());
        let frames = decoder.push(b"o\r\n\r\n").unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("response.output_text.delta".into()),
                data: "one\ntwo".into(),
            }]
        );
    }

    #[test]
    fn sse_decoder_rejects_oversized_incomplete_frame() {
        let mut decoder = SseDecoder::new(4);
        assert_eq!(decoder.push(b"12345"), Err(SseDecodeError::FrameTooLarge));
    }

    #[test]
    fn sse_reader_delivers_frames_without_waiting_for_eof() {
        let mut reader = std::io::Cursor::new(b"data: first\n\ndata: second\n\n".to_vec());
        let mut seen = Vec::new();
        let result = consume_sse(&mut reader, 1024, |frame| {
            seen.push(frame.data);
            Ok::<(), ()>(())
        });
        assert!(result.is_ok());
        assert_eq!(seen, vec!["first", "second"]);
    }

    #[test]
    fn sse_reader_propagates_consumer_cancellation() {
        let mut reader = std::io::Cursor::new(b"data: first\n\ndata: second\n\n".to_vec());
        let result = consume_sse(&mut reader, 1024, |_| Err("cancelled"));
        assert!(matches!(result, Err(SseReadError::Consumer("cancelled"))));
    }

    #[test]
    fn provider_text_delta_accepts_only_known_text_events() {
        let openai = SseFrame {
            event: Some("response.output_text.delta".into()),
            data: r#"{"delta":"hello"}"#.into(),
        };
        assert_eq!(provider_text_delta(&openai).as_deref(), Some("hello"));
        let anthropic = SseFrame {
            event: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"text_delta","text":"world"}}"#.into(),
        };
        assert_eq!(provider_text_delta(&anthropic).as_deref(), Some("world"));
        let tool_args = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: r#"{"delta":"secret"}"#.into(),
        };
        assert_eq!(provider_text_delta(&tool_args), None);
    }

    #[test]
    fn provider_delta_bridge_emits_only_text_and_preserves_sequence() {
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        let tool_args = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: r#"{"delta":"do not emit"}"#.into(),
        };
        assert_eq!(
            emit_provider_text_delta(&mut emitter, "answer", &tool_args),
            Ok(false)
        );
        let text = SseFrame {
            event: Some("response.output_text.delta".into()),
            data: r#"{"delta":"hello"}"#.into(),
        };
        assert_eq!(
            emit_provider_text_delta(&mut emitter, "answer", &text),
            Ok(true)
        );
        let sink = emitter.into_inner();
        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0].sequence, 1);
        assert_eq!(
            sink.0[0].event,
            StreamEvent::TextDelta {
                item_id: "answer".into(),
                text: "hello".into()
            }
        );
    }

    #[test]
    fn anthropic_text_delta_bridge_ignores_non_text_chunks() {
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        let tool_chunk = SseFrame {
            event: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"input_json_delta","partial_json":"secret"}}"#.into(),
        };
        assert_eq!(
            emit_provider_text_delta(&mut emitter, "message", &tool_chunk),
            Ok(false)
        );
        let text_chunk = SseFrame {
            event: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"text_delta","text":"hello"}}"#.into(),
        };
        assert_eq!(
            emit_provider_text_delta(&mut emitter, "message", &text_chunk),
            Ok(true)
        );
        let sink = emitter.into_inner();
        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0].sequence, 1);
        assert_eq!(
            sink.0[0].event,
            StreamEvent::TextDelta {
                item_id: "message".into(),
                text: "hello".into()
            }
        );
    }

    #[test]
    fn responses_batch_correlates_call_ids_and_rejects_duplicates() {
        let mut batch = ResponsesCallBatch::new(2, 64);
        let first = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&first), Ok(true));
        let duplicate = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_a"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&duplicate), Err(ArgumentsError::InvalidEvent));
        assert_eq!(batch.register_call(&first), Err(ArgumentsError::InvalidEvent));
    }

    #[test]
    fn responses_batch_requires_done_before_exposing_completed_arguments() {
        let mut batch = ResponsesCallBatch::new(2, 64);
        let delta = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
        };
        assert_eq!(batch.push(&delta), Ok(true));
        assert_eq!(batch.completed_arguments(), Ok(vec![]));
        let done = SseFrame {
            event: Some("response.function_call_arguments.done".into()),
            data: serde_json::json!({"output_index": 0}).to_string(),
        };
        assert_eq!(batch.complete(&done), Ok(true));
        assert_eq!(
            batch.completed_arguments(),
            Ok(vec![(0, serde_json::json!({}))])
        );
        assert_eq!(batch.complete(&done), Err(ArgumentsError::InvalidEvent));
        assert_eq!(batch.push(&delta), Err(ArgumentsError::InvalidEvent));
    }

    #[test]
    fn responses_batch_keeps_interleaved_calls_separate() {
        let mut batch = ResponsesCallBatch::new(2, 64);
        for (index, delta) in [(1, "{"), (0, "{}"), (1, "}")] {
            let frame = SseFrame {
                event: Some("response.function_call_arguments.delta".into()),
                data: serde_json::json!({"output_index": index, "delta": delta}).to_string(),
            };
            assert_eq!(batch.push(&frame), Ok(true));
        }
        assert_eq!(
            batch.parsed(),
            Ok(vec![(0, serde_json::json!({})), (1, serde_json::json!({}))])
        );
        let extra = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 2, "delta": "{}"}).to_string(),
        };
        assert_eq!(batch.push(&extra), Err(ArgumentsError::TooLarge));
    }

    #[test]
    fn responses_arguments_collect_fragments_without_executing() {
        let mut collector = ResponsesArguments::new(64);
        for (index, delta) in [(0, r#"{"path":"#), (0, r#""src"}"#)] {
            let frame = SseFrame {
                event: Some("response.function_call_arguments.delta".into()),
                data: serde_json::json!({"output_index": index, "delta": delta}).to_string(),
            };
            assert_eq!(collector.push(&frame), Ok(true));
        }
        assert_eq!(
            collector.parse_completed(),
            Ok(serde_json::json!({"path": "src"}))
        );
        let wrong = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 1, "delta": "{}"}).to_string(),
        };
        assert_eq!(collector.push(&wrong), Err(ArgumentsError::UnexpectedIndex));
    }

    #[test]
    fn responses_arguments_reject_oversize_and_incomplete_json() {
        let mut collector = ResponsesArguments::new(4);
        let fragment = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{"}).to_string(),
        };
        assert_eq!(collector.push(&fragment), Ok(true));
        assert_eq!(
            collector.parse_completed(),
            Err(ArgumentsError::InvalidJson)
        );
        let oversize = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "12345"}).to_string(),
        };
        assert_eq!(collector.push(&oversize), Err(ArgumentsError::TooLarge));
    }

    #[test]
    fn fragmented_provider_frames_keep_text_order_and_exclude_tool_arguments() {
        let payload = concat!(
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hel\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"delta\":\"private args\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"lo\"}\n\n"
        );
        let mut decoder = SseDecoder::new(1024);
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        for chunk in payload.as_bytes().chunks(3) {
            for frame in decoder.push(chunk).expect("decode fragmented SSE") {
                emit_provider_text_delta(&mut emitter, "message", &frame)
                    .expect("emit recognized text");
            }
        }
        let events = emitter.into_inner().0;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
        assert_eq!(
            events[0].event,
            StreamEvent::TextDelta {
                item_id: "message".into(),
                text: "hel".into()
            }
        );
        assert_eq!(
            events[1].event,
            StreamEvent::TextDelta {
                item_id: "message".into(),
                text: "lo".into()
            }
        );
    }

    #[test]
    fn sse_decoder_limits_each_frame_not_the_entire_read_chunk() {
        let mut decoder = SseDecoder::new(9);
        let frames = decoder.push(b"data: a\n\ndata: b\n\n").unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "a");
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn sse_decoder_resets_after_invalid_utf8() {
        let mut decoder = SseDecoder::new(32);
        assert_eq!(
            decoder.push(b"data: \xff\n"),
            Err(SseDecodeError::InvalidUtf8)
        );
        let frames = decoder.push(b"data: ok\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
    }

    struct FailingSink;
    impl EventSink for FailingSink {
        type Error = &'static str;
        fn send(&mut self, _: &StreamEnvelope) -> Result<(), Self::Error> {
            Err("disconnected")
        }
    }

    #[test]
    fn failed_delivery_is_not_acknowledged() {
        let mut emitter = StreamEmitter::new(FailingSink);
        assert_eq!(
            emitter.emit(StreamEvent::StreamError {
                code: "failed".into()
            }),
            Err(EmitError::Sink("disconnected"))
        );
    }
}
