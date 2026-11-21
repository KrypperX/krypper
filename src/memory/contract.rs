//! Types shared by the memory contract: kinds, provenance, retention and errors.
//!
//! This module holds no storage and no decision about visibility. `record` owns the bounded
//! record, `scope` owns per-workspace visibility, bounded queries and deletion.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Current wire version written by [`MemoryRecord::to_json`](super::MemoryRecord::to_json).
pub const MEMORY_RECORD_VERSION: u32 = 1;
/// Maximum encoded size accepted for one record.
pub const MAX_MEMORY_RECORD_BYTES: usize = 16 * 1024;
/// Maximum content bytes of one record.
pub const MAX_MEMORY_CONTENT_BYTES: usize = 4096;
/// Maximum workspace-key bytes.
pub const MAX_WORKSPACE_KEY_BYTES: usize = 256;
/// Maximum tags per record.
pub const MAX_MEMORY_TAGS: usize = 8;
/// Maximum tag bytes.
pub const MAX_MEMORY_TAG_BYTES: usize = 64;
/// Maximum records one query may request.
pub const MAX_QUERY_RECORDS: usize = 64;
/// Maximum bytes one query may request.
pub const MAX_QUERY_BYTES: usize = 32 * 1024;

/// UUIDs travel as text so the contract needs no dependency feature toggle.
pub(super) mod uuid_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        Uuid::parse_str(&text).map_err(serde::de::Error::custom)
    }
}

/// Reasons a record, query or deletion request is refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryError {
    /// Content was empty or only whitespace.
    EmptyContent,
    /// Content exceeds [`MAX_MEMORY_CONTENT_BYTES`].
    ContentTooLarge,
    /// A workspace key was empty, too long, or contained control characters.
    InvalidWorkspaceKey,
    /// A tag was empty, too long, or duplicated.
    InvalidTag,
    /// A record carried secret-classified material.
    SecretBearingContent,
    /// A record had no usable provenance timestamp.
    MissingProvenance,
    /// Retention was inconsistent, for example an expiry at or before creation.
    InvalidRetention,
    /// A record uses an unsupported wire version.
    UnsupportedVersion { version: u32 },
    /// A record or envelope exceeded [`MAX_MEMORY_RECORD_BYTES`].
    RecordTooLarge,
    /// The wire form was malformed or contained unknown fields.
    MalformedRecord,
    /// A query exceeded [`MAX_QUERY_RECORDS`] or [`MAX_QUERY_BYTES`].
    QueryTooLarge,
    /// A deletion request had an unusable scope or timestamp.
    InvalidDeletionScope,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyContent => formatter.write_str("memory content must not be empty"),
            Self::ContentTooLarge => write!(
                formatter,
                "memory content exceeds {MAX_MEMORY_CONTENT_BYTES} bytes"
            ),
            Self::InvalidWorkspaceKey => formatter.write_str("invalid workspace key"),
            Self::InvalidTag => formatter.write_str("invalid or duplicate memory tag"),
            Self::SecretBearingContent => {
                formatter.write_str("secret-classified material must never become memory")
            }
            Self::MissingProvenance => formatter.write_str("memory provenance is incomplete"),
            Self::InvalidRetention => formatter.write_str("memory retention is inconsistent"),
            Self::UnsupportedVersion { version } => {
                write!(formatter, "unsupported memory record version {version}")
            }
            Self::RecordTooLarge => write!(
                formatter,
                "memory record exceeds {MAX_MEMORY_RECORD_BYTES} bytes"
            ),
            Self::MalformedRecord => formatter.write_str("malformed memory record"),
            Self::QueryTooLarge => formatter.write_str("memory query exceeds its bounds"),
            Self::InvalidDeletionScope => formatter.write_str("invalid memory deletion scope"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// What a record asserts. Kinds never grant authority over tools or the workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A user or host constraint that must survive the session.
    Constraint,
    /// A decision plus its rationale.
    Decision,
    /// A verified fact about the workspace.
    Fact,
    /// A stated preference.
    Preference,
    /// A verified outcome of earlier work.
    Outcome,
}

/// Where a record came from. Every record carries exactly one source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MemorySource {
    /// Provenance of an observed task turn; access is still decided by the caller.
    TaskTurn {
        #[serde(with = "uuid_text")]
        task: Uuid,
        #[serde(with = "uuid_text")]
        turn: Uuid,
    },
    /// An explicit host or user note.
    HostNote,
    /// A typed outcome of a completed tool action.
    ToolOutcome { tool: String },
}

/// Provenance that must accompany every record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryProvenance {
    pub source: MemorySource,
    /// Unix seconds when the evidence was recorded.
    pub recorded_at_unix: u64,
    /// Generator-supplied revision of the evidence, for example a git commit.
    pub revision: Option<String>,
}

impl MemoryProvenance {
    pub fn new(source: MemorySource, recorded_at_unix: u64) -> Result<Self, MemoryError> {
        if recorded_at_unix == 0 {
            return Err(MemoryError::MissingProvenance);
        }
        if let MemorySource::ToolOutcome { tool } = &source {
            if tool.is_empty()
                || tool.len() > MAX_MEMORY_TAG_BYTES
                || tool.chars().any(char::is_control)
            {
                return Err(MemoryError::MissingProvenance);
            }
        }
        Ok(Self {
            source,
            recorded_at_unix,
            revision: None,
        })
    }

    pub fn with_revision(mut self, revision: impl Into<String>) -> Result<Self, MemoryError> {
        let revision = revision.into();
        if revision.is_empty()
            || revision.len() > MAX_MEMORY_TAG_BYTES
            || revision.chars().any(char::is_control)
        {
            return Err(MemoryError::MissingProvenance);
        }
        self.revision = Some(revision);
        Ok(self)
    }
}

/// How long a record stays readable. Expiry is evaluated by the caller's clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Retention {
    /// Readable until the given Unix second (exclusive).
    Until { unix_seconds: u64 },
    /// Readable indefinitely; deletion still requires an explicit request.
    Permanent,
}

/// Classification of record content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySensitivity {
    Normal,
    /// Not storable: rejected at construction so secrets never enter memory.
    Secret,
}
