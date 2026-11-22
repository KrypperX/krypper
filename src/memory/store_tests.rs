use super::contract::{MemoryKind, MemoryProvenance, MemorySensitivity, MemorySource, Retention};
use super::record::MemoryRecord;
use super::scope::{DeletionRequest, DeletionScope, MemoryQuery, WorkspaceKey};
use super::store::{MemoryStore, MemoryStoreError, MEMORY_STORE_VERSION};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

fn store_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("krypper-memory-store-{}", Uuid::new_v4()))
        .join("memory.jsonl")
}

fn workspace(key: &str) -> WorkspaceKey {
    WorkspaceKey::new(key).expect("workspace key")
}

fn record(key: &str, content: &str, recorded_at: u64) -> MemoryRecord {
    MemoryRecord::new(
        workspace(key),
        MemoryKind::Fact,
        content,
        vec!["fixture".to_owned()],
        MemoryProvenance::new(MemorySource::HostNote, recorded_at).expect("provenance"),
        Retention::Permanent,
        MemorySensitivity::Normal,
        recorded_at,
    )
    .expect("record")
}

fn enabled_store(path: &std::path::Path) -> MemoryStore {
    MemoryStore::open_opt_in(path.clone(), true).expect("enabled store")
}

#[test]
fn a_disabled_store_touches_nothing_and_a_missing_store_is_empty() {
    let path = store_path();
    assert!(MemoryStore::open_opt_in(path.clone(), false).is_none());
    assert!(!path.exists());

    let store = enabled_store(&path);
    let load = store.load().expect("empty load");
    assert!(load.records.is_empty());
    assert!(!path.exists());
}

#[test]
fn restart_preserves_records_but_never_crosses_workspaces() {
    let path = store_path();
    let alpha = record("ws-a", "alpha fact", 10);
    let beta = record("ws-b", "beta fact", 11);
    {
        let store = enabled_store(&path);
        store.append(&alpha, 20).expect("append alpha");
        store.append(&beta, 20).expect("append beta");
    }

    let reopened = enabled_store(&path);
    let load = reopened.load().expect("reload after restart");
    assert_eq!(load.records.len(), 2);
    assert_eq!(load.skipped_corrupt, 0);

    let query = MemoryQuery::new(workspace("ws-a"), 100).expect("query");
    let admitted = query.admit(&load.records);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].content(), "alpha fact");
}

#[test]
fn corrupt_and_duplicate_lines_are_skipped_and_rewritten_clean() {
    let path = store_path();
    let valid = record("ws-a", "kept fact", 10).to_json().expect("json");
    let text =
        format!("{{\"version\":{MEMORY_STORE_VERSION}}}\nnot json at all\n{valid}\n{valid}\n");
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(&path, text).expect("write fixture");

    let store = enabled_store(&path);
    let load = store.load().expect("load with a corrupt line");
    assert_eq!(load.records.len(), 1);
    assert_eq!(load.skipped_corrupt, 1);
    assert_eq!(load.skipped_duplicate, 1);

    store
        .append(&record("ws-a", "new fact", 12), 20)
        .expect("append");
    let rewritten = store.load().expect("reload rewritten store");
    assert_eq!(rewritten.records.len(), 2);
    assert_eq!(rewritten.skipped_corrupt, 0);
    assert_eq!(rewritten.skipped_duplicate, 0);
}

#[test]
fn unsupported_store_and_record_versions_fail_closed() {
    let path = store_path();
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(&path, "{\"version\":99}\n").expect("write store");
    assert_eq!(
        enabled_store(&path).load(),
        Err(MemoryStoreError::UnsupportedVersion { version: 99 })
    );

    let newer = record("ws-a", "future fact", 10)
        .to_json()
        .expect("json")
        .replace("\"version\":1", "\"version\":2");
    let text = format!("{{\"version\":{MEMORY_STORE_VERSION}}}\n{newer}\n");
    fs::write(&path, text).expect("write store");
    let load = enabled_store(&path).load().expect("load newer record");
    assert!(load.records.is_empty());
    assert_eq!(load.skipped_unsupported, 1);
}

#[test]
fn append_replaces_the_same_id_and_prunes_expired_records() {
    let path = store_path();
    let store = enabled_store(&path);
    let kept = record("ws-a", "kept fact", 10);
    let expiring = MemoryRecord::new(
        workspace("ws-a"),
        MemoryKind::Decision,
        "short lived",
        Vec::new(),
        MemoryProvenance::new(MemorySource::HostNote, 10).expect("provenance"),
        Retention::Until { unix_seconds: 50 },
        MemorySensitivity::Normal,
        10,
    )
    .expect("expiring record");

    store.append(&kept, 20).expect("append kept");
    store.append(&kept, 20).expect("append kept again");
    store.append(&expiring, 20).expect("append expiring");
    store
        .append(&record("ws-a", "later fact", 30), 60)
        .expect("append later");

    let load = store.load().expect("reload");
    assert_eq!(load.records.len(), 2);
    assert!(load
        .records
        .iter()
        .any(|record| record.content() == "kept fact"));
    assert!(load
        .records
        .iter()
        .any(|record| record.content() == "later fact"));
}

#[test]
fn deletion_removes_only_the_requested_scope() {
    let path = store_path();
    let store = enabled_store(&path);
    let alpha = record("ws-a", "alpha fact", 10);
    let beta = record("ws-b", "beta fact", 11);
    store.append(&alpha, 20).expect("append alpha");
    store.append(&beta, 20).expect("append beta");

    let request =
        DeletionRequest::new(DeletionScope::Record(alpha.id()), 21).expect("deletion request");
    assert_eq!(store.apply_deletion(&request).expect("delete record"), 1);
    let load = store.load().expect("reload after record deletion");
    assert_eq!(load.records.len(), 1);
    assert_eq!(load.records[0].content(), "beta fact");

    let request = DeletionRequest::new(DeletionScope::Workspace(workspace("ws-b")), 22)
        .expect("workspace deletion request");
    assert_eq!(store.apply_deletion(&request).expect("delete workspace"), 1);
    assert!(store
        .load()
        .expect("reload after workspace deletion")
        .records
        .is_empty());
}
