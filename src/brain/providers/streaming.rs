mod anthropic;
mod responses;

use crate::contracts::sse::SseFrame;

pub(crate) use anthropic::{AnthropicAssemblyError, AnthropicFinalActions};
pub use responses::{
    ArgumentsError, ResponsesArguments, ResponsesCallBatch, ResponsesFinalActions,
};

/// Extract text from recognized provider events without exposing tool-call
/// argument fragments. Provider adapters validate those with their own parser.
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
