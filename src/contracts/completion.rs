//! Provider-independent completion evidence contracts and bounds.

/// Maximum number of required checks returned by one verification call.
pub const MAX_COMPLETION_CHECKS: usize = 32;
/// Maximum number of bytes returned for a check description or evidence summary.
pub const MAX_COMPLETION_TEXT_BYTES: usize = 256;
pub const MAX_COMPLETION_ID_BYTES: usize = 64;

/// A caller-defined outcome that must be checked independently before task success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredCompletionCheck {
    pub id: String,
    pub description: String,
}

/// The result supplied by a deterministic check outside Brain/provider output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionCheckEvidenceStatus {
    Passed,
    Failed,
    Unknown,
}

/// The kind of concrete evidence used to evaluate a completion check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionEvidenceSource {
    TaskSessionObservation,
    DeterministicLocalCheck,
}

/// Evidence supplied by a Harness caller for one required check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionCheckEvidence {
    pub check_id: String,
    pub status: CompletionCheckEvidenceStatus,
    pub source: CompletionEvidenceSource,
    pub summary: String,
}

/// The outcome of a check or complete verification request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionVerificationStatus {
    Passed,
    Failed,
    Unknown,
}

/// A bounded result for one required completion check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCompletionCheck {
    pub id: String,
    pub description: String,
    pub status: CompletionVerificationStatus,
    pub evidence_source: Option<CompletionEvidenceSource>,
    pub evidence_summary: Option<String>,
}

/// Structured verification output; omitted input counts prevent truncation from passing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionVerification {
    pub status: CompletionVerificationStatus,
    pub checks: Vec<VerifiedCompletionCheck>,
    pub omitted_checks: usize,
    pub omitted_evidence: usize,
}
