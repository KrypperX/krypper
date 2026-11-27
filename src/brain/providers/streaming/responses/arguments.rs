use crate::contracts::sse::SseFrame;

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
