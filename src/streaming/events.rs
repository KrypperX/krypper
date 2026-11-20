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
    /// Read-only high-water marks from the authoritative task snapshot.
    /// Lifecycle and activity positions belong to independent histories.
    ActivityPositionUpdated {
        task_id: String,
        lifecycle_position: u64,
        activity_position: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Message,
    Tool,
}
