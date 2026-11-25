//! Bounded, advisory impact evidence for workspace plans.

use crate::brain::WorkspacePlan;
use crate::harness::execution::{workspace_mutation_target, WorkspaceAction};
use crate::task_lifecycle::TaskSession;
use crate::tools::{preview_replace_text, preview_workspace_patch, text_revision};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_IMPACT_FILES: usize = 16;
const MAX_SYMBOL_QUERIES: usize = 4;
const MAX_IMPACT_REFERENCES: usize = 64;

/// Whether bounded lexical search completed within its configured limits and search scope.
///
/// Complete means no configured limit or search error was encountered. The workspace search
/// scope still follows `.gitignore` rules and excludes `.git` metadata directories.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactCoverage {
    Complete,
    Partial,
    Unknown,
}

/// Source range of a reference found by bounded literal search.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactReference {
    pub symbol: String,
    pub path: String,
    pub line: usize,
    pub start_column: usize,
    pub end_column: usize,
    pub kind: ImpactReferenceKind,
    /// Literal search confirms this source range; semantic linkage is not parsed.
    pub confidence: ImpactEvidenceConfidence,
    /// The reference remains or may remain after the proposed plan is applied.
    pub may_remain_after_plan: bool,
}

/// Classification inferred from the reference path and source line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactReferenceKind {
    PublicApiReference,
    TestReference,
    SymbolReference,
}

/// Confidence attached to analyzer evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactEvidenceConfidence {
    ExactSourceRange,
    LexicalHeuristic,
}

/// One bounded source file affected by a proposed workspace plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactFile {
    pub path: String,
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub changed_start_line: Option<usize>,
    pub changed_end_line: Option<usize>,
    pub public_api_status: PublicApiStatus,
    pub public_api_symbols: Vec<String>,
}

/// Result of the source-level public API heuristic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicApiStatus {
    Changed,
    NoChangeDetected,
    Unknown,
    NotApplicable,
}

/// Bounded impact evidence shown to the planner before a checked plan can execute.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ImpactReport {
    pub coverage: Option<ImpactCoverage>,
    pub mutation_targets: Vec<String>,
    pub files: Vec<ImpactFile>,
    pub references: Vec<ImpactReference>,
    pub unknowns: Vec<String>,
    pub limitations: Vec<String>,
    pub truncated: bool,
}

/// A reviewed plan paired with the evidence and full revisions used for its final analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImpactCheckedPlan {
    pub(crate) plan: WorkspacePlan,
    pub(crate) report: ImpactReport,
    pub(crate) expected_revisions: BTreeMap<PathBuf, String>,
}

impl ImpactCheckedPlan {
    /// Returns the model-approved operations that Harness will execute.
    pub fn plan(&self) -> &WorkspacePlan {
        &self.plan
    }

    /// Returns the final bounded source-impact report.
    pub fn report(&self) -> &ImpactReport {
        &self.report
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ImpactAnalysisError {
    WorkspaceRead,
    UnsupportedMutation,
    TooManyFiles,
    ConflictingEdits,
    InvalidPreview,
    StaleDuringAnalysis,
}

impl fmt::Display for ImpactAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WorkspaceRead => "workspace files could not be read safely for impact analysis",
            Self::UnsupportedMutation => "impact analysis does not support this mutation type",
            Self::TooManyFiles => "impact analysis exceeded its 16-file bound",
            Self::ConflictingEdits => "a plan may mutate each path only once",
            Self::InvalidPreview => "a proposed edit could not be previewed safely",
            Self::StaleDuringAnalysis => "a mutation target changed during impact analysis",
        })
    }
}

impl std::error::Error for ImpactAnalysisError {}

pub(crate) fn analyze_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    plan: &WorkspacePlan,
) -> Result<(ImpactReport, BTreeMap<PathBuf, String>), ImpactAnalysisError> {
    let targets = unique_mutation_targets(plan)?;
    if targets.len() > MAX_IMPACT_FILES {
        return Err(ImpactAnalysisError::TooManyFiles);
    }

    let mut changed = Vec::new();
    let mut expected_revisions = BTreeMap::new();
    for action in plan.operations() {
        let Some(path) = workspace_mutation_target(action) else {
            continue;
        };
        match action {
            WorkspaceAction::ReplaceText {
                match_text,
                replacement_text,
                ..
            } => {
                let before = session
                    .read_workspace_file(task_id, turn_id, path)
                    .map_err(|_| ImpactAnalysisError::WorkspaceRead)?;
                let after = preview_replace_text(&before, match_text, replacement_text)
                    .map_err(|_| ImpactAnalysisError::InvalidPreview)?;
                expected_revisions.insert(path.clone(), text_revision(&before));
                changed.push(ChangedFile::new(path, Some(before), after));
            }
            WorkspaceAction::ApplyPatch { patch, .. } => {
                let before = session
                    .read_workspace_file(task_id, turn_id, path)
                    .map_err(|_| ImpactAnalysisError::WorkspaceRead)?;
                let relative = path
                    .to_str()
                    .ok_or(ImpactAnalysisError::UnsupportedMutation)?;
                let after = preview_workspace_patch(relative, &before, patch)
                    .map_err(|_| ImpactAnalysisError::InvalidPreview)?;
                expected_revisions.insert(path.clone(), text_revision(&before));
                changed.push(ChangedFile::new(path, Some(before), after));
            }
            WorkspaceAction::CreateFile { contents, .. } => {
                changed.push(ChangedFile::new(path, None, contents.clone()));
            }
            WorkspaceAction::CreateDirectory { .. } => {}
            _ => return Err(ImpactAnalysisError::UnsupportedMutation),
        }
    }

    let mut report = ImpactReport {
        coverage: Some(ImpactCoverage::Complete),
        mutation_targets: targets
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        ..ImpactReport::default()
    };
    let mut symbols = BTreeSet::new();
    for file in &changed {
        let (start_line, end_line, old_lines, new_lines) = changed_lines(&file.before, &file.after);
        let changed_text = old_lines.iter().chain(new_lines.iter()).copied();
        if changed_text.clone().any(|line| !line.is_ascii()) {
            report.coverage = Some(ImpactCoverage::Partial);
            report.unknowns.push(format!(
                "references in non-ASCII changed text for {} were not searched",
                file.path.display()
            ));
        }
        symbols.extend(changed_identifiers(&old_lines, &new_lines));
        let (public_api_status, public_api_symbols) = public_api_changes(&file.before, &file.after);
        if public_api_status == PublicApiStatus::Unknown {
            report.coverage = Some(ImpactCoverage::Partial);
            report.unknowns.push(format!(
                "public API status for {} is unknown",
                file.path.display()
            ));
        }
        report.files.push(ImpactFile {
            path: file.path.to_string_lossy().into_owned(),
            before_revision: file.before.as_deref().map(text_revision),
            after_revision: Some(text_revision(&file.after)),
            changed_start_line: start_line,
            changed_end_line: end_line,
            public_api_status,
            public_api_symbols,
        });
    }

    let all_candidates = symbols.into_iter().collect::<Vec<_>>();
    let omitted_short_symbols = all_candidates
        .iter()
        .filter(|symbol| symbol.len() < 3)
        .count();
    if omitted_short_symbols > 0 {
        report.coverage = Some(ImpactCoverage::Partial);
        report.unknowns.push(format!(
            "{omitted_short_symbols} short identifiers were not searched for references"
        ));
    }
    let candidates = all_candidates
        .into_iter()
        .filter(|symbol| symbol.len() >= 3)
        .collect::<Vec<_>>();
    if candidates.len() > MAX_SYMBOL_QUERIES {
        report.coverage = Some(ImpactCoverage::Partial);
        report.truncated = true;
        report.unknowns.push(format!(
            "symbol search was limited to {MAX_SYMBOL_QUERIES} identifiers; remaining references are unknown"
        ));
    }
    'symbol_search: for symbol in candidates.iter().take(MAX_SYMBOL_QUERIES) {
        let locations =
            match session.search_workspace_tree_text(task_id, turn_id, Path::new("."), symbol) {
                Ok(locations) => locations,
                Err(_) => {
                    report.coverage = Some(ImpactCoverage::Partial);
                    report.unknowns.push(format!(
                        "references to {symbol} are unknown because bounded search failed"
                    ));
                    continue;
                }
            };
        for location in locations {
            if report.references.len() == MAX_IMPACT_REFERENCES {
                report.coverage = Some(ImpactCoverage::Partial);
                report.truncated = true;
                report.unknowns.push(format!(
                    "references were limited to {MAX_IMPACT_REFERENCES}; additional locations are unknown"
                ));
                break;
            }
            let Ok(contents) = session.read_workspace_file(task_id, turn_id, &location.path) else {
                report.coverage = Some(ImpactCoverage::Partial);
                report.unknowns.push(format!(
                    "source range {}:{} changed or became unreadable during analysis",
                    location.path.display(),
                    location.line
                ));
                continue;
            };
            let Some(line) = contents.lines().nth(location.line.saturating_sub(1)) else {
                report.coverage = Some(ImpactCoverage::Partial);
                report.unknowns.push(format!(
                    "source range {}:{} became stale during analysis",
                    location.path.display(),
                    location.line
                ));
                continue;
            };
            let changed_file = changed.iter().find(|file| {
                workspace_path_comparison_key(&file.path)
                    == workspace_path_comparison_key(&location.path)
            });
            let after_line = changed_file.and_then(|file| {
                file.before
                    .as_deref()
                    .and_then(|before| corresponding_after_line(before, &file.after, location.line))
            });
            for (start_column, end_column) in identifier_ranges(line, symbol) {
                if report.references.len() == MAX_IMPACT_REFERENCES {
                    report.coverage = Some(ImpactCoverage::Partial);
                    report.truncated = true;
                    report.unknowns.push(format!(
                        "references were limited to {MAX_IMPACT_REFERENCES}; additional locations are unknown"
                    ));
                    break 'symbol_search;
                }
                report.references.push(ImpactReference {
                    symbol: symbol.clone(),
                    path: location.path.to_string_lossy().into_owned(),
                    line: location.line,
                    start_column,
                    end_column,
                    kind: reference_kind(&location.path, line),
                    confidence: ImpactEvidenceConfidence::ExactSourceRange,
                    may_remain_after_plan: match changed_file {
                        None => true,
                        Some(_) => after_line
                            .is_none_or(|after| !identifier_ranges(after, symbol).is_empty()),
                    },
                });
            }
        }
    }

    for (path, expected) in &expected_revisions {
        let current = session
            .read_workspace_file(task_id, turn_id, path)
            .map_err(|_| ImpactAnalysisError::StaleDuringAnalysis)?;
        if text_revision(&current) != *expected {
            return Err(ImpactAnalysisError::StaleDuringAnalysis);
        }
    }
    report.limitations.push(
        "references use bounded ASCII lexical searches; semantic resolution, macros and generated code are not parsed".to_owned(),
    );
    report.limitations.push(
        "workspace search respects applicable .gitignore rules and skips .git metadata directories; references in ignored paths are outside this report's scope".to_owned(),
    );
    Ok((report, expected_revisions))
}

fn unique_mutation_targets(plan: &WorkspacePlan) -> Result<BTreeSet<PathBuf>, ImpactAnalysisError> {
    let mut normalized_targets = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for path in plan
        .operations()
        .iter()
        .filter_map(workspace_mutation_target)
    {
        let normalized = normalize_workspace_relative_path(path)
            .ok_or(ImpactAnalysisError::UnsupportedMutation)?;
        if !normalized_targets.insert(workspace_path_comparison_key(&normalized)) {
            return Err(ImpactAnalysisError::ConflictingEdits);
        }
        targets.insert(path.clone());
    }
    Ok(targets)
}

struct ChangedFile {
    path: PathBuf,
    before: Option<String>,
    after: String,
}

impl ChangedFile {
    fn new(path: &Path, before: Option<String>, after: String) -> Self {
        Self {
            path: path.to_owned(),
            before,
            after,
        }
    }
}

fn changed_lines<'a>(
    before: &'a Option<String>,
    after: &'a str,
) -> (Option<usize>, Option<usize>, Vec<&'a str>, Vec<&'a str>) {
    let old = before.as_deref().unwrap_or("").lines().collect::<Vec<_>>();
    let new = after.lines().collect::<Vec<_>>();
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let old_end = old.len().saturating_sub(suffix);
    let new_end = new.len().saturating_sub(suffix);
    let old_changed = old[prefix..old_end].to_vec();
    let new_changed = new[prefix..new_end].to_vec();
    let changed_len = old_changed.len().max(new_changed.len());
    let start = (changed_len > 0).then_some(prefix + 1);
    let end = (changed_len > 0).then_some(prefix + changed_len);
    (start, end, old_changed, new_changed)
}

fn changed_identifiers(old_lines: &[&str], new_lines: &[&str]) -> BTreeSet<String> {
    let old_counts = identifier_counts(old_lines.iter().copied());
    let new_counts = identifier_counts(new_lines.iter().copied());
    old_counts
        .keys()
        .chain(new_counts.keys())
        .filter(|symbol| old_counts.get(*symbol) != new_counts.get(*symbol))
        .cloned()
        .collect()
}

fn identifier_counts<'a>(lines: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "bool", "break", "const", "continue", "crate", "else", "enum",
        "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
        "unsafe", "use", "where", "while", "String", "Option", "Result", "Some", "None",
    ];
    let mut result = BTreeMap::new();
    for line in lines {
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if !is_identifier_start(bytes[index]) {
                index += 1;
                continue;
            }
            let start = index;
            index += 1;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            let token = &line[start..index];
            if !KEYWORDS.contains(&token) {
                *result.entry(token.to_owned()).or_default() += 1;
            }
        }
    }
    result
}

fn public_api_changes(before: &Option<String>, after: &str) -> (PublicApiStatus, Vec<String>) {
    let Some(before) = before else {
        if !after.contains("pub ") {
            return (PublicApiStatus::NotApplicable, Vec::new());
        }
        return match public_declarations(after) {
            Some(declarations) if !declarations.is_empty() => (
                PublicApiStatus::Changed,
                declarations
                    .into_iter()
                    .map(|name| format!("added:{name}"))
                    .collect(),
            ),
            _ => (PublicApiStatus::Unknown, Vec::new()),
        };
    };
    if before.as_str() == after {
        return (PublicApiStatus::NoChangeDetected, Vec::new());
    }
    if !before.contains("pub ") && !after.contains("pub ") {
        return (PublicApiStatus::NoChangeDetected, Vec::new());
    }
    let old = public_declarations(before);
    let new = public_declarations(after);
    if old.is_none() || new.is_none() {
        return (PublicApiStatus::Unknown, Vec::new());
    }
    let old = old.unwrap_or_default();
    let new = new.unwrap_or_default();
    let changes = old
        .difference(&new)
        .map(|name| format!("removed:{name}"))
        .chain(new.difference(&old).map(|name| format!("added:{name}")))
        .collect::<Vec<_>>();
    if changes.is_empty() {
        // Names alone cannot prove API compatibility: parameters, return types, fields and
        // enum variants can change while the item name remains stable. Keep this unknown so
        // the caller routes the report through model review.
        (PublicApiStatus::Unknown, changes)
    } else {
        (PublicApiStatus::Changed, changes)
    }
}

fn normalize_workspace_relative_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::Prefix(_) | std::path::Component::RootDir => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn workspace_path_comparison_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        // Windows paths are case-insensitive. Lowercase only the comparison key, preserving the
        // caller's original path for all reads and writes. Lossy conversion errs on the safe side
        // by rejecting potentially equivalent non-Unicode paths as duplicates.
        PathBuf::from(path.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn public_declarations(contents: &str) -> Option<BTreeSet<String>> {
    let mut declarations = BTreeSet::new();
    for line in contents
        .lines()
        .filter(|line| line.trim_start().starts_with("pub "))
    {
        let words = line
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let kind_index = words.iter().position(|word| {
            matches!(
                *word,
                "fn" | "struct" | "enum" | "trait" | "type" | "const" | "static" | "mod"
            )
        })?;
        let name = words.get(kind_index + 1)?;
        declarations.insert((*name).to_owned());
    }
    Some(declarations)
}

fn reference_kind(path: &Path, line: &str) -> ImpactReferenceKind {
    if path.components().any(|part| part.as_os_str() == "tests")
        || path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains("test"))
    {
        ImpactReferenceKind::TestReference
    } else if line.trim_start().starts_with("pub ") {
        ImpactReferenceKind::PublicApiReference
    } else {
        ImpactReferenceKind::SymbolReference
    }
}

fn identifier_ranges(line: &str, symbol: &str) -> Vec<(usize, usize)> {
    let mut search_from = 0;
    let mut ranges = Vec::new();
    while let Some(offset) = line[search_from..].find(symbol) {
        let start = search_from + offset;
        let end = start + symbol.len();
        let before_ok = start == 0 || !is_identifier_continue(line.as_bytes()[start - 1]);
        let after_ok = end == line.len() || !is_identifier_continue(line.as_bytes()[end]);
        if before_ok && after_ok {
            ranges.push((start + 1, end + 1));
        }
        search_from = end;
        if search_from >= line.len() {
            break;
        }
    }
    ranges
}

fn corresponding_after_line<'a>(before: &str, after: &'a str, line: usize) -> Option<&'a str> {
    let before_lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();
    let index = line.checked_sub(1)?;
    if index >= before_lines.len() {
        return None;
    }
    let prefix = before_lines
        .iter()
        .zip(&after_lines)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = before_lines[prefix..]
        .iter()
        .rev()
        .zip(after_lines[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let old_changed_end = before_lines.len().saturating_sub(suffix);
    let new_changed_end = after_lines.len().saturating_sub(suffix);
    let after_index = if index < prefix {
        index
    } else if index >= old_changed_end {
        after_lines
            .len()
            .checked_sub(before_lines.len().saturating_sub(index))?
    } else if old_changed_end - prefix == new_changed_end - prefix {
        prefix + (index - prefix)
    } else {
        return None;
    };
    after_lines.get(after_index).copied()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::WorkspaceAction;
    use crate::harness::execution::{
        run_goal_turn_with_impact_checked_plan, GoalTurnStopReason, WorkspaceObservation,
    };
    use crate::task_lifecycle::TurnOutcome;
    use crate::task_lifecycle::{TaskState, TurnState};
    use std::fs;

    struct TempWorkspace(PathBuf);

    impl TempWorkspace {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("krypper-impact-analysis-{}", Uuid::new_v4()));
            fs::create_dir_all(&root).expect("create fixture workspace");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove fixture workspace");
        }
    }

    fn running_session(workspace: &Path) -> (TaskSession, Uuid, Uuid) {
        let mut session = TaskSession::new_in_workspace(workspace).expect("bind workspace");
        let task = session.create_task("inspect source impact");
        let task_id = task.id();
        task.transition(TaskState::Running).expect("start task");
        let turn_id = task.create_turn().expect("create turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("start turn");
        (session, task_id, turn_id)
    }

    #[test]
    fn analysis_reports_source_range_test_reference_and_full_revision() {
        let workspace = TempWorkspace::new();
        fs::create_dir_all(workspace.path().join("src")).expect("create src");
        fs::create_dir_all(workspace.path().join("tests")).expect("create tests");
        fs::write(workspace.path().join("src/lib.rs"), "pub fn old_api() {}\n")
            .expect("write source");
        fs::write(
            workspace.path().join("tests/api.rs"),
            "fn check() { old_api(); }\n",
        )
        .expect("write test reference");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![WorkspaceAction::ReplaceText {
                path: PathBuf::from("src/lib.rs"),
                match_text: "old_api".to_owned(),
                replacement_text: "new_api".to_owned(),
            }],
            1,
        )
        .expect("valid plan");

        let (report, revisions) = analyze_plan(&session, task_id, turn_id, &plan)
            .expect("analyze bounded source and test references");

        assert_eq!(report.files[0].public_api_status, PublicApiStatus::Changed);
        assert_eq!(report.files[0].changed_start_line, Some(1));
        assert!(report.references.iter().any(|reference| {
            reference.path == "tests/api.rs"
                && reference.symbol == "old_api"
                && reference.kind == ImpactReferenceKind::TestReference
                && reference.line == 1
        }));
        assert_eq!(revisions[Path::new("src/lib.rs")].len(), 64);
        assert_eq!(report.coverage, Some(ImpactCoverage::Complete));
        assert!(!report.limitations.is_empty());
        assert!(report.unknowns.is_empty());
    }

    #[test]
    fn analysis_rejects_multiple_edits_to_one_path_before_mutation() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("file.rs"), "old name\n").expect("write source");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("file.rs"),
                    match_text: "old".to_owned(),
                    replacement_text: "new".to_owned(),
                },
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("file.rs"),
                    match_text: "name".to_owned(),
                    replacement_text: "symbol".to_owned(),
                },
            ],
            2,
        )
        .expect("valid bounded plan");

        assert_eq!(
            analyze_plan(&session, task_id, turn_id, &plan),
            Err(ImpactAnalysisError::ConflictingEdits)
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.rs")).unwrap(),
            "old name\n"
        );
    }

    #[test]
    fn analysis_rejects_lexical_path_aliases_before_mutation() {
        let workspace = TempWorkspace::new();
        fs::create_dir(workspace.path().join("src")).expect("create source directory");
        fs::write(workspace.path().join("file.rs"), "old name\n").expect("write source");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("file.rs"),
                    match_text: "old".to_owned(),
                    replacement_text: "new".to_owned(),
                },
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("src/../file.rs"),
                    match_text: "name".to_owned(),
                    replacement_text: "symbol".to_owned(),
                },
            ],
            2,
        )
        .expect("valid bounded plan");

        assert_eq!(
            analyze_plan(&session, task_id, turn_id, &plan),
            Err(ImpactAnalysisError::ConflictingEdits)
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.rs")).unwrap(),
            "old name\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn duplicate_case_equivalent_targets_are_rejected_on_windows() {
        let plan = WorkspacePlan::try_from_actions(
            vec![
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("Foo.rs"),
                    match_text: "old".to_owned(),
                    replacement_text: "new".to_owned(),
                },
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("foo.rs"),
                    match_text: "name".to_owned(),
                    replacement_text: "symbol".to_owned(),
                },
            ],
            2,
        )
        .expect("valid bounded plan");

        assert_eq!(
            unique_mutation_targets(&plan),
            Err(ImpactAnalysisError::ConflictingEdits)
        );
    }

    #[test]
    fn analysis_reports_ignored_path_search_scope() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("source.rs"), "fn old_name() {}\n").expect("write source");
        fs::write(workspace.path().join(".gitignore"), "ignored.rs\n").expect("write ignore rules");
        fs::write(
            workspace.path().join("ignored.rs"),
            "fn caller() { old_name(); }\n",
        )
        .expect("write ignored reference");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![WorkspaceAction::ReplaceText {
                path: PathBuf::from("source.rs"),
                match_text: "old_name".to_owned(),
                replacement_text: "new_name".to_owned(),
            }],
            1,
        )
        .expect("valid bounded plan");

        let (report, _) = analyze_plan(&session, task_id, turn_id, &plan)
            .expect("analyze bounded source and visible workspace paths");

        assert_eq!(report.coverage, Some(ImpactCoverage::Complete));
        assert!(!report
            .references
            .iter()
            .any(|reference| reference.path == "ignored.rs"));
        assert!(report.limitations.iter().any(|limitation| {
            limitation.contains(".gitignore") && limitation.contains("ignored paths")
        }));
    }

    #[test]
    fn analysis_caps_reference_ranges_within_a_line() {
        let workspace = TempWorkspace::new();
        let before = format!("fn old_name() {{ {} }}\n", "old_name(); ".repeat(70));
        fs::write(workspace.path().join("source.rs"), &before).expect("write source");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![WorkspaceAction::ReplaceText {
                path: PathBuf::from("source.rs"),
                match_text: "fn old_name".to_owned(),
                replacement_text: "fn new_name".to_owned(),
            }],
            1,
        )
        .expect("valid bounded plan");

        let (report, _) = analyze_plan(&session, task_id, turn_id, &plan)
            .expect("analyze bounded source references");

        assert_eq!(report.references.len(), MAX_IMPACT_REFERENCES);
        assert_eq!(report.coverage, Some(ImpactCoverage::Partial));
        assert!(report.truncated);
    }

    #[test]
    fn public_signature_and_membership_changes_remain_unknown_for_review() {
        for (before, after) in [
            (
                "pub fn parse(input: &str) -> usize;\n",
                "pub fn parse(input: Path) -> usize;\n",
            ),
            (
                "pub struct Config { pub name: String }\n",
                "pub struct Config { pub key: String }\n",
            ),
            (
                "pub enum State { Ready }\n",
                "pub enum State { Ready, Broken }\n",
            ),
        ] {
            assert_eq!(
                public_api_changes(&Some(before.to_owned()), after).0,
                PublicApiStatus::Unknown
            );
        }
    }

    #[test]
    fn short_or_unicode_changed_identifiers_are_reported_as_unknown() {
        for (before, after) in [
            ("fn go() { x(); }\n", "fn go() { y(); }\n"),
            ("fn café() {}\n", "fn café_new() {}\n"),
        ] {
            let workspace = TempWorkspace::new();
            fs::write(workspace.path().join("source.rs"), before).expect("write source");
            let (session, task_id, turn_id) = running_session(workspace.path());
            let plan = WorkspacePlan::try_from_actions(
                vec![WorkspaceAction::ReplaceText {
                    path: PathBuf::from("source.rs"),
                    match_text: before.trim_end().to_owned(),
                    replacement_text: after.trim_end().to_owned(),
                }],
                1,
            )
            .expect("bounded plan");

            let (report, _) =
                analyze_plan(&session, task_id, turn_id, &plan).expect("analyze changed text");

            assert_eq!(report.coverage, Some(ImpactCoverage::Partial));
            assert!(!report.unknowns.is_empty());
        }
    }

    #[test]
    fn analysis_detects_public_api_added_by_a_new_file() {
        let workspace = TempWorkspace::new();
        let (session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![WorkspaceAction::CreateFile {
                path: PathBuf::from("src/new_api.rs"),
                contents: "pub fn new_api() {}\n".to_owned(),
            }],
            1,
        )
        .expect("bounded plan");

        let (report, _) =
            analyze_plan(&session, task_id, turn_id, &plan).expect("analyze new file public API");

        assert_eq!(report.files[0].public_api_status, PublicApiStatus::Changed);
        assert!(report.files[0]
            .public_api_symbols
            .contains(&"added:new_api".to_owned()));
    }

    #[test]
    fn atomic_replace_rejects_a_revision_that_changed_after_analysis() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("file.rs"), "before\n").expect("write source");
        let (session, task_id, turn_id) = running_session(workspace.path());
        let expected = text_revision("before\n");
        fs::write(workspace.path().join("file.rs"), "external edit\n")
            .expect("change source after analysis");

        let result = session.replace_workspace_text_with_revision(
            task_id,
            turn_id,
            Path::new("file.rs"),
            "external",
            "agent",
            Some(&expected),
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.rs")).unwrap(),
            "external edit\n"
        );
    }

    #[test]
    fn denied_path_fails_preflight_without_touching_safe_targets() {
        let workspace = TempWorkspace::new();
        let task_workspace = workspace.path().join("workspace");
        fs::create_dir(&task_workspace).expect("create task workspace");
        fs::write(task_workspace.join("safe.rs"), "old\n").expect("write safe source");
        let outside = workspace.path().join("outside.rs");
        fs::write(&outside, "outside\n").expect("write outside sentinel");
        let (session, task_id, turn_id) = running_session(&task_workspace);
        let plan = WorkspacePlan::try_from_actions(
            vec![
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("safe.rs"),
                    match_text: "old".to_owned(),
                    replacement_text: "new".to_owned(),
                },
                WorkspaceAction::ReplaceText {
                    path: PathBuf::from("../outside.rs"),
                    match_text: "outside".to_owned(),
                    replacement_text: "changed".to_owned(),
                },
            ],
            2,
        )
        .expect("bounded plan");

        assert_eq!(
            analyze_plan(&session, task_id, turn_id, &plan),
            Err(ImpactAnalysisError::UnsupportedMutation)
        );
        assert_eq!(
            fs::read_to_string(task_workspace.join("safe.rs")).unwrap(),
            "old\n"
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside\n");
    }

    #[test]
    fn stale_checked_plan_fails_before_any_planned_mutation() {
        let workspace = TempWorkspace::new();
        fs::write(workspace.path().join("source.rs"), "old_name\n").expect("write source");
        let (mut session, task_id, turn_id) = running_session(workspace.path());
        let plan = WorkspacePlan::try_from_actions(
            vec![WorkspaceAction::ReplaceText {
                path: PathBuf::from("source.rs"),
                match_text: "old_name".to_owned(),
                replacement_text: "new_name".to_owned(),
            }],
            1,
        )
        .expect("bounded plan");
        let (report, expected_revisions) =
            analyze_plan(&session, task_id, turn_id, &plan).expect("initial impact analysis");
        let checked = ImpactCheckedPlan {
            plan,
            report,
            expected_revisions,
        };
        fs::write(workspace.path().join("source.rs"), "concurrent edit\n")
            .expect("change source after analysis");
        let mut control =
            |_goal: &str, _observations: &[WorkspaceObservation]| WorkspaceAction::Complete {
                result: "must not complete".to_owned(),
            };

        let result = run_goal_turn_with_impact_checked_plan(
            &mut session,
            task_id,
            turn_id,
            "rename source",
            2,
            Ok(checked),
            &mut control,
        )
        .expect("record stale precondition as a failed turn");

        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert!(matches!(
            result.outcome(),
            Some(TurnOutcome::Failure { .. })
        ));
        assert_eq!(
            fs::read_to_string(workspace.path().join("source.rs")).unwrap(),
            "concurrent edit\n"
        );
    }
}
