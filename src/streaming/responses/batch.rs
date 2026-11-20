use super::super::sse::SseFrame;
use super::arguments::{ArgumentsError, ResponsesArguments};

/// Collect interleaved Responses function-call fragments by output index.
///
/// Call IDs and completion markers are tracked separately from JSON buffers.
/// Neither a completed buffer nor a registered call ID authorizes execution.
pub struct ResponsesCallBatch {
    pub(super) calls: std::collections::BTreeMap<u64, ResponsesArguments>,
    max_calls: usize,
    max_bytes_per_call: usize,
    completed: std::collections::BTreeSet<u64>,
    pub(super) call_ids: std::collections::BTreeMap<u64, String>,
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
