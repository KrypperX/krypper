use super::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

fn readiness(verification: CapabilityVerification) -> ProviderReadiness {
    ProviderReadiness {
        discoverable: true,
        executable: true,
        configured: true,
        verification,
    }
}

fn model(id: &str) -> ModelSpec {
    ModelSpec {
        provider: "test-provider".to_owned(),
        id: id.to_owned(),
        name: id.to_owned(),
        context_window: Some(32_000),
        max_output_tokens: Some(8_000),
        tool_calling: Some(true),
        reasoning: Some(false),
        status: Some("active".to_owned()),
        modalities: None,
    }
}

fn candidate(provider: &str, id: &str) -> RoutingCandidate {
    RoutingCandidate {
        provider: provider.to_owned(),
        model: model(id),
        readiness: readiness(CapabilityVerification::Verified),
        capabilities: vec![CapabilityAssertion::new(
            CapabilityName::ToolCalling,
            CapabilitySupport::Supported,
            CapabilityProvenance::Declared,
        )],
        capability_evidence_freshness: CapabilityEvidenceFreshness::Fresh,
        estimated_cost_micro_usd: None,
        observed_latency_ms: None,
        quality_score_basis_points: None,
    }
}

fn automatic_request() -> RoutingRequest {
    RoutingRequest::Automatic(AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        ..AutomaticRoutingRequest::default()
    })
}

fn select(candidates: &[RoutingCandidate], request: &RoutingRequest) -> RoutingDecision {
    route_model(request, candidates, &DeterministicModelRouter)
}

fn has_reason(result: &NoCandidate, expected: CandidateRejectionCode) -> bool {
    result
        .rejected
        .iter()
        .any(|candidate| candidate.reasons.contains(&expected))
}

fn selected(provider: Option<&str>, model: &str, reason: SelectionReasonCode) -> RoutingDecision {
    RoutingDecision::Selected(RoutingSelection {
        provider: provider.map(str::to_owned),
        model: model.to_owned(),
        reason,
    })
}

#[test]
fn explicit_manual_selection_preserves_unknown_models_and_skips_auto_router() {
    struct CountingRouter(AtomicUsize);
    impl ModelRouter for CountingRouter {
        fn route(
            &self,
            _request: &RoutingRequest,
            _candidates: &[RoutingCandidate],
        ) -> RoutingDecision {
            self.0.fetch_add(1, AtomicOrdering::Relaxed);
            RoutingDecision::NoCandidate(NoCandidate {
                reason: NoCandidateReasonCode::NoCandidatesProvided,
                rejected: Vec::new(),
            })
        }
    }

    let auto = CountingRouter(AtomicUsize::new(0));
    let decision = route_model(
        &RoutingRequest::Manual {
            provider: Some("custom-profile".to_owned()),
            model: "model-absent-from-catalog".to_owned(),
        },
        &[],
        &auto,
    );
    assert_eq!(
        decision,
        RoutingDecision::Selected(RoutingSelection {
            provider: Some("custom-profile".to_owned()),
            model: "model-absent-from-catalog".to_owned(),
            reason: SelectionReasonCode::ManualOverride,
        })
    );
    assert_eq!(auto.0.load(AtomicOrdering::Relaxed), 0);
}

#[test]
fn alternate_router_can_be_injected_through_the_shared_policy_interface() {
    struct FixedRouter;
    impl ModelRouter for FixedRouter {
        fn route(
            &self,
            request: &RoutingRequest,
            _candidates: &[RoutingCandidate],
        ) -> RoutingDecision {
            assert!(matches!(request, RoutingRequest::Automatic(_)));
            RoutingDecision::Selected(RoutingSelection {
                provider: Some("fixture".to_owned()),
                model: "fixture-model".to_owned(),
                reason: SelectionReasonCode::StableTieBreak,
            })
        }
    }

    assert_eq!(
        select(&[], &automatic_request()),
        RoutingDecision::NoCandidate(NoCandidate {
            reason: NoCandidateReasonCode::NoCandidatesProvided,
            rejected: Vec::new(),
        })
    );
    let decision = route_model(&automatic_request(), &[], &FixedRouter);
    assert_eq!(
        decision,
        selected(
            Some("fixture"),
            "fixture-model",
            SelectionReasonCode::StableTieBreak
        )
    );
}

#[test]
fn deterministic_router_filters_unconfigured_unexecutable_and_unverified_candidates() {
    let mut missing_credentials = candidate("a", "missing-credentials");
    missing_credentials.readiness.configured = false;
    let mut undiscoverable = candidate("a", "undiscoverable");
    undiscoverable.readiness.discoverable = false;
    let mut unavailable_adapter = candidate("a", "missing-adapter");
    unavailable_adapter.readiness.executable = false;
    let mut unverified = candidate("a", "unverified");
    unverified.readiness.verification = CapabilityVerification::Unknown;
    let mut failed_probe = candidate("a", "failed-probe");
    failed_probe.readiness.verification = CapabilityVerification::Failed;
    let mut stale = candidate("a", "stale");
    stale.readiness.verification = CapabilityVerification::Stale;

    let decision = select(
        &[
            missing_credentials,
            undiscoverable,
            unavailable_adapter,
            unverified,
            failed_probe,
            stale,
        ],
        &automatic_request(),
    );
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("all candidates should be rejected");
    };
    assert_eq!(result.reason, NoCandidateReasonCode::AllCandidatesRejected);
    assert_eq!(result.rejected.len(), 6);
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CredentialsMissing
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::AdapterUnavailable
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::ProviderNotDiscoverable
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::VerificationRequired
    ));
    assert!(has_reason(&result, CandidateRejectionCode::ProbeFailed));
    assert!(has_reason(&result, CandidateRejectionCode::EvidenceStale));
}

#[test]
fn unsupported_unknown_and_observed_capabilities_are_not_eligible() {
    let mut unsupported = candidate("a", "unsupported");
    unsupported.capabilities[0].support = CapabilitySupport::Unsupported;
    let mut unknown = candidate("b", "unknown");
    unknown.capabilities[0].support = CapabilitySupport::Unknown;
    let mut observed = candidate("c", "observed");
    observed.capabilities[0].provenance = CapabilityProvenance::Observed;

    let decision = select(&[unsupported, unknown, observed], &automatic_request());
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("unsupported and unknown features must not be selected");
    };
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CapabilityUnsupported(CapabilityName::ToolCalling)
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CapabilityUnknown(CapabilityName::ToolCalling)
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CapabilityUnverified(CapabilityName::ToolCalling)
    ));
}

#[test]
fn stale_or_unknown_capability_source_cannot_verify_a_required_capability() {
    let mut stale_capability = candidate("a", "stale-capability");
    stale_capability.capability_evidence_freshness = CapabilityEvidenceFreshness::Stale;
    let mut unknown_freshness = candidate("b", "unknown-freshness");
    unknown_freshness.capability_evidence_freshness = CapabilityEvidenceFreshness::Unknown;

    let decision = select(&[stale_capability, unknown_freshness], &automatic_request());
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("stale or unchecked capability evidence must not be selected");
    };
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CapabilityEvidenceStale(CapabilityName::ToolCalling)
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::CapabilityEvidenceFreshnessUnknown(CapabilityName::ToolCalling)
    ));
}

#[test]
fn context_limits_and_unknown_cost_fail_closed_when_the_request_requires_them() {
    let mut small_context = candidate("a", "small");
    small_context.model.context_window = Some(1_000);
    let mut unknown_context = candidate("a", "unknown-context");
    unknown_context.model.context_window = None;
    let unknown_cost = candidate("b", "unknown-cost");
    let mut over_budget = candidate("c", "over-budget");
    over_budget.estimated_cost_micro_usd = Some(50);
    let mut within_budget = candidate("d", "within-budget");
    within_budget.estimated_cost_micro_usd = Some(10);
    within_budget.model.max_output_tokens = Some(128);
    let mut unknown_output = candidate("e", "unknown-output");
    unknown_output.model.max_output_tokens = None;

    let request = RoutingRequest::Automatic(AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        minimum_context_window_tokens: Some(2_000),
        minimum_output_tokens: Some(256),
        maximum_estimated_cost_micro_usd: Some(20),
        preferences: Vec::new(),
    });
    let decision = select(
        &[
            small_context,
            unknown_context,
            unknown_cost,
            over_budget,
            within_budget,
            unknown_output,
        ],
        &request,
    );
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("no candidate satisfies all three constraints");
    };
    assert!(has_reason(
        &result,
        CandidateRejectionCode::ContextWindowTooSmall
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::ContextWindowUnknown
    ));
    assert!(has_reason(&result, CandidateRejectionCode::CostUnknown));
    assert!(has_reason(&result, CandidateRejectionCode::OverBudget));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::OutputLimitTooSmall
    ));
    assert!(has_reason(
        &result,
        CandidateRejectionCode::OutputLimitUnknown
    ));
}

#[test]
fn ordered_preferences_rank_cost_latency_and_quality_without_unknown_becoming_zero() {
    let mut cheap_slow = candidate("p", "cheap-slow");
    cheap_slow.estimated_cost_micro_usd = Some(10);
    cheap_slow.observed_latency_ms = Some(900);
    cheap_slow.quality_score_basis_points = Some(8_000);
    let mut expensive_fast = candidate("p", "expensive-fast");
    expensive_fast.estimated_cost_micro_usd = Some(40);
    expensive_fast.observed_latency_ms = Some(100);
    expensive_fast.quality_score_basis_points = Some(9_000);
    let mut unknown_cost = candidate("a", "unknown-cost");
    unknown_cost.observed_latency_ms = Some(1);
    unknown_cost.quality_score_basis_points = Some(10_000);

    let cost_first = RoutingRequest::Automatic(AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        preferences: vec![
            SelectionPreference::LowestCost,
            SelectionPreference::LowestLatency,
            SelectionPreference::HighestQuality,
        ],
        ..AutomaticRoutingRequest::default()
    });
    assert_eq!(
        select(
            &[
                expensive_fast.clone(),
                unknown_cost.clone(),
                cheap_slow.clone(),
            ],
            &cost_first,
        ),
        selected(
            Some("p"),
            "cheap-slow",
            SelectionReasonCode::LowestCostPreference
        )
    );

    let latency_first = RoutingRequest::Automatic(AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        preferences: vec![SelectionPreference::LowestLatency],
        ..AutomaticRoutingRequest::default()
    });
    assert_eq!(
        select(
            &[
                cheap_slow.clone(),
                expensive_fast.clone(),
                unknown_cost.clone(),
            ],
            &latency_first,
        ),
        selected(
            Some("a"),
            "unknown-cost",
            SelectionReasonCode::LowestLatencyPreference
        )
    );

    let quality_first = RoutingRequest::Automatic(AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        preferences: vec![SelectionPreference::HighestQuality],
        ..AutomaticRoutingRequest::default()
    });
    assert_eq!(
        select(
            &[cheap_slow.clone(), expensive_fast.clone(), unknown_cost],
            &quality_first,
        ),
        selected(
            Some("a"),
            "unknown-cost",
            SelectionReasonCode::HighestQualityPreference
        )
    );
}

#[test]
fn equal_scores_tie_break_by_provider_and_model_independent_of_input_order() {
    let candidates = [
        candidate("z-provider", "model-b"),
        candidate("a-provider", "model-z"),
        candidate("a-provider", "model-a"),
    ];
    let request = automatic_request();
    let first = select(&candidates, &request);
    let reversed: Vec<_> = candidates.into_iter().rev().collect();
    let second = select(&reversed, &request);
    assert_eq!(first, second);
    assert_eq!(
        first,
        selected(
            Some("a-provider"),
            "model-a",
            SelectionReasonCode::StableTieBreak
        )
    );
}

#[test]
fn duplicate_provider_model_candidates_are_rejected_deterministically() {
    let duplicate = candidate("provider", "same-model");
    let decision = select(&[duplicate.clone(), duplicate], &automatic_request());
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("conflicting duplicate candidates must be rejected");
    };
    assert_eq!(result.rejected.len(), 2);
    assert!(result.rejected.iter().all(|candidate| {
        candidate
            .reasons
            .contains(&CandidateRejectionCode::DuplicateCandidate)
    }));
}

#[test]
fn conflicting_duplicate_capability_assertions_are_rejected() {
    let mut duplicate = candidate("provider", "ambiguous-model");
    duplicate.capabilities.push(CapabilityAssertion::new(
        CapabilityName::ToolCalling,
        CapabilitySupport::Unsupported,
        CapabilityProvenance::Declared,
    ));

    let decision = select(&[duplicate], &automatic_request());
    let RoutingDecision::NoCandidate(result) = decision else {
        panic!("conflicting capability facts must not make a candidate eligible");
    };
    assert!(result.rejected[0].reasons.contains(
        &CandidateRejectionCode::DuplicateCapabilityEvidence(CapabilityName::ToolCalling)
    ));
}
