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
    TruncatedFrame,
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

    /// Reject an EOF in the middle of an SSE frame instead of silently
    /// treating a partial provider event as a successful stream.
    pub fn finish(&self) -> Result<(), SseDecodeError> {
        if self.pending.is_empty() && self.data.is_empty() && self.event.is_none() {
            Ok(())
        } else {
            Err(SseDecodeError::TruncatedFrame)
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
                    self.pending.clear();
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
            return decoder.finish().map_err(SseReadError::Decode);
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

/// Bounded, provider-native argument fragments for one Responses output index.
///
/// This collector only buffers and parses JSON; it does not observe provider
/// completion itself, validate an action schema, or authorize tool execution.
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

    /// Parse buffered JSON without checking provider completion.
    ///
    /// Callers must independently verify completion and authorize any action;
    /// this method is deliberately not an execution boundary.
    pub fn parse_completed(&self) -> Result<serde_json::Value, ArgumentsError> {
        if self.output_index.is_none() {
            return Err(ArgumentsError::InvalidEvent);
        }
        serde_json::from_str(&self.bytes).map_err(|_| ArgumentsError::InvalidJson)
    }
}

/// Collect interleaved Responses function-call fragments by output index.
///
/// Call IDs and completion markers are tracked separately from JSON buffers.
/// Neither a completed buffer nor a registered call ID authorizes execution.
pub struct ResponsesCallBatch {
    calls: std::collections::BTreeMap<u64, ResponsesArguments>,
    max_calls: usize,
    max_bytes_per_call: usize,
    completed: std::collections::BTreeSet<u64>,
    call_ids: std::collections::BTreeMap<u64, String>,
    function_names: std::collections::BTreeMap<u64, String>,
    output_items_done: std::collections::BTreeSet<u64>,
}

impl ResponsesCallBatch {
    pub fn new(max_calls: usize, max_bytes_per_call: usize) -> Self {
        Self {
            calls: std::collections::BTreeMap::new(),
            max_calls,
            max_bytes_per_call,
            completed: std::collections::BTreeSet::new(),
            call_ids: std::collections::BTreeMap::new(),
            function_names: std::collections::BTreeMap::new(),
            output_items_done: std::collections::BTreeSet::new(),
        }
    }

    /// Buffer an argument delta, rejecting late fragments and excess call buffers.
    ///
    /// This does not require prior call-ID registration: provider event ordering
    /// must be validated separately before a final action can be trusted.
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

    /// Record a function-call ID against the provider's output index.
    ///
    /// Duplicate IDs and indexes are rejected. This currently does not verify
    /// subsequent completion-event identity or the function name, so it is not
    /// sufficient to construct a trusted final action.
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
        let function_name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
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
        self.function_names.insert(index, function_name.to_owned());
        Ok(true)
    }

    /// Record an argument-done event after checking the buffered JSON parses.
    ///
    /// The index must have buffered arguments, but this currently does not
    /// require a registered call ID or a response-level completion event.
    /// A done event must never be treated as permission to execute a tool.
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

    /// Return only indexes that received argument-done events.
    ///
    /// These are untrusted parsed values, not validated executable actions.
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

    /// Return call IDs paired with completed argument JSON.
    ///
    /// Every buffered call must have both a registered provider call ID and an
    /// argument-done event. This is a correlation check, not action validation:
    /// the caller must still verify response completion and authorize tools.
    pub fn correlated_completed_calls(
        &self,
    ) -> Result<Vec<(String, serde_json::Value)>, ArgumentsError> {
        if self.calls.len() != self.completed.len() || self.calls.len() != self.call_ids.len() {
            return Err(ArgumentsError::InvalidEvent);
        }
        self.calls
            .iter()
            .map(|(index, arguments)| {
                if !self.completed.contains(index) {
                    return Err(ArgumentsError::InvalidEvent);
                }
                let call_id = self
                    .call_ids
                    .get(index)
                    .ok_or(ArgumentsError::UnexpectedIndex)?;
                Ok((call_id.clone(), arguments.parse_completed()?))
            })
            .collect()
    }

    /// Return function names, call IDs and parsed arguments for completed calls.
    ///
    /// This rejects incomplete correlation, but does not verify response-level
    /// completion or authorize the named tool. The normal decision path must
    /// validate action schemas and permissions before any execution.
    pub fn correlated_named_calls(
        &self,
    ) -> Result<Vec<(String, String, serde_json::Value)>, ArgumentsError> {
        if self.calls.len() != self.completed.len()
            || self.calls.len() != self.call_ids.len()
            || self.calls.len() != self.function_names.len()
        {
            return Err(ArgumentsError::InvalidEvent);
        }
        self.calls
            .iter()
            .map(|(index, arguments)| {
                if !self.completed.contains(index) {
                    return Err(ArgumentsError::InvalidEvent);
                }
                let call_id = self
                    .call_ids
                    .get(index)
                    .ok_or(ArgumentsError::UnexpectedIndex)?;
                let name = self
                    .function_names
                    .get(index)
                    .ok_or(ArgumentsError::UnexpectedIndex)?;
                Ok((call_id.clone(), name.clone(), arguments.parse_completed()?))
            })
            .collect()
    }

    /// Expose named calls only after the provider confirms response completion.
    ///
    /// A successful response-level event is distinct from argument-done events.
    /// Even after both checks, action schemas and tool permissions must be
    /// validated by the normal decision path before any tool can run.
    pub fn finalized_named_calls(
        &self,
        response_event: &SseFrame,
    ) -> Result<Vec<(String, String, serde_json::Value)>, ArgumentsError> {
        if response_event.event.as_deref() != Some("response.completed") {
            return Err(ArgumentsError::InvalidEvent);
        }
        let value: serde_json::Value =
            serde_json::from_str(&response_event.data).map_err(|_| ArgumentsError::InvalidEvent)?;
        let response = value.get("response").ok_or(ArgumentsError::InvalidEvent)?;
        if response.get("status").and_then(serde_json::Value::as_str) != Some("completed") {
            return Err(ArgumentsError::InvalidEvent);
        }
        if self.calls.len() != self.output_items_done.len() {
            return Err(ArgumentsError::InvalidEvent);
        }
        self.correlated_named_calls()
    }

    /// Validate a provider function-call output item completion event.
    ///
    /// The output index, call ID and function name must match the registered
    /// item, and its argument-done event must already have been observed.
    /// This does not authorize execution or replace response-level completion.
    pub fn complete_output_item(&mut self, frame: &SseFrame) -> Result<bool, ArgumentsError> {
        if frame.event.as_deref() != Some("response.output_item.done") {
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
            .ok_or(ArgumentsError::InvalidEvent)?;
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or(ArgumentsError::InvalidEvent)?;
        if self.call_ids.get(&index).map(String::as_str) != Some(call_id)
            || self.function_names.get(&index).map(String::as_str) != Some(name)
            || !self.completed.contains(&index)
            || self.output_items_done.contains(&index)
        {
            return Err(ArgumentsError::InvalidEvent);
        }
        self.output_items_done.insert(index);
        Ok(true)
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

/// Stateful, fail-closed assembler for one OpenAI Responses function-call stream.
///
/// This is a provider-data boundary, not an execution boundary. Callers must
/// validate each returned action against the normal tool schema and permissions.
pub struct ResponsesFinalActions {
    batch: ResponsesCallBatch,
    finished: bool,
    failed: bool,
}

impl ResponsesFinalActions {
    pub fn new(max_calls: usize, max_bytes_per_call: usize) -> Self {
        Self {
            batch: ResponsesCallBatch::new(max_calls, max_bytes_per_call),
            finished: false,
            failed: false,
        }
    }

    /// Consume one frame in provider order, returning untrusted actions only
    /// after successful response completion and all per-call done events.
    pub fn consume(
        &mut self,
        frame: &SseFrame,
    ) -> Result<Option<Vec<(String, String, serde_json::Value)>>, ArgumentsError> {
        if self.finished || self.failed {
            return Err(ArgumentsError::InvalidEvent);
        }
        let event = frame.event.as_deref();
        if matches!(
            event,
            Some("response.failed" | "response.incomplete" | "error")
        ) {
            self.failed = true;
            return Err(ArgumentsError::InvalidEvent);
        }
        // Never accept argument fragments before their registered output item.
        if event == Some("response.function_call_arguments.delta") {
            let value: serde_json::Value =
                serde_json::from_str(&frame.data).map_err(|_| ArgumentsError::InvalidEvent)?;
            let index = value
                .get("output_index")
                .and_then(serde_json::Value::as_u64)
                .ok_or(ArgumentsError::InvalidEvent)?;
            if !self.batch.call_ids.contains_key(&index) {
                self.failed = true;
                return Err(ArgumentsError::UnexpectedIndex);
            }
        }
        let result = match event {
            Some("response.output_item.added") => self.batch.register_call(frame).map(|_| None),
            Some("response.function_call_arguments.delta") => self.batch.push(frame).map(|_| None),
            Some("response.function_call_arguments.done") => {
                self.batch.complete(frame).map(|_| None)
            }
            Some("response.output_item.done") => {
                self.batch.complete_output_item(frame).map(|_| None)
            }
            Some("response.completed") => {
                if self.batch.calls.is_empty() {
                    Err(ArgumentsError::InvalidEvent)
                } else {
                    self.batch.finalized_named_calls(frame).map(Some)
                }
            }
            _ => Ok(None),
        };
        match result {
            Ok(actions) => {
                if actions.is_some() {
                    self.finished = true;
                }
                Ok(actions)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
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
    fn jsonl_sink_propagates_write_failure_without_advancing_sequence() {
        struct RejectWriter;
        impl std::io::Write for RejectWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("consumer disconnected"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut emitter = StreamEmitter::new(JsonlSink::new(RejectWriter));
        let result = emitter.emit(StreamEvent::ItemCompleted {
            item_id: "test".into(),
        });
        assert!(result.is_err());
        assert_eq!(emitter.next_sequence, 1);
    }

    #[test]
    fn sse_consumer_cancellation_stops_before_next_read() {
        struct OneReadOnly {
            reads: usize,
        }
        impl std::io::Read for OneReadOnly {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads > 1 {
                    panic!("SSE reader was polled after cancellation");
                }
                let bytes = b"data: first\n\ndata: second\n\n";
                buffer[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        let mut reader = OneReadOnly { reads: 0 };
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Err("cancelled")
        });
        assert!(matches!(result, Err(SseReadError::Consumer("cancelled"))));
        assert_eq!(delivered, 1);
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn sse_sink_backpressure_propagates_without_consuming_more_input() {
        struct OneReadOnly {
            reads: usize,
        }
        impl std::io::Read for OneReadOnly {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads > 1 {
                    panic!("SSE reader was polled after sink failure");
                }
                let bytes = b"event: response.output_text.delta\ndata: {\"delta\":\"hello\"}\n\n";
                buffer[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        struct RejectSink;
        impl EventSink for RejectSink {
            type Error = &'static str;
            fn send(&mut self, _: &StreamEnvelope) -> Result<(), Self::Error> {
                Err("backpressure")
            }
        }
        let mut reader = OneReadOnly { reads: 0 };
        let mut emitter = StreamEmitter::new(RejectSink);
        let result = consume_sse(&mut reader, 1024, |frame| {
            emit_provider_text_delta(&mut emitter, "message", &frame).map(|_| ())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Consumer(EmitError::Sink("backpressure")))
        ));
        assert_eq!(reader.reads, 1);
        assert_eq!(emitter.next_sequence, 1);
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
    fn sse_decoder_ignores_heartbeat_comments_without_leaking_event_state() {
        let mut decoder = SseDecoder::new(1024);
        let frames = decoder
            .push(b": heartbeat\n\nevent: response.output_text.delta\ndata: one\n\n: ping\n\ndata: two\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].event, None);
        assert_eq!(frames[1].data, "two");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_rejects_oversized_frame_across_chunks_and_resets() {
        let mut decoder = SseDecoder::new(12);
        assert!(decoder.push(b"data: long").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"er-than-limit"),
            Err(SseDecodeError::FrameTooLarge)
        );
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: ok\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
    }

    #[test]
    fn sse_decoder_rejects_oversized_incomplete_frame() {
        let mut decoder = SseDecoder::new(4);
        assert_eq!(decoder.push(b"12345"), Err(SseDecodeError::FrameTooLarge));
    }

    #[test]
    fn sse_decoder_rejects_partial_event_at_eof() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\ndata: {\"delta\":\"partial\"}")
            .unwrap()
            .is_empty());
        assert_eq!(decoder.finish(), Err(SseDecodeError::TruncatedFrame));
    }

    #[test]
    fn sse_decoder_recovers_cleanly_after_invalid_utf8() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\n")
            .unwrap()
            .is_empty());
        assert_eq!(
            decoder.push(b"data: \xff\n"),
            Err(SseDecodeError::InvalidUtf8)
        );
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: recovered\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].data, "recovered");
    }

    #[test]
    fn sse_decoder_rejects_truncated_utf8_and_recovers_after_error() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder.push(b"data: \xe2\x82").unwrap().is_empty());
        assert_eq!(decoder.finish(), Err(SseDecodeError::TruncatedFrame));
        assert_eq!(decoder.push(b"\xff\n"), Err(SseDecodeError::InvalidUtf8));
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: valid\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "valid");
    }

    #[test]
    fn sse_reader_rejects_truncated_provider_frame_at_eof() {
        let mut reader = std::io::Cursor::new(b"event: response.completed\ndata: {}".to_vec());
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Ok::<(), ()>(())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Decode(SseDecodeError::TruncatedFrame))
        ));
        assert_eq!(delivered, 0);
    }

    #[test]
    fn sse_reader_rejects_invalid_utf8_before_dispatch() {
        let mut reader = std::io::Cursor::new(b"data: \xff\n\n".to_vec());
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Ok::<(), ()>(())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Decode(SseDecodeError::InvalidUtf8))
        ));
        assert_eq!(delivered, 0);
    }

    #[test]
    fn sse_decoder_accepts_frame_at_exact_byte_limit() {
        let frame = b"data: ok\n\n";
        let mut decoder = SseDecoder::new(frame.len());
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_preserves_multiline_data_across_chunk_boundaries() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\r")
            .unwrap()
            .is_empty());
        assert!(decoder.push(b"\ndata: first\r\n").unwrap().is_empty());
        assert!(decoder.push(b"data: second\r\n\r").unwrap().is_empty());
        let frames = decoder.push(b"\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(frames[0].data, "first\nsecond");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_ignores_unknown_fields_and_resets_between_events() {
        let mut decoder = SseDecoder::new(1024);
        let frames = decoder
            .push(b"id: 7\nretry: 1000\nevent: first\ndata: one\n\ndata: two\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("first"));
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].event, None);
        assert_eq!(frames[1].data, "two");
        assert_eq!(decoder.finish(), Ok(()));
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
    fn provider_text_delta_rejects_malformed_or_unrecognized_payloads() {
        let invalid_json = SseFrame {
            event: Some("response.output_text.delta".into()),
            data: "{invalid".into(),
        };
        assert_eq!(provider_text_delta(&invalid_json), None);
        let wrong_type = SseFrame {
            event: Some("response.output_text.delta".into()),
            data: r#"{"delta":42}"#.into(),
        };
        assert_eq!(provider_text_delta(&wrong_type), None);
        let wrong_anthropic_delta = SseFrame {
            event: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"input_json_delta","text":"secret"}}"#.into(),
        };
        assert_eq!(provider_text_delta(&wrong_anthropic_delta), None);
        let unknown = SseFrame {
            event: Some("unknown.event".into()),
            data: r#"{"delta":"ignored"}"#.into(),
        };
        assert_eq!(provider_text_delta(&unknown), None);
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
    fn final_actions_require_full_ordered_completion_and_reject_replay() {
        let mut actions = ResponsesFinalActions::new(2, 128);
        let frames = [
            (
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
                }),
            ),
            (
                "response.function_call_arguments.delta",
                serde_json::json!({
                    "output_index": 0, "delta": r#"{"path":"src"}"#
                }),
            ),
            (
                "response.function_call_arguments.done",
                serde_json::json!({
                    "output_index": 0
                }),
            ),
            (
                "response.output_item.done",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
                }),
            ),
            (
                "response.completed",
                serde_json::json!({
                    "response": {"status": "completed"}
                }),
            ),
        ];
        for (index, (event, data)) in frames.iter().enumerate() {
            let frame = SseFrame {
                event: Some((*event).into()),
                data: data.to_string(),
            };
            let result = actions.consume(&frame).expect("valid stream event");
            if index == 4 {
                assert_eq!(
                    result,
                    Some(vec![(
                        "call_a".to_owned(),
                        "run".to_owned(),
                        serde_json::json!({"path": "src"})
                    )])
                );
                assert_eq!(actions.consume(&frame), Err(ArgumentsError::InvalidEvent));
            } else {
                assert_eq!(result, None);
            }
        }
    }

    #[test]
    fn final_actions_reject_unregistered_argument_fragments() {
        let mut actions = ResponsesFinalActions::new(1, 128);
        let delta = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
        };
        assert_eq!(
            actions.consume(&delta),
            Err(ArgumentsError::UnexpectedIndex)
        );
        assert_eq!(actions.consume(&delta), Err(ArgumentsError::InvalidEvent));
    }

    #[test]
    fn final_actions_reject_completed_response_without_tool_call() {
        let mut actions = ResponsesFinalActions::new(1, 128);
        let completed = SseFrame {
            event: Some("response.completed".into()),
            data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
        };
        assert_eq!(
            actions.consume(&completed),
            Err(ArgumentsError::InvalidEvent)
        );
    }

    #[test]
    fn final_actions_fail_closed_after_provider_failure() {
        let mut actions = ResponsesFinalActions::new(1, 64);
        let failed = SseFrame {
            event: Some("response.failed".into()),
            data: "{}".into(),
        };
        assert_eq!(actions.consume(&failed), Err(ArgumentsError::InvalidEvent));
        let completed = SseFrame {
            event: Some("response.completed".into()),
            data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
        };
        assert_eq!(
            actions.consume(&completed),
            Err(ArgumentsError::InvalidEvent)
        );
    }

    #[test]
    fn finalization_rejects_missing_output_item_completion() {
        let mut batch = ResponsesCallBatch::new(1, 64);
        let added = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&added), Ok(true));
        let delta = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
        };
        assert_eq!(batch.push(&delta), Ok(true));
        let done = SseFrame {
            event: Some("response.function_call_arguments.done".into()),
            data: serde_json::json!({"output_index": 0}).to_string(),
        };
        assert_eq!(batch.complete(&done), Ok(true));
        let completed = SseFrame {
            event: Some("response.completed".into()),
            data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
        };
        assert_eq!(
            batch.finalized_named_calls(&completed),
            Err(ArgumentsError::InvalidEvent)
        );
    }

    #[test]
    fn output_item_completion_rejects_unregistered_identity() {
        let mut batch = ResponsesCallBatch::new(1, 64);
        let done = SseFrame {
            event: Some("response.output_item.done".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(
            batch.complete_output_item(&done),
            Err(ArgumentsError::InvalidEvent)
        );
    }

    #[test]
    fn finalization_requires_successful_response_completion() {
        let batch = ResponsesCallBatch::new(1, 64);
        let incomplete = SseFrame {
            event: Some("response.incomplete".into()),
            data: serde_json::json!({"response": {"status": "incomplete"}}).to_string(),
        };
        assert_eq!(
            batch.finalized_named_calls(&incomplete),
            Err(ArgumentsError::InvalidEvent)
        );
        let failed = SseFrame {
            event: Some("response.completed".into()),
            data: serde_json::json!({"response": {"status": "failed"}}).to_string(),
        };
        assert_eq!(
            batch.finalized_named_calls(&failed),
            Err(ArgumentsError::InvalidEvent)
        );
        let completed = SseFrame {
            event: Some("response.completed".into()),
            data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
        };
        assert_eq!(batch.finalized_named_calls(&completed), Ok(vec![]));
    }

    #[test]
    fn named_calls_require_name_id_and_completion() {
        let mut batch = ResponsesCallBatch::new(1, 64);
        let added = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&added), Ok(true));
        assert!(batch.correlated_named_calls().is_err());
        let delta = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
        };
        assert_eq!(batch.push(&delta), Ok(true));
        let done = SseFrame {
            event: Some("response.function_call_arguments.done".into()),
            data: serde_json::json!({"output_index": 0}).to_string(),
        };
        assert_eq!(batch.complete(&done), Ok(true));
        assert_eq!(
            batch.correlated_named_calls(),
            Ok(vec![(
                "call_a".to_owned(),
                "run".to_owned(),
                serde_json::json!({})
            )])
        );
    }

    #[test]
    fn correlated_calls_require_registered_id_and_done_event() {
        let mut batch = ResponsesCallBatch::new(2, 64);
        let delta = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
        };
        assert_eq!(batch.push(&delta), Ok(true));
        assert!(batch.correlated_completed_calls().is_err());
        let added = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&added), Ok(true));
        assert!(batch.correlated_completed_calls().is_err());
        let done = SseFrame {
            event: Some("response.function_call_arguments.done".into()),
            data: serde_json::json!({"output_index": 0}).to_string(),
        };
        assert_eq!(batch.complete(&done), Ok(true));
        assert_eq!(
            batch.correlated_completed_calls(),
            Ok(vec![("call_a".to_owned(), serde_json::json!({}))])
        );
    }

    #[test]
    fn responses_batch_correlates_call_ids_and_rejects_duplicates() {
        let mut batch = ResponsesCallBatch::new(2, 64);
        let first = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(batch.register_call(&first), Ok(true));
        let duplicate = SseFrame {
            event: Some("response.output_item.added".into()),
            data: serde_json::json!({
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            })
            .to_string(),
        };
        assert_eq!(
            batch.register_call(&duplicate),
            Err(ArgumentsError::InvalidEvent)
        );
        assert_eq!(
            batch.register_call(&first),
            Err(ArgumentsError::InvalidEvent)
        );
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
    fn sse_reader_rejects_truncated_frame_at_eof() {
        let mut input =
            std::io::Cursor::new(b"event: content_block_delta\ndata: {\"delta\":".to_vec());
        let result = consume_sse(&mut input, 1024, |_frame| Ok::<_, ()>(()));
        assert!(matches!(
            result,
            Err(SseReadError::Decode(SseDecodeError::TruncatedFrame))
        ));
    }

    #[test]
    fn sse_reader_accepts_complete_frame_at_eof() {
        let mut input = std::io::Cursor::new(b"data: ok\n\n".to_vec());
        let mut frames = Vec::new();
        consume_sse(&mut input, 1024, |frame| {
            frames.push(frame);
            Ok::<_, ()>(())
        })
        .expect("complete stream");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
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
    #[test]
    fn failed_sink_delivery_does_not_advance_sequence_on_retry() {
        let mut emitter = StreamEmitter::new(FailOnceSink {
            attempts: 0,
            delivered: Vec::new(),
        });
        assert_eq!(
            emitter.emit(StreamEvent::ItemStarted {
                item_id: "message".into(),
                kind: ItemKind::Message,
            }),
            Err(EmitError::Sink("backpressure"))
        );
        assert_eq!(
            emitter.emit(StreamEvent::ItemStarted {
                item_id: "message".into(),
                kind: ItemKind::Message,
            }),
            Ok(1)
        );
        assert_eq!(
            emitter.emit(StreamEvent::ItemCompleted {
                item_id: "message".into(),
            }),
            Ok(2)
        );
        let sink = emitter.into_inner();
        assert_eq!(sink.attempts, 3);
        assert_eq!(sink.delivered.len(), 2);
        assert_eq!(sink.delivered[0].sequence, 1);
        assert_eq!(sink.delivered[1].sequence, 2);
    }

    #[test]
    fn text_sink_renders_only_public_event_content() {
        let mut emitter = StreamEmitter::new(TextSink::new(Vec::<u8>::new()));
        emitter
            .emit(StreamEvent::TurnStarted {
                task_id: "private-task".into(),
                turn_id: "turn".into(),
            })
            .unwrap();
        emitter
            .emit(StreamEvent::TextDelta {
                item_id: "message".into(),
                text: "hello".into(),
            })
            .unwrap();
        emitter
            .emit(StreamEvent::TurnCompleted {
                task_id: "private-task".into(),
                turn_id: "turn".into(),
                status: "completed".into(),
            })
            .unwrap();
        let output = String::from_utf8(emitter.into_inner().into_inner()).unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("\n[turn: completed]\n"));
        assert!(!output.contains("private-task"));
    }

    #[test]
    fn activity_jsonl_supports_dyn_writer_and_excludes_task_goal() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        session.create_task("private goal must not appear");
        let mut bytes = Vec::<u8>::new();
        let writer: &mut dyn std::io::Write = &mut bytes;
        write_activity_positions(&session, writer).expect("activity JSONL write");
        let output = String::from_utf8(bytes).expect("UTF-8 JSONL");
        assert!(!output.contains("private goal must not appear"));
        for line in output.lines() {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("valid activity JSONL");
            assert!(record.get("task_id").is_some());
            assert!(record.get("lifecycle_position").is_some());
            assert!(record.get("activity_position").is_some());
        }
    }

    #[test]
    fn jsonl_sink_preserves_sequence_and_event_types() {
        let mut emitter = StreamEmitter::new(JsonlSink::new(Vec::<u8>::new()));
        assert_eq!(
            emitter
                .emit(StreamEvent::ItemStarted {
                    item_id: "a".into(),
                    kind: ItemKind::Message,
                })
                .expect("JSONL sink write"),
            1
        );
        assert_eq!(
            emitter
                .emit(StreamEvent::ItemCompleted {
                    item_id: "a".into(),
                })
                .expect("JSONL sink write"),
            2
        );
        let output = String::from_utf8(emitter.into_inner().into_inner()).unwrap();
        let records: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["sequence"], 1);
        assert_eq!(records[1]["sequence"], 2);
        assert_eq!(records[0]["type"], "item_started");
        assert_eq!(records[1]["type"], "item_completed");
    }
}
