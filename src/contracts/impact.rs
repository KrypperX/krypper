//! Provider-independent impact evidence contracts.

use serde::Serialize;

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
