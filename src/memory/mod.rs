//! Cross-session memory (#219 PR 12 contract, PR 13 storage and recall, PR 14 integration and
//! evaluation).
//!
//! The contract defines what a memory record is, who may see it, how long it lives and how it is
//! deleted. [`MemoryStore`] adds opt-in bounded local persistence, [`MemoryContext`] adds
//! deterministic bounded recall through the existing prompt builders, and [`recall_opt_in`] is the
//! host-facing door: no store, no recall. Recall stays opt-in and provenance-labelled, and
//! [`evaluate`] measures recall against held-out fixtures instead of asserting model behaviour.
//!
//! Security invariants:
//! - A record belongs to exactly one host-provided workspace key and is invisible to every other
//!   workspace, whatever the query says.
//! - Records cannot carry secrets. [`MemorySensitivity::Secret`] exists only so a caller that
//!   classifies secret material as memory is rejected with [`MemoryError::SecretBearingContent`].
//! - Unknown wire versions and unknown fields are rejected, never coerced.
//! - A disabled store opens no path, reads nothing and writes nothing.
//! - Memory content is data: it can never grant a tool, capability or execution profile.

mod contract;
mod evaluation;
mod integration;
mod record;
mod retrieval;
mod scope;
mod store;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod retrieval_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;

pub use self::contract::{
    MemoryError, MemoryKind, MemoryProvenance, MemorySensitivity, MemorySource, Retention,
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_RECORD_BYTES, MAX_MEMORY_TAGS, MAX_MEMORY_TAG_BYTES,
    MAX_QUERY_BYTES, MAX_QUERY_RECORDS, MAX_WORKSPACE_KEY_BYTES, MEMORY_RECORD_VERSION,
};
pub use self::evaluation::{evaluate, EvaluationCase, EvaluationReport};
pub use self::integration::{recall_opt_in, MemoryRecallPolicy};
pub use self::record::MemoryRecord;
pub use self::retrieval::{
    append_memory_context, collect_memory_context, recalled_ids, MemoryContext, MemoryExcerpt,
    MAX_MEMORY_CONTEXT_BYTES,
};
pub use self::scope::{DeletionRequest, DeletionScope, MemoryQuery, WorkspaceKey};
pub use self::store::{
    MemoryLoad, MemoryStore, MemoryStoreError, MAX_STORED_RECORDS, MAX_STORE_FILE_BYTES,
    MEMORY_STORE_VERSION,
};
