//! Cross-session memory (#219 PR 12).
//!
//! The contract defines what a memory record is, who may see it, how long it lives and how it is
//! deleted. Nothing here persists or injects memory: storage and retrieval are separate subtasks,
//! and recall stays opt-in and provenance-labelled when they land. Memory is data, never
//! authority over tools, files or the sandbox, and it never reads TaskSession checkpoints or
//! history.
//!
//! Security invariants:
//! - A record belongs to exactly one host-provided workspace key and is invisible to every other
//!   workspace, whatever the query says.
//! - Records cannot carry secrets. [`MemorySensitivity::Secret`] exists only so a caller that
//!   classifies secret material as memory is rejected with [`MemoryError::SecretBearingContent`].
//! - Unknown wire versions and unknown fields are rejected, never coerced.

mod contract;
mod record;
mod scope;

#[cfg(test)]
mod tests;

pub use self::contract::{
    MemoryError, MemoryKind, MemoryProvenance, MemorySensitivity, MemorySource, Retention,
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_RECORD_BYTES, MAX_MEMORY_TAGS, MAX_MEMORY_TAG_BYTES,
    MAX_QUERY_BYTES, MAX_QUERY_RECORDS, MAX_WORKSPACE_KEY_BYTES, MEMORY_RECORD_VERSION,
};
pub use self::record::MemoryRecord;
pub use self::scope::{DeletionRequest, DeletionScope, MemoryQuery, WorkspaceKey};
