//! Host-facing opt-in recall (#219 PR 14).
//!
//! Recall is enabled by handing a store to [`recall_opt_in`]. Without a store the call returns
//! `None` and reads nothing, so a host that never enables memory keeps its current prompts. The
//! policy can only narrow a query's bounds; it never widens visibility, kinds or expiry.

use super::contract::{MemoryError, MAX_QUERY_BYTES, MAX_QUERY_RECORDS};
use super::retrieval::{collect_memory_context, MemoryContext, MAX_MEMORY_CONTEXT_BYTES};
use super::scope::MemoryQuery;
use super::store::{MemoryStore, MemoryStoreError};

/// Host-owned recall bounds. Ignoring a stored record is always preferred to exceeding them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryRecallPolicy {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl Default for MemoryRecallPolicy {
    fn default() -> Self {
        Self {
            max_records: 16,
            max_bytes: MAX_MEMORY_CONTEXT_BYTES,
        }
    }
}

impl MemoryRecallPolicy {
    /// Rejects a zero or contract-breaking bound instead of silently clamping it.
    pub fn new(max_records: usize, max_bytes: usize) -> Result<Self, MemoryError> {
        if max_records == 0
            || max_records > MAX_QUERY_RECORDS
            || max_bytes == 0
            || max_bytes > MAX_QUERY_BYTES
        {
            return Err(MemoryError::QueryTooLarge);
        }
        Ok(Self {
            max_records,
            max_bytes,
        })
    }

    /// Narrows a query: the recalled set is the intersection of query and policy bounds.
    fn bound(&self, query: &MemoryQuery) -> MemoryQuery {
        let mut bounded = query.clone();
        bounded.max_records = bounded.max_records.min(self.max_records);
        bounded.max_bytes = bounded.max_bytes.min(self.max_bytes);
        bounded
    }
}

/// Recalls only when the host supplied a store; `None` means memory is not enabled.
///
/// The result is advisory data. It carries no grant, so no caller can turn memory content into
/// tool authority: authorization remains in the existing capability policy.
pub fn recall_opt_in(
    store: Option<&MemoryStore>,
    query: &MemoryQuery,
    policy: &MemoryRecallPolicy,
) -> Result<Option<MemoryContext>, MemoryStoreError> {
    let Some(store) = store else {
        return Ok(None);
    };
    let load = store.load()?;
    Ok(Some(collect_memory_context(
        &load.records,
        &policy.bound(query),
    )))
}
