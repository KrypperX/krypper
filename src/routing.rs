//! Shared, deterministic model selection policies.
//!
//! Routing consumes caller-provided candidates and never fetches a catalog or
//! grants credentials, network access, tools, or execution authority. Callers
//! opt into automatic selection by constructing [`RoutingRequest::Automatic`].

use crate::catalog_data::ModelSpec;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum CapabilityName {
    /// A model can return a decision accepted by the selected adapter.
    DecisionRequest,
    /// A model can use the provider's native decision stream.
    NativeStreaming,
    /// A model can issue tool calls.
    ToolCalling,
    /// A model advertises reasoning support.
    Reasoning,
    /// A model accepts image input.
    ImageInput,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    /// The available evidence says the capability works.
    Supported,
    /// The available evidence says the capability does not work.
    Unsupported,
    /// No usable evidence establishes support or lack of support.
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProvenance {
    /// The provider descriptor or model catalog declares the capability.
    Declared,
    /// A probe or runtime check exercised this specific capability.
    Verified,
    /// A runtime signal was observed but does not verify the capability.
    Observed,
}

/// One capability fact with its support and evidence source kept distinct.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityAssertion {
    /// Capability this assertion describes.
    pub name: CapabilityName,
    /// Support value; unknown remains distinct from unsupported.
    pub support: CapabilitySupport,
    /// Source that produced the assertion.
    pub provenance: CapabilityProvenance,
}

impl CapabilityAssertion {
    /// Construct a capability assertion without interpreting unknown as false.
    pub const fn new(
        name: CapabilityName,
        support: CapabilitySupport,
        provenance: CapabilityProvenance,
    ) -> Self {
        Self {
            name,
            support,
            provenance,
        }
    }
}

/// Freshness of the capability facts attached to a routing candidate.
///
/// Provider verification and catalog freshness are separate evidence. A fresh
/// provider probe cannot make stale or unavailable model capability claims
/// suitable for automatic selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilityEvidenceFreshness {
    /// Capability facts come from a current trusted source or fresh evidence record.
    Fresh,
    /// Capability facts come from a source known to be out of date.
    Stale,
    /// Capability fact freshness is unavailable or has not been checked.
    Unknown,
}

/// Current state of provider and model verification for one configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilityVerification {
    /// No explicit verification is available.
    Unknown,
    /// A recent probe succeeded for this provider, model, and configuration.
    Verified,
    /// The latest probe failed.
    Failed,
    /// The latest probe is too old to use for automatic selection.
    Stale,
}

/// Provider readiness supplied to the router by the caller.
///
/// This is selection evidence only. A selected route must still pass through
/// the ordinary credential resolver, adapter registry, and request checks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderReadiness {
    /// The provider is known to this caller.
    pub discoverable: bool,
    /// A trusted adapter is registered for the provider's protocol.
    pub executable: bool,
    /// Required configuration and credentials are present for this account.
    pub configured: bool,
    /// Freshness and result of an explicit provider/model probe.
    pub verification: CapabilityVerification,
}

/// Preference order used to rank otherwise eligible automatic candidates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SelectionPreference {
    /// Prefer the lowest caller-computed estimated request cost.
    LowestCost,
    /// Prefer the lowest latency from fresh runtime evidence.
    LowestLatency,
    /// Prefer the highest caller-supplied quality score.
    HighestQuality,
}

/// Explicit model and provider, or a request to use automatic routing.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RoutingRequest {
    /// Preserve the caller's manual choice, including model IDs absent from catalogs.
    Manual {
        /// Explicit provider selector, if one was supplied.
        provider: Option<String>,
        /// Explicit model ID.
        model: String,
    },
    /// Rank the supplied candidates under task requirements and preferences.
    Automatic(AutomaticRoutingRequest),
}

/// Requirements and preferences for an explicitly requested automatic route.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticRoutingRequest {
    /// Capabilities required by the task.
    pub required_capabilities: Vec<CapabilityName>,
    /// Minimum model context window, when the caller knows it.
    pub minimum_context_window_tokens: Option<u64>,
    /// Minimum output-token limit, when the caller needs one.
    pub minimum_output_tokens: Option<u64>,
    /// Maximum estimated total request cost, in micro-USD. Unknown cost cannot pass this budget.
    pub maximum_estimated_cost_micro_usd: Option<u64>,
    /// Ordered ranking preferences. Earlier preferences take priority.
    pub preferences: Vec<SelectionPreference>,
}

/// A configured provider/model candidate with metadata and evidence supplied by the caller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingCandidate {
    /// Executable provider selector or configured profile ID.
    pub provider: String,
    /// Exact catalog record for this model.
    pub model: ModelSpec,
    /// Readiness for the provider/model and current configuration revision.
    pub readiness: ProviderReadiness,
    /// Declared, verified, or observed capability facts for this candidate.
    pub capabilities: Vec<CapabilityAssertion>,
    /// Freshness of `capabilities`, independent of provider probe readiness.
    pub capability_evidence_freshness: CapabilityEvidenceFreshness,
    /// Caller-computed estimated total cost for this request, in micro-USD.
    /// `None` means unknown; the router never interprets it as zero.
    pub estimated_cost_micro_usd: Option<u64>,
    /// Latency from fresh runtime evidence, in milliseconds.
    pub observed_latency_ms: Option<u64>,
    /// Optional quality score in basis points from 0 through 10,000.
    pub quality_score_basis_points: Option<u16>,
}

/// Why an automatic route selected the winning candidate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SelectionReasonCode {
    /// A manual provider/model selection bypassed automatic ranking.
    ManualOverride,
    /// Exactly one candidate passed all requirements.
    OnlyEligibleCandidate,
    /// The first preference that separated the winner was lowest cost.
    LowestCostPreference,
    /// The first preference that separated the winner was lowest latency.
    LowestLatencyPreference,
    /// The first preference that separated the winner was highest quality.
    HighestQualityPreference,
    /// Preferences tied or were absent, so the stable provider/model key won.
    StableTieBreak,
}

impl SelectionReasonCode {
    /// Stable identifier for logs, CLI output, and UI integrations.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManualOverride => "manual_override",
            Self::OnlyEligibleCandidate => "only_eligible_candidate",
            Self::LowestCostPreference => "lowest_cost_preference",
            Self::LowestLatencyPreference => "lowest_latency_preference",
            Self::HighestQualityPreference => "highest_quality_preference",
            Self::StableTieBreak => "stable_tie_break",
        }
    }
}

/// Why an automatic candidate was rejected.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum CandidateRejectionCode {
    /// The provider is not discoverable to the caller.
    ProviderNotDiscoverable,
    /// No trusted adapter is executable for the provider protocol.
    AdapterUnavailable,
    /// Required account configuration or credentials are missing.
    CredentialsMissing,
    /// A fresh successful probe is required before automatic selection.
    VerificationRequired,
    /// The latest probe failed.
    ProbeFailed,
    /// The latest probe is stale.
    EvidenceStale,
    /// A required capability is explicitly unsupported.
    CapabilityUnsupported(CapabilityName),
    /// A required capability has no usable support claim.
    CapabilityUnknown(CapabilityName),
    /// An observed signal is not sufficient to verify a required capability.
    CapabilityUnverified(CapabilityName),
    /// The model context limit is missing while the request requires one.
    ContextWindowUnknown,
    /// The model context limit is too small for the request.
    ContextWindowTooSmall,
    /// The model output limit is missing while the request requires one.
    OutputLimitUnknown,
    /// The model output limit is too small for the request.
    OutputLimitTooSmall,
    /// Request cost is unknown while a cost budget is active.
    CostUnknown,
    /// Estimated request cost exceeds the caller's budget.
    OverBudget,
    /// The quality estimate is outside the supported range.
    InvalidQualityEstimate,
    /// More than one candidate has this provider/model identity.
    DuplicateCandidate,
    /// Multiple conflicting or duplicate assertions describe a required capability.
    DuplicateCapabilityEvidence(CapabilityName),
    /// A required capability is based on stale source data.
    CapabilityEvidenceStale(CapabilityName),
    /// Freshness of a required capability's source has not been established.
    CapabilityEvidenceFreshnessUnknown(CapabilityName),
}

/// Rejection details for one provider/model candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRejection {
    /// Provider selector or configured profile ID.
    pub provider: String,
    /// Model ID.
    pub model: String,
    /// All applicable rejection codes in stable order.
    pub reasons: Vec<CandidateRejectionCode>,
}

/// Why the router could not select an automatic candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NoCandidateReasonCode {
    /// The caller supplied no candidates.
    NoCandidatesProvided,
    /// Candidates were supplied, but each failed at least one requirement.
    AllCandidatesRejected,
    /// A policy received a request mode it does not implement.
    RouterModeMismatch,
}

/// A reason-coded automatic selection failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoCandidate {
    /// Stable top-level reason.
    pub reason: NoCandidateReasonCode,
    /// Per-candidate details, sorted by provider and model ID.
    pub rejected: Vec<CandidateRejection>,
}

/// The result of manual or automatic model selection.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RoutingDecision {
    /// One provider/model selection and its reason code.
    Selected(RoutingSelection),
    /// No candidate satisfied the request.
    NoCandidate(NoCandidate),
}

/// Selected provider/model and the reason that won.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingSelection {
    /// Selected provider selector; manual selection may leave it unset.
    pub provider: Option<String>,
    /// Selected model ID.
    pub model: String,
    /// Stable reason code suitable for explanation or machine output.
    pub reason: SelectionReasonCode,
}

/// Replaceable policy for manual passthrough or automatic candidate ranking.
pub trait ModelRouter: Send + Sync {
    /// Select a provider/model or return reason-coded candidate rejections.
    fn route(&self, request: &RoutingRequest, candidates: &[RoutingCandidate]) -> RoutingDecision;

    /// Return a stable, non-secret identifier for this strategy and its behavior.
    ///
    /// A durable task that may use automatic fallback must persist this value
    /// and return the same value after restart. Change it when strategy behavior
    /// or behavior-affecting configuration changes. Return `None` when the
    /// strategy cannot provide a task-stable identity.
    fn task_stable_id(&self) -> Option<String> {
        None
    }
}

/// Manual policy that preserves the exact explicit provider/model choice.
#[derive(Clone, Copy, Debug, Default)]
pub struct ManualModelRouter;

impl ModelRouter for ManualModelRouter {
    fn route(&self, request: &RoutingRequest, _candidates: &[RoutingCandidate]) -> RoutingDecision {
        match request {
            RoutingRequest::Manual { provider, model } => {
                RoutingDecision::Selected(RoutingSelection {
                    provider: provider.clone(),
                    model: model.clone(),
                    reason: SelectionReasonCode::ManualOverride,
                })
            }
            RoutingRequest::Automatic(_) => mode_mismatch(),
        }
    }

    fn task_stable_id(&self) -> Option<String> {
        Some("manual-model-router-v1".to_owned())
    }
}

/// Built-in deterministic policy for explicit automatic requests.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicModelRouter;

impl ModelRouter for DeterministicModelRouter {
    fn route(&self, request: &RoutingRequest, candidates: &[RoutingCandidate]) -> RoutingDecision {
        let RoutingRequest::Automatic(request) = request else {
            return mode_mismatch();
        };
        self.route_automatic(request, candidates)
    }

    fn task_stable_id(&self) -> Option<String> {
        Some("deterministic-model-router-v1".to_owned())
    }
}

impl DeterministicModelRouter {
    fn route_automatic(
        &self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
    ) -> RoutingDecision {
        if candidates.is_empty() {
            return RoutingDecision::NoCandidate(NoCandidate {
                reason: NoCandidateReasonCode::NoCandidatesProvided,
                rejected: Vec::new(),
            });
        }

        let mut identities = BTreeMap::<(&str, &str), usize>::new();
        for candidate in candidates {
            *identities
                .entry((candidate.provider.as_str(), candidate.model.id.as_str()))
                .or_default() += 1;
        }

        let mut eligible = Vec::new();
        let mut rejected = Vec::new();
        for candidate in candidates {
            let reasons = if identities
                .get(&(candidate.provider.as_str(), candidate.model.id.as_str()))
                .is_some_and(|count| *count > 1)
            {
                vec![CandidateRejectionCode::DuplicateCandidate]
            } else {
                rejection_reasons(request, candidate)
            };
            if reasons.is_empty() {
                eligible.push(candidate);
            } else {
                rejected.push(CandidateRejection {
                    provider: candidate.provider.clone(),
                    model: candidate.model.id.clone(),
                    reasons,
                });
            }
        }

        if eligible.is_empty() {
            rejected.sort_by(|left, right| {
                (&left.provider, &left.model).cmp(&(&right.provider, &right.model))
            });
            return RoutingDecision::NoCandidate(NoCandidate {
                reason: NoCandidateReasonCode::AllCandidatesRejected,
                rejected,
            });
        }

        eligible.sort_by(|left, right| compare_candidates(request, left, right));
        let winner = eligible[0];
        let reason = selection_reason(request, &eligible);
        RoutingDecision::Selected(RoutingSelection {
            provider: Some(winner.provider.clone()),
            model: winner.model.id.clone(),
            reason,
        })
    }
}

/// Preserve explicit manual requests and dispatch automatic requests to the
/// replaceable policy. Explicit choices therefore never get ranked or replaced
/// by injected automatic candidates.
pub fn route_model(
    request: &RoutingRequest,
    candidates: &[RoutingCandidate],
    automatic: &dyn ModelRouter,
) -> RoutingDecision {
    match request {
        RoutingRequest::Manual { .. } => ManualModelRouter.route(request, candidates),
        RoutingRequest::Automatic(_) => automatic.route(request, candidates),
    }
}

fn mode_mismatch() -> RoutingDecision {
    RoutingDecision::NoCandidate(NoCandidate {
        reason: NoCandidateReasonCode::RouterModeMismatch,
        rejected: Vec::new(),
    })
}

fn rejection_reasons(
    request: &AutomaticRoutingRequest,
    candidate: &RoutingCandidate,
) -> Vec<CandidateRejectionCode> {
    let mut reasons = Vec::new();
    if !candidate.readiness.discoverable {
        reasons.push(CandidateRejectionCode::ProviderNotDiscoverable);
    }
    if !candidate.readiness.executable {
        reasons.push(CandidateRejectionCode::AdapterUnavailable);
    }
    if !candidate.readiness.configured {
        reasons.push(CandidateRejectionCode::CredentialsMissing);
    }
    match candidate.readiness.verification {
        CapabilityVerification::Unknown => {
            reasons.push(CandidateRejectionCode::VerificationRequired)
        }
        CapabilityVerification::Failed => reasons.push(CandidateRejectionCode::ProbeFailed),
        CapabilityVerification::Stale => reasons.push(CandidateRejectionCode::EvidenceStale),
        CapabilityVerification::Verified => {}
    }

    let mut required_capabilities = request.required_capabilities.clone();
    required_capabilities.sort_unstable();
    required_capabilities.dedup();
    for required in required_capabilities {
        let mut facts = candidate
            .capabilities
            .iter()
            .filter(|fact| fact.name == required);
        match (facts.next(), facts.next()) {
            (None, _) => reasons.push(CandidateRejectionCode::CapabilityUnknown(required)),
            (Some(_), Some(_)) => reasons.push(
                CandidateRejectionCode::DuplicateCapabilityEvidence(required),
            ),
            (Some(fact), None) => {
                if fact.support == CapabilitySupport::Unsupported {
                    reasons.push(CandidateRejectionCode::CapabilityUnsupported(required));
                } else if fact.support == CapabilitySupport::Unknown {
                    reasons.push(CandidateRejectionCode::CapabilityUnknown(required));
                } else if candidate.capability_evidence_freshness
                    == CapabilityEvidenceFreshness::Stale
                {
                    reasons.push(CandidateRejectionCode::CapabilityEvidenceStale(required));
                } else if candidate.capability_evidence_freshness
                    == CapabilityEvidenceFreshness::Unknown
                {
                    reasons.push(CandidateRejectionCode::CapabilityEvidenceFreshnessUnknown(
                        required,
                    ));
                } else if fact.provenance == CapabilityProvenance::Observed {
                    reasons.push(CandidateRejectionCode::CapabilityUnverified(required));
                }
            }
        }
    }

    if let Some(minimum) = request.minimum_context_window_tokens {
        match candidate.model.context_window {
            None => reasons.push(CandidateRejectionCode::ContextWindowUnknown),
            Some(actual) if actual < minimum => {
                reasons.push(CandidateRejectionCode::ContextWindowTooSmall);
            }
            Some(_) => {}
        }
    }
    if let Some(minimum) = request.minimum_output_tokens {
        match candidate.model.max_output_tokens {
            None => reasons.push(CandidateRejectionCode::OutputLimitUnknown),
            Some(actual) if actual < minimum => {
                reasons.push(CandidateRejectionCode::OutputLimitTooSmall);
            }
            Some(_) => {}
        }
    }
    if let Some(maximum) = request.maximum_estimated_cost_micro_usd {
        match candidate.estimated_cost_micro_usd {
            None => reasons.push(CandidateRejectionCode::CostUnknown),
            Some(actual) if actual > maximum => reasons.push(CandidateRejectionCode::OverBudget),
            Some(_) => {}
        }
    }
    if candidate
        .quality_score_basis_points
        .is_some_and(|score| score > 10_000)
    {
        reasons.push(CandidateRejectionCode::InvalidQualityEstimate);
    }

    reasons.sort_unstable();
    reasons.dedup();
    reasons
}

fn compare_candidates(
    request: &AutomaticRoutingRequest,
    left: &RoutingCandidate,
    right: &RoutingCandidate,
) -> Ordering {
    for preference in &request.preferences {
        let ordering = match preference {
            SelectionPreference::LowestCost => compare_optional_ascending(
                left.estimated_cost_micro_usd,
                right.estimated_cost_micro_usd,
            ),
            SelectionPreference::LowestLatency => {
                compare_optional_ascending(left.observed_latency_ms, right.observed_latency_ms)
            }
            SelectionPreference::HighestQuality => compare_optional_descending(
                left.quality_score_basis_points,
                right.quality_score_basis_points,
            ),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    left.provider
        .cmp(&right.provider)
        .then_with(|| left.model.id.cmp(&right.model.id))
}

fn selection_reason(
    request: &AutomaticRoutingRequest,
    eligible: &[&RoutingCandidate],
) -> SelectionReasonCode {
    if eligible.len() == 1 {
        return SelectionReasonCode::OnlyEligibleCandidate;
    }
    let winner = eligible[0];
    let runner_up = eligible[1];
    for preference in &request.preferences {
        let ordering = match preference {
            SelectionPreference::LowestCost => compare_optional_ascending(
                winner.estimated_cost_micro_usd,
                runner_up.estimated_cost_micro_usd,
            ),
            SelectionPreference::LowestLatency => compare_optional_ascending(
                winner.observed_latency_ms,
                runner_up.observed_latency_ms,
            ),
            SelectionPreference::HighestQuality => compare_optional_descending(
                winner.quality_score_basis_points,
                runner_up.quality_score_basis_points,
            ),
        };
        if ordering != Ordering::Equal {
            return match preference {
                SelectionPreference::LowestCost => SelectionReasonCode::LowestCostPreference,
                SelectionPreference::LowestLatency => SelectionReasonCode::LowestLatencyPreference,
                SelectionPreference::HighestQuality => {
                    SelectionReasonCode::HighestQualityPreference
                }
            };
        }
    }
    SelectionReasonCode::StableTieBreak
}

fn compare_optional_ascending<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_optional_descending<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    compare_optional_ascending(right, left)
}

#[cfg(test)]
#[path = "routing/tests.rs"]
mod tests;
