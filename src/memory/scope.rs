//! Per-workspace scope, bounded queries and explicit deletion.

use super::contract::{
    MemoryError, MemoryKind, MAX_QUERY_BYTES, MAX_QUERY_RECORDS, MAX_WORKSPACE_KEY_BYTES,
};
use super::record::MemoryRecord;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Host-provided identity of one workspace. Model output and file content are never keys.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceKey(String);

impl WorkspaceKey {
    /// Validates a host-provided key. Keys are opaque: this checks shape, not meaning.
    pub fn new(key: impl Into<String>) -> Result<Self, MemoryError> {
        let key = key.into();
        if key.is_empty()
            || key.len() > MAX_WORKSPACE_KEY_BYTES
            || key.chars().any(char::is_control)
        {
            return Err(MemoryError::InvalidWorkspaceKey);
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded read request. A query never widens visibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryQuery {
    pub workspace: WorkspaceKey,
    pub kinds: Vec<MemoryKind>,
    pub max_records: usize,
    pub max_bytes: usize,
    pub now_unix: u64,
}

impl MemoryQuery {
    pub fn new(workspace: WorkspaceKey, now_unix: u64) -> Result<Self, MemoryError> {
        if now_unix == 0 {
            return Err(MemoryError::InvalidDeletionScope);
        }
        Ok(Self {
            workspace,
            kinds: Vec::new(),
            max_records: MAX_QUERY_RECORDS,
            max_bytes: MAX_QUERY_BYTES,
            now_unix,
        })
    }

    pub fn with_kinds(mut self, kinds: Vec<MemoryKind>) -> Self {
        self.kinds = kinds;
        self
    }

    /// Applies the query to candidate records: visibility first, then kind, then bounds.
    pub fn admit<'a>(
        &self,
        records: impl IntoIterator<Item = &'a MemoryRecord>,
    ) -> Vec<&'a MemoryRecord> {
        let mut admitted: Vec<&MemoryRecord> = records
            .into_iter()
            .filter(|record| record.is_visible_to(&self.workspace, self.now_unix))
            .filter(|record| self.kinds.is_empty() || self.kinds.contains(&record.kind()))
            .collect();
        admitted.sort_by_key(|record| std::cmp::Reverse(record.ordering_key()));
        let mut bytes = 0usize;
        admitted.retain(|record| {
            let next = bytes + record.content().len();
            if next > self.max_bytes {
                return false;
            }
            bytes = next;
            true
        });
        admitted.truncate(self.max_records);
        admitted
    }
}

/// Deletion is explicit and scoped; there is no implicit purge path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionScope {
    Record(Uuid),
    Workspace(WorkspaceKey),
    Everything,
}

/// A validated deletion request. The caller performs it; this type only states intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionRequest {
    pub scope: DeletionScope,
    pub requested_at_unix: u64,
}

impl DeletionRequest {
    pub fn new(scope: DeletionScope, requested_at_unix: u64) -> Result<Self, MemoryError> {
        if requested_at_unix == 0 {
            return Err(MemoryError::InvalidDeletionScope);
        }
        Ok(Self {
            scope,
            requested_at_unix,
        })
    }

    /// Whether this request covers `record`. Record scope never touches another id.
    pub fn covers(&self, record: &MemoryRecord) -> bool {
        match &self.scope {
            DeletionScope::Record(id) => *id == record.id(),
            DeletionScope::Workspace(workspace) => record.workspace() == workspace,
            DeletionScope::Everything => true,
        }
    }
}
