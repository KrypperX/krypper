//! Opt-in, bounded local persistence for cross-session memory (#219 PR 13).
//!
//! Persistence is never implicit: [`MemoryStore::open_opt_in`] returns `None` unless the host
//! explicitly enables it, and a disabled store reads and writes nothing. The store is a JSON-lines
//! file with a version header and one record per line. Every line is validated through
//! [`MemoryRecord::from_json`], so a corrupt line is skipped and counted, never coerced, while an
//! unsupported store version fails closed. Workspace isolation and expiry are decided at read time
//! by [`MemoryQuery`](super::MemoryQuery); the store never widens either.

use super::contract::MemoryError;
use super::record::MemoryRecord;
use super::scope::DeletionRequest;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Version of the store envelope written by [`MemoryStore`].
pub const MEMORY_STORE_VERSION: u32 = 1;
/// Maximum records one store retains.
pub const MAX_STORED_RECORDS: usize = 512;
/// Maximum encoded store file accepted or written.
pub const MAX_STORE_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Why a store operation was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryStoreError {
    /// The file could not be read, written or replaced.
    Io(String),
    /// The store header was missing or malformed.
    CorruptEnvelope,
    /// The store uses an unsupported envelope version; records are never coerced across versions.
    UnsupportedVersion { version: u32 },
    /// The file exceeded [`MAX_STORE_FILE_BYTES`].
    StoreTooLarge,
    /// Appending would exceed [`MAX_STORED_RECORDS`].
    StoreFull,
    /// A record could not be encoded for storage.
    UnencodableRecord,
}

impl fmt::Display for MemoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "memory store i/o failed: {message}"),
            Self::CorruptEnvelope => formatter.write_str("memory store header is corrupt"),
            Self::UnsupportedVersion { version } => {
                write!(formatter, "unsupported memory store version {version}")
            }
            Self::StoreTooLarge => write!(
                formatter,
                "memory store exceeds {MAX_STORE_FILE_BYTES} bytes"
            ),
            Self::StoreFull => {
                write!(
                    formatter,
                    "memory store exceeds {MAX_STORED_RECORDS} records"
                )
            }
            Self::UnencodableRecord => formatter.write_str("memory record could not be encoded"),
        }
    }
}

impl std::error::Error for MemoryStoreError {}

/// One load pass. Counters make every skipped record observable instead of silent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryLoad {
    pub records: Vec<MemoryRecord>,
    /// Lines that were not valid records.
    pub skipped_corrupt: usize,
    /// Lines written by a newer or older record version.
    pub skipped_unsupported: usize,
    /// Later lines that repeated an already loaded record id.
    pub skipped_duplicate: usize,
    /// Valid records beyond [`MAX_STORED_RECORDS`].
    pub skipped_over_limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreHeader {
    version: u32,
}

/// A host-owned store path. Holding a store grants no visibility beyond a query's workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryStore {
    path: PathBuf,
}

impl MemoryStore {
    /// Opt-in constructor: `None` when the host did not enable persistence, and nothing is opened.
    pub fn open_opt_in(path: impl Into<PathBuf>, enabled: bool) -> Option<Self> {
        enabled.then(|| Self { path: path.into() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads every readable record. A missing file is an empty store, not an error.
    pub fn load(&self) -> Result<MemoryLoad, MemoryStoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(MemoryLoad::default()),
            Err(error) => return Err(MemoryStoreError::Io(error.to_string())),
        };
        if bytes.len() > MAX_STORE_FILE_BYTES {
            return Err(MemoryStoreError::StoreTooLarge);
        }
        let text = String::from_utf8(bytes).map_err(|_| MemoryStoreError::CorruptEnvelope)?;
        let mut lines = text.lines();
        let header: StoreHeader = lines
            .next()
            .and_then(|line| serde_json::from_str(line).ok())
            .ok_or(MemoryStoreError::CorruptEnvelope)?;
        if header.version != MEMORY_STORE_VERSION {
            return Err(MemoryStoreError::UnsupportedVersion {
                version: header.version,
            });
        }
        let mut load = MemoryLoad::default();
        let mut seen = BTreeSet::new();
        for line in lines.filter(|line| !line.trim().is_empty()) {
            match MemoryRecord::from_json(line) {
                Ok(record) => {
                    if !seen.insert(record.id()) {
                        load.skipped_duplicate += 1;
                    } else if load.records.len() >= MAX_STORED_RECORDS {
                        load.skipped_over_limit += 1;
                    } else {
                        load.records.push(record);
                    }
                }
                Err(MemoryError::UnsupportedVersion { .. }) => load.skipped_unsupported += 1,
                Err(_) => load.skipped_corrupt += 1,
            }
        }
        Ok(load)
    }

    /// Appends or replaces one record, pruning records expired at `now_unix`.
    ///
    /// The rewrite is atomic: the new content is written beside the target and renamed over it, so
    /// a crash leaves the previous store intact rather than a half-written file.
    pub fn append(&self, record: &MemoryRecord, now_unix: u64) -> Result<(), MemoryStoreError> {
        let mut load = self.load()?;
        load.records
            .retain(|existing| existing.id() != record.id() && existing.is_live_at(now_unix));
        if record.is_live_at(now_unix) {
            load.records.push(record.clone());
        }
        if load.records.len() > MAX_STORED_RECORDS {
            return Err(MemoryStoreError::StoreFull);
        }
        self.write(&load.records)
    }

    /// Removes every record covered by `request` and rewrites the store.
    pub fn apply_deletion(&self, request: &DeletionRequest) -> Result<usize, MemoryStoreError> {
        let mut load = self.load()?;
        let before = load.records.len();
        load.records.retain(|record| !request.covers(record));
        let removed = before - load.records.len();
        if removed > 0 {
            self.write(&load.records)?;
        }
        Ok(removed)
    }

    fn write(&self, records: &[MemoryRecord]) -> Result<(), MemoryStoreError> {
        let mut text = format!("{{\"version\":{MEMORY_STORE_VERSION}}}\n");
        for record in records {
            text.push_str(
                &record
                    .to_json()
                    .map_err(|_| MemoryStoreError::UnencodableRecord)?,
            );
            text.push('\n');
        }
        if text.len() > MAX_STORE_FILE_BYTES {
            return Err(MemoryStoreError::StoreTooLarge);
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|error| MemoryStoreError::Io(error.to_string()))?;
            }
        }
        let temporary = self.path.with_extension("tmp");
        fs::write(&temporary, text).map_err(|error| MemoryStoreError::Io(error.to_string()))?;
        fs::rename(&temporary, &self.path).map_err(|error| MemoryStoreError::Io(error.to_string()))
    }
}
