//! Deterministic, measurable recall fixtures (#219 PR 14).
//!
//! Evaluation is offline and clock-free: each case supplies its own records, query and gold ids,
//! so the same inputs always produce the same report. Reported numbers are counts and basis
//! points, not claims about model behaviour.

use super::record::MemoryRecord;
use super::retrieval::collect_memory_context;
use super::scope::MemoryQuery;
use std::collections::BTreeMap;
use uuid::Uuid;

/// One held-out fixture: a workspace query, its candidate records and the ids that should recall.
#[derive(Clone, Debug)]
pub struct EvaluationCase {
    pub name: String,
    pub records: Vec<MemoryRecord>,
    pub query: MemoryQuery,
    pub gold_ids: Vec<Uuid>,
}

impl EvaluationCase {
    pub fn new(
        name: impl Into<String>,
        records: Vec<MemoryRecord>,
        query: MemoryQuery,
        gold_ids: Vec<Uuid>,
    ) -> Self {
        Self {
            name: name.into(),
            records,
            query,
            gold_ids,
        }
    }
}

/// Aggregate recall measurements. Zero leaks and zero expired recalls are the expected result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EvaluationReport {
    pub cases: usize,
    pub recalled: usize,
    pub gold: usize,
    pub hits: usize,
    pub bytes: usize,
    /// Recalled records that belong to another workspace.
    pub cross_workspace_leaks: usize,
    /// Recalled records that were expired at query time.
    pub expired_recalls: usize,
    /// Cases where the render budget stopped recall early.
    pub truncated_cases: usize,
}

impl EvaluationReport {
    /// Shared ids over recalled ids, in basis points.
    pub fn precision_basis_points(&self) -> u32 {
        ratio_basis_points(self.hits, self.recalled)
    }

    /// Shared ids over gold ids, in basis points.
    pub fn recall_basis_points(&self) -> u32 {
        ratio_basis_points(self.hits, self.gold)
    }

    /// True only when recall never crossed a workspace or served expired data.
    pub fn isolation_holds(&self) -> bool {
        self.cross_workspace_leaks == 0 && self.expired_recalls == 0
    }
}

fn ratio_basis_points(numerator: usize, denominator: usize) -> u32 {
    if denominator == 0 {
        return 0;
    }
    u32::try_from(numerator.saturating_mul(10_000) / denominator).unwrap_or(u32::MAX)
}

/// Runs every case and aggregates the measurements. No model is called.
pub fn evaluate(cases: &[EvaluationCase]) -> EvaluationReport {
    let mut report = EvaluationReport::default();
    for case in cases {
        report.cases += 1;
        report.gold += case.gold_ids.len();
        let context = collect_memory_context(&case.records, &case.query);
        if context.truncated {
            report.truncated_cases += 1;
        }
        let by_id: BTreeMap<Uuid, &MemoryRecord> = case
            .records
            .iter()
            .map(|record| (record.id(), record))
            .collect();
        for excerpt in &context.excerpts {
            report.recalled += 1;
            report.bytes += serde_json::to_string(excerpt).map_or(0, |encoded| encoded.len());
            let Ok(id) = Uuid::parse_str(&excerpt.id) else {
                continue;
            };
            if case.gold_ids.contains(&id) {
                report.hits += 1;
            }
            if let Some(record) = by_id.get(&id) {
                if record.workspace() != &case.query.workspace {
                    report.cross_workspace_leaks += 1;
                }
                if !record.is_live_at(case.query.now_unix) {
                    report.expired_recalls += 1;
                }
            }
        }
    }
    report
}
