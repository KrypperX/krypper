use super::*;

fn workspace() -> WorkspaceKey {
    WorkspaceKey::new("/workspace/krypper").unwrap()
}

fn provenance() -> MemoryProvenance {
    MemoryProvenance::new(MemorySource::HostNote, 1_000).unwrap()
}

fn record(content: &str) -> MemoryRecord {
    MemoryRecord::new(
        workspace(),
        MemoryKind::Fact,
        content,
        vec!["cargo".into()],
        provenance(),
        Retention::Permanent,
        MemorySensitivity::Normal,
        1_000,
    )
    .unwrap()
}

#[test]
fn record_round_trips_with_its_version() {
    let json = record("the repository uses a locked Cargo workspace")
        .to_json()
        .unwrap();
    assert!(json.contains(&format!("\"version\":{MEMORY_RECORD_VERSION}")));
    let decoded = MemoryRecord::from_json(&json).unwrap();
    assert_eq!(
        decoded.content(),
        "the repository uses a locked Cargo workspace"
    );
    assert_eq!(decoded.workspace(), &workspace());
    assert_eq!(decoded.kind(), MemoryKind::Fact);
}

#[test]
fn unknown_and_missing_versions_are_rejected() {
    let json = record("fact").to_json().unwrap();
    let future = json.replace(
        &format!("\"version\":{MEMORY_RECORD_VERSION}"),
        "\"version\":99",
    );
    assert_eq!(
        MemoryRecord::from_json(&future),
        Err(MemoryError::UnsupportedVersion { version: 99 })
    );
    let zero = json.replace(
        &format!("\"version\":{MEMORY_RECORD_VERSION}"),
        "\"version\":0",
    );
    assert_eq!(
        MemoryRecord::from_json(&zero),
        Err(MemoryError::UnsupportedVersion { version: 0 })
    );
}

#[test]
fn unknown_fields_and_oversized_wire_forms_are_rejected() {
    let json = record("fact").to_json().unwrap();
    let extended = json.trim_end_matches('}').to_owned() + ",\"grants\":\"all\"}";
    assert_eq!(
        MemoryRecord::from_json(&extended),
        Err(MemoryError::MalformedRecord)
    );
    let oversized = format!(
        "{{\"version\":1,\"padding\":\"{}\"}}",
        "x".repeat(MAX_MEMORY_RECORD_BYTES)
    );
    assert_eq!(
        MemoryRecord::from_json(&oversized),
        Err(MemoryError::RecordTooLarge)
    );
}

#[test]
fn secrets_can_never_be_constructed_or_decoded() {
    let error = MemoryRecord::new(
        workspace(),
        MemoryKind::Fact,
        "token sk-live-123",
        Vec::new(),
        provenance(),
        Retention::Permanent,
        MemorySensitivity::Secret,
        1_000,
    )
    .unwrap_err();
    assert_eq!(error, MemoryError::SecretBearingContent);

    let json = record("fact").to_json().unwrap();
    let forged = json.replace("\"sensitivity\":\"normal\"", "\"sensitivity\":\"secret\"");
    assert_eq!(
        MemoryRecord::from_json(&forged),
        Err(MemoryError::SecretBearingContent)
    );
}

#[test]
fn content_tags_and_workspace_keys_are_bounded() {
    assert_eq!(
        MemoryRecord::new(
            workspace(),
            MemoryKind::Fact,
            "   ",
            Vec::new(),
            provenance(),
            Retention::Permanent,
            MemorySensitivity::Normal,
            1_000,
        ),
        Err(MemoryError::EmptyContent)
    );
    assert_eq!(
        MemoryRecord::new(
            workspace(),
            MemoryKind::Fact,
            "x".repeat(MAX_MEMORY_CONTENT_BYTES + 1),
            Vec::new(),
            provenance(),
            Retention::Permanent,
            MemorySensitivity::Normal,
            1_000,
        ),
        Err(MemoryError::ContentTooLarge)
    );
    assert_eq!(
        MemoryRecord::new(
            workspace(),
            MemoryKind::Fact,
            "fact",
            vec!["same".into(), "same".into()],
            provenance(),
            Retention::Permanent,
            MemorySensitivity::Normal,
            1_000,
        ),
        Err(MemoryError::InvalidTag)
    );
    assert_eq!(
        WorkspaceKey::new("bad\nkey"),
        Err(MemoryError::InvalidWorkspaceKey)
    );
    assert_eq!(WorkspaceKey::new(""), Err(MemoryError::InvalidWorkspaceKey));
}

#[test]
fn provenance_and_retention_are_consistent() {
    assert_eq!(
        MemoryProvenance::new(MemorySource::HostNote, 0),
        Err(MemoryError::MissingProvenance)
    );
    assert_eq!(
        MemoryProvenance::new(MemorySource::ToolOutcome {
            tool: String::new()
        }, 10),
        Err(MemoryError::MissingProvenance)
    );
    let future_provenance = MemoryProvenance::new(MemorySource::HostNote, 2_000).unwrap();
    assert_eq!(
        MemoryRecord::new(
            workspace(),
            MemoryKind::Outcome,
            "ran the fixed profile",
            Vec::new(),
            future_provenance,
            Retention::Permanent,
            MemorySensitivity::Normal,
            1_000,
        ),
        Err(MemoryError::MissingProvenance)
    );
    assert_eq!(
        MemoryRecord::new(
            workspace(),
            MemoryKind::Fact,
            "fact",
            Vec::new(),
            provenance(),
            Retention::Until { unix_seconds: 1_000 },
            MemorySensitivity::Normal,
            1_000,
        ),
        Err(MemoryError::InvalidRetention)
    );
}

#[test]
fn visibility_is_workspace_scoped_and_expiry_aware() {
    let live = record("fact");
    let expiring = MemoryRecord::new(
        workspace(),
        MemoryKind::Constraint,
        "run tests offline",
        Vec::new(),
        provenance(),
        Retention::Until { unix_seconds: 2_000 },
        MemorySensitivity::Normal,
        1_000,
    )
    .unwrap();
    assert!(live.is_visible_to(&workspace(), 1_500));
    assert!(expiring.is_visible_to(&workspace(), 1_999));
    assert!(!expiring.is_visible_to(&workspace(), 2_000));
    let other = WorkspaceKey::new("/workspace/other").unwrap();
    assert!(!live.is_visible_to(&other, 1_500));
    assert!(!expiring.is_visible_to(&other, 1_500));
}

#[test]
fn query_applies_visibility_kind_and_budget_deterministically() {
    let workspace = workspace();
    let facts = record("oldest fact");
    let newest = record("newest fact");
    let constraint = MemoryRecord::new(
        workspace.clone(),
        MemoryKind::Constraint,
        "never widen tool grants",
        Vec::new(),
        MemoryProvenance::new(MemorySource::HostNote, 1_001).unwrap(),
        Retention::Permanent,
        MemorySensitivity::Normal,
        1_001,
    )
    .unwrap();
    let foreign = MemoryRecord::new(
        WorkspaceKey::new("/workspace/other").unwrap(),
        MemoryKind::Fact,
        "foreign fact",
        Vec::new(),
        provenance(),
        Retention::Permanent,
        MemorySensitivity::Normal,
        1_000,
    )
    .unwrap();
    let all = [&facts, &newest, &constraint, &foreign];
    let query = MemoryQuery::new(workspace.clone(), 1_500).unwrap();
    let admitted = query.admit(all);
    assert_eq!(admitted.len(), 3);
    assert!(admitted
        .iter()
        .all(|record| record.workspace() == &workspace));

    let constraints = MemoryQuery::new(workspace.clone(), 1_500)
        .unwrap()
        .with_kinds(vec![MemoryKind::Constraint]);
    let admitted = constraints.admit(all);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].kind(), MemoryKind::Constraint);

    let mut limited = MemoryQuery::new(workspace.clone(), 1_500).unwrap();
    limited.max_records = 1;
    assert_eq!(limited.admit(all).len(), 1);

    let mut byte_limited = MemoryQuery::new(workspace, 1_500).unwrap();
    byte_limited.max_bytes = 4;
    assert!(byte_limited.admit(all).is_empty());
}

#[test]
fn ordering_key_is_newest_evidence_first_and_stable() {
    let older = record("older");
    let newer = MemoryRecord::new(
        workspace(),
        MemoryKind::Fact,
        "newer",
        Vec::new(),
        MemoryProvenance::new(MemorySource::HostNote, 1_002).unwrap(),
        Retention::Permanent,
        MemorySensitivity::Normal,
        1_002,
    )
    .unwrap();
    let mut keyed = vec![&older, &newer];
    keyed.sort_by_key(|record| std::cmp::Reverse(record.ordering_key()));
    assert_eq!(keyed[0].content(), "newer");
    assert_eq!(older.ordering_key(), (1_000, older.id()));
}

#[test]
fn deletion_scope_never_reaches_beyond_its_request() {
    let first = record("first");
    let second = record("second");
    let foreign = MemoryRecord::new(
        WorkspaceKey::new("/workspace/other").unwrap(),
        MemoryKind::Fact,
        "foreign",
        Vec::new(),
        provenance(),
        Retention::Permanent,
        MemorySensitivity::Normal,
        1_000,
    )
    .unwrap();
    let one = DeletionRequest::new(DeletionScope::Record(first.id()), 1_500).unwrap();
    assert!(one.covers(&first));
    assert!(!one.covers(&second));

    let scoped = DeletionRequest::new(DeletionScope::Workspace(workspace()), 1_500).unwrap();
    assert!(scoped.covers(&first));
    assert!(scoped.covers(&second));
    assert!(!scoped.covers(&foreign));

    let everything = DeletionRequest::new(DeletionScope::Everything, 1_500).unwrap();
    assert!(everything.covers(&foreign));

    assert_eq!(
        DeletionRequest::new(DeletionScope::Everything, 0),
        Err(MemoryError::InvalidDeletionScope)
    );
    assert_eq!(
        MemoryQuery::new(workspace(), 0),
        Err(MemoryError::InvalidDeletionScope)
    );
}
