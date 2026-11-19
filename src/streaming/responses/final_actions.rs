use super::super::emitter::{EmitError, EventSink, StreamEmitter};
use super::super::events::StreamEvent;
use super::super::sse::{provider_text_delta, SseFrame};
use super::arguments::ArgumentsError;
use super::batch::ResponsesCallBatch;

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
