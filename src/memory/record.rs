//! The one validated cross-session memory record.

use super::contract::{
    MemoryError, MemoryKind, MemoryProvenance, MemorySensitivity, Retention,
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_RECORD_BYTES, MAX_MEMORY_TAGS, MAX_MEMORY_TAG_BYTES,
    MEMORY_RECORD_VERSION,
};
use super::scope::WorkspaceKey;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One validated memory record. Content is stored verbatim as untrusted data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    version: u32,
    #[serde(with = "super::contract::uuid_text")]
    id: Uuid,
    workspace: WorkspaceKey,
    kind: MemoryKind,
    content: String,
    tags: Vec<String>,
    provenance: MemoryProvenance,
    retention: Retention,
    sensitivity: MemorySensitivity,
}

impl MemoryRecord {
    /// Builds a record, refusing secret material, empty or oversized content, bad tags,
    /// incomplete provenance and an expiry at or before creation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: WorkspaceKey,
        kind: MemoryKind,
        content: impl Into<String>,
        tags: Vec<String>,
        provenance: MemoryProvenance,
        retention: Retention,
        sensitivity: MemorySensitivity,
        created_at_unix: u64,
    ) -> Result<Self, MemoryError> {
        if sensitivity == MemorySensitivity::Secret {
            return Err(MemoryError::SecretBearingContent);
        }
        let content = content.into();
        if content.trim().is_empty() {
            return Err(MemoryError::EmptyContent);
        }
        if content.len() > MAX_MEMORY_CONTENT_BYTES {
            return Err(MemoryError::ContentTooLarge);
        }
        if tags.len() > MAX_MEMORY_TAGS {
            return Err(MemoryError::InvalidTag);
        }
        let mut seen: Vec<String> = Vec::with_capacity(tags.len());
        for tag in &tags {
            if tag.is_empty()
                || tag.len() > MAX_MEMORY_TAG_BYTES
                || tag.chars().any(char::is_control)
                || seen.contains(tag)
            {
                return Err(MemoryError::InvalidTag);
            }
            seen.push(tag.clone());
        }
        if provenance.recorded_at_unix > created_at_unix {
            return Err(MemoryError::MissingProvenance);
        }
        if let Retention::Until { unix_seconds } = retention {
            if unix_seconds <= created_at_unix {
                return Err(MemoryError::InvalidRetention);
            }
        }
        Ok(Self {
            version: MEMORY_RECORD_VERSION,
            id: Uuid::new_v4(),
            workspace,
            kind,
            content,
            tags,
            provenance,
            retention,
            sensitivity,
        })
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn workspace(&self) -> &WorkspaceKey {
        &self.workspace
    }

    pub fn kind(&self) -> MemoryKind {
        self.kind
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    pub fn provenance(&self) -> &MemoryProvenance {
        &self.provenance
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// True only when the record is unexpired at `now_unix`.
    pub fn is_live_at(&self, now_unix: u64) -> bool {
        match self.retention {
            Retention::Until { unix_seconds } => now_unix < unix_seconds,
            Retention::Permanent => true,
        }
    }

    /// The single visibility rule: same workspace and unexpired. Nothing else widens it.
    pub fn is_visible_to(&self, workspace: &WorkspaceKey, now_unix: u64) -> bool {
        self.workspace == *workspace && self.is_live_at(now_unix)
    }

    /// Deterministic ordering key for retrieval: newest evidence first, then id.
    pub fn ordering_key(&self) -> (u64, Uuid) {
        (self.provenance.recorded_at_unix, self.id)
    }

    /// Serializes with an explicit version header for later migration.
    pub fn to_json(&self) -> Result<String, MemoryError> {
        serde_json::to_string(self).map_err(|_| MemoryError::MalformedRecord)
    }

    /// Rejects unknown fields, unknown versions and oversized input before producing a record.
    pub fn from_json(json: &str) -> Result<Self, MemoryError> {
        if json.len() > MAX_MEMORY_RECORD_BYTES {
            return Err(MemoryError::RecordTooLarge);
        }
        #[derive(Deserialize)]
        struct VersionProbe {
            version: u32,
        }
        let probe: VersionProbe =
            serde_json::from_str(json).map_err(|_| MemoryError::MalformedRecord)?;
        if probe.version != MEMORY_RECORD_VERSION {
            return Err(MemoryError::UnsupportedVersion {
                version: probe.version,
            });
        }
        let record: Self = serde_json::from_str(json).map_err(|_| MemoryError::MalformedRecord)?;
        if record.sensitivity == MemorySensitivity::Secret {
            return Err(MemoryError::SecretBearingContent);
        }
        if record.version != MEMORY_RECORD_VERSION {
            return Err(MemoryError::UnsupportedVersion {
                version: record.version,
            });
        }
        Ok(record)
    }
}
