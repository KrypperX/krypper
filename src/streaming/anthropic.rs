use super::sse::{provider_text_delta, SseFrame};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Failures while validating or assembling an Anthropic Messages stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnthropicAssemblyError {
    InvalidJson,
    InvalidToolCall(&'static str),
}

/// Stateful, fail-closed assembler for one Anthropic Messages tool-use stream.
///
/// It returns text deltas separately from tool arguments and never executes a
/// tool. The caller remains responsible for validating the final action.
pub(crate) struct AnthropicFinalActions {
    blocks: BTreeMap<usize, (String, String)>,
    completed_blocks: BTreeSet<usize>,
    max_bytes_per_call: usize,
    completed: bool,
    failed: bool,
    tool_use_stop: bool,
}

impl AnthropicFinalActions {
    pub(crate) fn new(max_bytes_per_call: usize) -> Self {
        Self {
            blocks: BTreeMap::new(),
            completed_blocks: BTreeSet::new(),
            max_bytes_per_call,
            completed: false,
            failed: false,
            tool_use_stop: false,
        }
    }

    /// Consume one provider frame, returning only decoded display text.
    pub(crate) fn push(
        &mut self,
        frame: &SseFrame,
    ) -> Result<Option<String>, AnthropicAssemblyError> {
        if self.completed || self.failed {
            self.failed = true;
            return Err(Self::invalid_tool_call());
        }

        let result = self.push_frame(frame);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Return the single complete tool call only after a valid terminal frame.
    pub(crate) fn finish(self) -> Result<(String, Value), AnthropicAssemblyError> {
        if self.failed
            || !self.completed
            || !self.tool_use_stop
            || self.blocks.len() != 1
            || self.completed_blocks.len() != 1
        {
            return Err(AnthropicAssemblyError::InvalidToolCall(
                "stream ended without one complete tool call",
            ));
        }
        let (_, (name, arguments)) = self
            .blocks
            .into_iter()
            .next()
            .ok_or(AnthropicAssemblyError::InvalidToolCall("missing tool call"))?;
        let arguments =
            serde_json::from_str(&arguments).map_err(|_| AnthropicAssemblyError::InvalidJson)?;
        Ok((name, arguments))
    }

    fn push_frame(&mut self, frame: &SseFrame) -> Result<Option<String>, AnthropicAssemblyError> {
        let event: Value =
            serde_json::from_str(&frame.data).map_err(|_| AnthropicAssemblyError::InvalidJson)?;
        if frame.event.as_deref() != event["type"].as_str() {
            return Err(Self::invalid_tool_call());
        }
        if self.tool_use_stop
            && matches!(
                frame.event.as_deref(),
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
        {
            return Err(Self::invalid_tool_call());
        }

        match frame.event.as_deref() {
            Some("content_block_start") => {
                let index = usize::try_from(
                    event["index"]
                        .as_u64()
                        .ok_or_else(Self::invalid_tool_call)?,
                )
                .map_err(|_| Self::invalid_tool_call())?;
                let block = &event["content_block"];
                if block["type"] == "tool_use" {
                    let name = block["name"].as_str().ok_or_else(Self::invalid_tool_call)?;
                    if !self.blocks.is_empty() || self.completed_blocks.contains(&index) {
                        return Err(Self::invalid_tool_call());
                    }
                    self.blocks.insert(index, (name.to_owned(), String::new()));
                }
            }
            Some("content_block_delta") => {
                if event["delta"]["type"] == "input_json_delta" {
                    let index = usize::try_from(
                        event["index"]
                            .as_u64()
                            .ok_or_else(Self::invalid_tool_call)?,
                    )
                    .map_err(|_| Self::invalid_tool_call())?;
                    let chunk = event["delta"]["partial_json"]
                        .as_str()
                        .ok_or_else(Self::invalid_tool_call)?;
                    let (_, arguments) = self
                        .blocks
                        .get_mut(&index)
                        .ok_or_else(Self::invalid_tool_call)?;
                    if self.completed_blocks.contains(&index)
                        || arguments.len().saturating_add(chunk.len()) > self.max_bytes_per_call
                    {
                        return Err(Self::invalid_tool_call());
                    }
                    arguments.push_str(chunk);
                }
            }
            Some("content_block_stop") => {
                let index = usize::try_from(
                    event["index"]
                        .as_u64()
                        .ok_or_else(Self::invalid_tool_call)?,
                )
                .map_err(|_| Self::invalid_tool_call())?;
                if self.blocks.contains_key(&index) && !self.completed_blocks.insert(index) {
                    return Err(Self::invalid_tool_call());
                }
            }
            Some("message_delta") => {
                if self.tool_use_stop
                    || event["delta"]["stop_reason"] != "tool_use"
                    || self.blocks.len() != 1
                    || self.completed_blocks.len() != 1
                {
                    return Err(Self::invalid_tool_call());
                }
                self.tool_use_stop = true;
            }
            Some("message_stop") => {
                if !self.tool_use_stop || self.blocks.len() != 1 || self.completed_blocks.len() != 1
                {
                    return Err(Self::invalid_tool_call());
                }
                self.completed = true;
            }
            Some("error") => return Err(Self::invalid_tool_call()),
            _ => {}
        }

        let text = provider_text_delta(frame);
        if frame.event.as_deref() == Some("content_block_delta")
            && event["delta"]["type"] == "text_delta"
            && text.is_none()
        {
            return Err(AnthropicAssemblyError::InvalidJson);
        }
        Ok(text)
    }

    fn invalid_tool_call() -> AnthropicAssemblyError {
        AnthropicAssemblyError::InvalidToolCall("invalid Anthropic streaming tool call")
    }
}

#[cfg(test)]
mod tests {
    use super::AnthropicFinalActions;
    use crate::streaming::SseFrame;
    use serde_json::{json, Value};

    fn frame(event: &str, mut data: Value) -> SseFrame {
        data["type"] = Value::String(event.to_owned());
        SseFrame {
            event: Some(event.to_owned()),
            data: data.to_string(),
        }
    }

    fn tool_start(index: u64) -> SseFrame {
        frame(
            "content_block_start",
            json!({"index":index,"content_block":{"type":"tool_use","name":"read_file"}}),
        )
    }

    fn argument_delta(index: u64, partial_json: &str) -> SseFrame {
        frame(
            "content_block_delta",
            json!({"index":index,"delta":{"type":"input_json_delta","partial_json":partial_json}}),
        )
    }

    fn complete_tool_prefix() -> AnthropicFinalActions {
        let mut actions = AnthropicFinalActions::new(32);
        actions.push(&tool_start(0)).expect("tool start");
        actions
            .push(&argument_delta(0, r#"{"path":"README.md"}"#))
            .expect("arguments");
        actions
            .push(&frame("content_block_stop", json!({"index":0})))
            .expect("block stop");
        actions
    }

    fn complete_tool_use() -> AnthropicFinalActions {
        let mut actions = complete_tool_prefix();
        actions
            .push(&frame(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"}}),
            ))
            .expect("tool-use stop reason");
        actions
            .push(&frame("message_stop", json!({})))
            .expect("message stop");
        actions
    }

    #[test]
    fn returns_one_complete_tool_call_after_message_stop() {
        let (name, arguments) = complete_tool_use().finish().expect("complete call");
        assert_eq!(name, "read_file");
        assert_eq!(arguments, json!({"path":"README.md"}));
    }

    #[test]
    fn returns_text_deltas_but_never_tool_arguments() {
        let mut actions = AnthropicFinalActions::new(32);
        let text = frame(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"hello"}}),
        );
        assert_eq!(
            actions.push(&text).expect("text delta").as_deref(),
            Some("hello")
        );
        actions.push(&tool_start(0)).expect("tool start");
        assert_eq!(
            actions
                .push(&argument_delta(0, "secret"))
                .expect("tool argument"),
            None
        );
    }

    #[test]
    fn rejects_event_type_mismatch_and_malformed_text_delta() {
        let mut actions = AnthropicFinalActions::new(32);
        let mismatch = SseFrame {
            event: Some("message_stop".to_owned()),
            data: json!({"type":"message_delta"}).to_string(),
        };
        assert!(actions.push(&mismatch).is_err());

        let mut actions = AnthropicFinalActions::new(32);
        let malformed_text = frame(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta"}}),
        );
        assert!(actions.push(&malformed_text).is_err());
    }

    #[test]
    fn rejects_second_tool_and_arguments_for_unknown_or_completed_blocks() {
        let mut actions = AnthropicFinalActions::new(32);
        actions.push(&tool_start(0)).expect("first tool");
        assert!(actions.push(&tool_start(1)).is_err());

        let mut actions = AnthropicFinalActions::new(32);
        assert!(actions.push(&argument_delta(0, "{}")).is_err());

        let mut actions = AnthropicFinalActions::new(32);
        actions.push(&tool_start(0)).expect("tool start");
        actions
            .push(&frame("content_block_stop", json!({"index":0})))
            .expect("block stop");
        assert!(actions.push(&argument_delta(0, "{}")).is_err());
    }

    #[test]
    fn rejects_oversized_arguments() {
        let mut actions = AnthropicFinalActions::new(2);
        actions.push(&tool_start(0)).expect("tool start");
        assert!(actions.push(&argument_delta(0, "123")).is_err());
    }

    #[test]
    fn requires_one_completed_tool_before_message_stop_reason_and_end() {
        let mut actions = AnthropicFinalActions::new(32);
        assert!(actions
            .push(&frame(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"}}),
            ))
            .is_err());

        let mut actions = AnthropicFinalActions::new(32);
        actions.push(&tool_start(0)).expect("tool start");
        assert!(actions
            .push(&frame(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"}}),
            ))
            .is_err());

        let mut actions = AnthropicFinalActions::new(32);
        assert!(actions.push(&frame("message_stop", json!({}))).is_err());

        let actions = complete_tool_prefix();
        assert!(actions.finish().is_err());
    }

    #[test]
    fn rejects_content_after_stop_reason_and_any_frame_after_completion() {
        let mut actions = complete_tool_prefix();
        actions
            .push(&frame(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"}}),
            ))
            .expect("tool-use stop reason");
        assert!(actions.push(&tool_start(1)).is_err());

        let mut actions = complete_tool_use();
        assert!(actions.push(&frame("ping", json!({}))).is_err());
    }

    #[test]
    fn rejects_error_events() {
        let mut actions = AnthropicFinalActions::new(32);
        assert!(actions
            .push(&frame("error", json!({"error":{"type":"api_error"}})))
            .is_err());
    }
}
