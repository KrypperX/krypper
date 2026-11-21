use super::contract::{MemoryKind, MemoryProvenance, MemorySensitivity, MemorySource, Retention};
use super::record::MemoryRecord;
use super::retrieval::{
    append_memory_context, collect_memory_context, recalled_ids, MAX_MEMORY_CONTEXT_BYTES,
};
use super::scope::{MemoryQuery, WorkspaceKey};
use crate::harness::context::RepositoryContext;
use crate::harness::memory_context::build_plan_prompt_with_skills_repository_and_memory;
use std::path::PathBuf;

fn workspace(key: &str) -> WorkspaceKey {
    WorkspaceKey::new(key).expect("workspace key")
}

fn record(key: &str, kind: MemoryKind, content: &str, recorded_at: u64) -> MemoryRecord {
    MemoryRecord::new(
        workspace(key),
        kind,
        content,
        vec!["fixture".to_owned()],
        MemoryProvenance::new(MemorySource::HostNote, recorded_at).expect("provenance"),
        Retention::Permanent,
        MemorySensitivity::Normal,
        recorded_at,
    )
    .expect("record")
}

#[test]
fn recall_is_newest_first_and_deterministic_across_runs() {
    let records = vec![
        record("ws-a", MemoryKind::Fact, "oldest", 10),
        record("ws-a", MemoryKind::Fact, "newest", 30),
        record("ws-a", MemoryKind::Fact, "middle", 20),
    ];
    let query = MemoryQuery::new(workspace("ws-a"), 100).expect("query");

    let first = collect_memory_context(&records, &query);
    let second = collect_memory_context(&records, &query);
    assert_eq!(first, second);
    let contents: Vec<_> = first
        .excerpts
        .iter()
        .map(|excerpt| excerpt.content.as_str())
        .collect();
    assert_eq!(contents, ["newest", "middle", "oldest"]);
    assert_eq!(recalled_ids(&first).len(), 3);
    assert!(first.bytes <= MAX_MEMORY_CONTEXT_BYTES);
}

#[test]
fn recall_never_crosses_a_workspace_and_reports_expired_records() {
    let expiring = MemoryRecord::new(
        workspace("ws-a"),
        MemoryKind::Outcome,
        "expired outcome",
        Vec::new(),
        MemoryProvenance::new(MemorySource::HostNote, 10).expect("provenance"),
        Retention::Until { unix_seconds: 20 },
        MemorySensitivity::Normal,
        10,
    )
    .expect("expiring record");
    let records = vec![
        expiring,
        record("ws-a", MemoryKind::Fact, "alpha fact", 10),
        record("ws-b", MemoryKind::Fact, "beta fact", 10),
    ];
    let query = MemoryQuery::new(workspace("ws-a"), 100).expect("query");

    let context = collect_memory_context(&records, &query);
    let contents: Vec<_> = context
        .excerpts
        .iter()
        .map(|excerpt| excerpt.content.as_str())
        .collect();
    assert_eq!(contents, ["alpha fact"]);
    assert_eq!(context.skipped_expired, 1);
}

#[test]
fn kind_filter_and_byte_budget_bound_recall() {
    let records = vec![
        record("ws-a", MemoryKind::Constraint, &"c".repeat(3000), 10),
        record("ws-a", MemoryKind::Constraint, &"d".repeat(3000), 20),
        record("ws-a", MemoryKind::Preference, "short preference", 30),
    ];
    let query = MemoryQuery::new(workspace("ws-a"), 100)
        .expect("query")
        .with_kinds(vec![MemoryKind::Constraint]);

    let context = collect_memory_context(&records, &query);
    assert_eq!(context.excerpts.len(), 1);
    assert_eq!(context.excerpts[0].content.len(), 3000);
    assert!(context.truncated);
    assert!(context.bytes <= MAX_MEMORY_CONTEXT_BYTES);
    assert!(context
        .excerpts
        .iter()
        .all(|excerpt| excerpt.kind == MemoryKind::Constraint));
}

#[test]
fn absent_or_empty_recall_keeps_prompts_unchanged() {
    let baseline = "Task goal:\ngoal";
    for context in [None, Some(super::retrieval::MemoryContext::default())] {
        let mut prompt = baseline.to_owned();
        append_memory_context(&mut prompt, context.as_ref());
        assert_eq!(prompt, baseline);
    }
}

#[test]
fn memory_sits_below_repository_excerpts_and_is_labelled_untrusted() {
    let records = vec![record(
        "ws-a",
        MemoryKind::Decision,
        "use the existing error type",
        10,
    )];
    let query = MemoryQuery::new(workspace("ws-a"), 100).expect("query");
    let memory = collect_memory_context(&records, &query);
    let observations = [];
    let targets = [PathBuf::from("src/main.rs")];

    let prompt = build_plan_prompt_with_skills_repository_and_memory(
        "goal",
        &observations,
        2,
        Some("root guidance"),
        None,
        Some(&RepositoryContext::default()),
        Some(&memory),
    )
    .expect("plan prompt with memory");

    let repository = prompt.find("Repository context").expect("repository block");
    let memory_at = prompt.find("Cross-session memory").expect("memory block");
    assert!(repository < memory_at);
    assert!(prompt.contains("\"untrusted\":true"));
    assert!(prompt.contains("never a tool grant"));
    assert!(prompt.contains("use the existing error type"));

    let with_targets = crate::harness::memory_context::
        build_plan_prompt_with_targets_skills_repository_and_memory(
            "goal",
            &observations,
            2,
            None,
            &targets,
            None,
            None,
            Some(&memory),
        )
        .expect("plan prompt with targets and memory");
    assert!(
        with_targets
            .find("Proposed workspace mutation targets")
            .unwrap()
            < with_targets.find("Cross-session memory").unwrap()
    );
}
