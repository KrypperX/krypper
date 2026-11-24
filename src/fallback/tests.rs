use super::*;
use crate::routing::{
    CapabilityAssertion, CapabilityEvidenceFreshness, CapabilityName, CapabilityProvenance,
    CapabilitySupport, CapabilityVerification, NoCandidateReasonCode, ProviderReadiness,
};

fn limits() -> FallbackLimits {
    FallbackLimits {
        max_additional_attempts: 4,
        max_total_provider_requests: 5,
        max_total_estimated_cost_micro_usd: 100,
        initial_backoff: Duration::from_millis(100),
        maximum_backoff: Duration::from_millis(250),
        allow_cross_provider_data_transfer: false,
    }
}

fn policy() -> FallbackPolicy {
    FallbackPolicy::opt_in(limits()).expect("valid fallback limits")
}

fn request() -> AutomaticRoutingRequest {
    AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        minimum_context_window_tokens: Some(4_000),
        minimum_output_tokens: Some(1_000),
        maximum_estimated_cost_micro_usd: Some(50),
        ..AutomaticRoutingRequest::default()
    }
}

fn route(provider: &str, model: &str) -> RoutingSelection {
    RoutingSelection {
        provider: Some(provider.to_owned()),
        model: model.to_owned(),
        reason: SelectionReasonCode::LowestCostPreference,
    }
}

fn candidate(provider: &str, id: &str, cost: Option<u64>) -> RoutingCandidate {
    RoutingCandidate {
        provider: provider.to_owned(),
        model: crate::catalog_data::ModelSpec {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            context_window: Some(32_000),
            max_output_tokens: Some(8_000),
            tool_calling: Some(true),
            reasoning: Some(false),
            status: Some("active".to_owned()),
            modalities: None,
        },
        readiness: ProviderReadiness {
            discoverable: true,
            executable: true,
            configured: true,
            verification: CapabilityVerification::Verified,
        },
        capabilities: vec![CapabilityAssertion::new(
            CapabilityName::ToolCalling,
            CapabilitySupport::Supported,
            CapabilityProvenance::Declared,
        )],
        capability_evidence_freshness: CapabilityEvidenceFreshness::Fresh,
        estimated_cost_micro_usd: cost,
        observed_latency_ms: Some(100),
        quality_score_basis_points: Some(8_000),
    }
}

fn failure(code: DecisionFailureCode, status: Option<u16>) -> FallbackFailure {
    FallbackFailure::new(code, status, ProviderCompletion::ExplicitHttpError)
}

fn usage(requests: u8, cost: Option<u64>) -> FallbackUsage {
    FallbackUsage {
        total_provider_requests: requests,
        total_estimated_cost_micro_usd: cost,
    }
}

fn context<'a>(
    request: &'a AutomaticRoutingRequest,
    current: &'a RoutingSelection,
    attempted: &'a [RoutingSelection],
    candidates: &'a [RoutingCandidate],
    failure: FallbackFailure,
    usage: FallbackUsage,
) -> FallbackContext<'a> {
    FallbackContext {
        request,
        current_selection: current,
        attempted_routes: attempted,
        candidates,
        boundary: FallbackBoundary::InitialTaskStart,
        cancelled: false,
        failure,
        usage,
    }
}

fn switch(decision: FallbackDecision) -> FallbackPlan {
    match decision {
        FallbackDecision::Switch(plan) => plan,
        FallbackDecision::Stop(reason) => panic!("expected fallback, got stop: {reason:?}"),
    }
}

fn stopped(decision: FallbackDecision) -> FallbackStopReason {
    match decision {
        FallbackDecision::Switch(plan) => panic!("expected stop, got route: {plan:?}"),
        FallbackDecision::Stop(reason) => reason,
    }
}

#[test]
fn fallback_is_disabled_by_default_and_limits_must_be_bounded() {
    assert!(!FallbackPolicy::default().is_enabled());
    assert_eq!(
        plan_fallback(
            &FallbackPolicy::default(),
            &FallbackContext {
                request: &request(),
                current_selection: &route("p", "m1"),
                attempted_routes: &[],
                candidates: &[],
                boundary: FallbackBoundary::InitialTaskStart,
                cancelled: false,
                failure: failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
                usage: usage(1, Some(1)),
            },
            &DeterministicModelRouter,
        ),
        FallbackDecision::Stop(FallbackStopReason::Disabled)
    );

    let mut invalid = limits();
    invalid.max_additional_attempts = 0;
    assert_eq!(
        FallbackPolicy::opt_in(invalid),
        Err(FallbackPolicyError::InvalidAttemptLimit)
    );
    let mut invalid = limits();
    invalid.max_total_provider_requests = 2;
    assert_eq!(
        FallbackPolicy::opt_in(invalid),
        Err(FallbackPolicyError::InvalidRequestLimit)
    );
    let mut invalid = limits();
    invalid.maximum_backoff = MAX_FALLBACK_BACKOFF + Duration::from_millis(1);
    assert_eq!(
        FallbackPolicy::opt_in(invalid),
        Err(FallbackPolicyError::InvalidBackoff)
    );
}

#[test]
fn rate_limit_selects_an_untried_same_provider_route_with_bounded_backoff_and_cost() {
    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(15)),
        candidate("provider-b", "model-3", Some(5)),
    ];
    let context = context(
        &request,
        &current,
        &attempted,
        &candidates,
        failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
        usage(1, Some(10)),
    );
    let plan = switch(plan_fallback(
        &policy(),
        &context,
        &DeterministicModelRouter,
    ));
    assert_eq!(plan.selection.provider.as_deref(), Some("provider-a"));
    assert_eq!(plan.selection.model, "model-2");
    assert_eq!(plan.reason, FallbackSwitchReason::RateLimited);
    assert_eq!(plan.backoff, Duration::from_millis(100));
    assert!(!plan.crosses_provider);
    assert_eq!(plan.estimated_cost_micro_usd, 15);
    assert_eq!(plan.remaining_estimated_cost_micro_usd, 75);
}

#[test]
fn explicit_server_failures_are_eligible_but_timeout_statuses_are_not() {
    for status in [500, 503, 599] {
        let request = request();
        let current = route("provider-a", "model-1");
        let attempted = [current.clone()];
        let candidates = [
            candidate("provider-a", "model-1", Some(10)),
            candidate("provider-a", "model-2", Some(10)),
        ];
        let context = context(
            &request,
            &current,
            &attempted,
            &candidates,
            failure(DecisionFailureCode::ProviderUnavailable, Some(status)),
            usage(1, Some(10)),
        );
        assert_eq!(
            switch(plan_fallback(
                &policy(),
                &context,
                &DeterministicModelRouter
            ))
            .reason,
            FallbackSwitchReason::ServerFailure
        );
    }
    for status in [408, 425] {
        assert_eq!(
            stopped(plan_fallback(
                &policy(),
                &context(
                    &request(),
                    &route("provider-a", "model-1"),
                    &[route("provider-a", "model-1")],
                    &[
                        candidate("provider-a", "model-1", Some(10)),
                        candidate("provider-a", "model-2", Some(10)),
                    ],
                    failure(DecisionFailureCode::ProviderUnavailable, Some(status)),
                    usage(1, Some(10)),
                ),
                &DeterministicModelRouter,
            )),
            FallbackStopReason::AmbiguousCompletion
        );
    }
}

#[test]
fn authentication_rejection_timeout_and_partial_stream_stop() {
    for (failure, expected) in [
        (
            failure(DecisionFailureCode::ProviderRejected, Some(401)),
            FallbackStopReason::FailureNotRetryable,
        ),
        (
            FallbackFailure::new(
                DecisionFailureCode::ProviderUnavailable,
                None,
                ProviderCompletion::Ambiguous,
            ),
            FallbackStopReason::AmbiguousCompletion,
        ),
        (
            FallbackFailure::new(
                DecisionFailureCode::InvalidProviderResponse,
                None,
                ProviderCompletion::Partial,
            ),
            FallbackStopReason::PartialResponse,
        ),
    ] {
        let request = request();
        let current = route("provider-a", "model-1");
        let attempted = [current.clone()];
        let candidates = [
            candidate("provider-a", "model-1", Some(10)),
            candidate("provider-a", "model-2", Some(10)),
        ];
        assert_eq!(
            stopped(plan_fallback(
                &policy(),
                &context(
                    &request,
                    &current,
                    &attempted,
                    &candidates,
                    failure,
                    usage(1, Some(10)),
                ),
                &DeterministicModelRouter,
            )),
            expected
        );
    }
}

#[test]
fn cancellation_and_unsafe_turn_state_stop_switching() {
    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(10)),
    ];
    let mut context = context(
        &request,
        &current,
        &attempted,
        &candidates,
        failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
        usage(1, Some(10)),
    );
    context.cancelled = true;
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context,
            &DeterministicModelRouter
        )),
        FallbackStopReason::Cancelled
    );
    context.cancelled = false;
    for boundary in [
        FallbackBoundary::WithinTurn,
        FallbackBoundary::UnreconciledTurnStart,
    ] {
        context.boundary = boundary;
        assert_eq!(
            stopped(plan_fallback(
                &policy(),
                &context,
                &DeterministicModelRouter
            )),
            FallbackStopReason::UnsafeBoundary
        );
    }
    context.boundary = FallbackBoundary::BeforeActionDispatch;
    assert!(matches!(
        plan_fallback(&policy(), &context, &DeterministicModelRouter),
        FallbackDecision::Switch(_)
    ));
}

#[test]
fn manual_selection_cannot_be_replaced() {
    let request = request();
    let mut current = route("provider-a", "manual-model");
    current.reason = SelectionReasonCode::ManualOverride;
    let attempted = [current.clone()];
    let candidates = [candidate("provider-a", "model-2", Some(10))];
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
                usage(1, Some(10)),
            ),
            &DeterministicModelRouter,
        )),
        FallbackStopReason::ManualSelection
    );
}

#[test]
fn previously_attempted_routes_cannot_be_replayed_by_a_router() {
    struct RepeatingRouter;
    impl ModelRouter for RepeatingRouter {
        fn route(
            &self,
            _request: &RoutingRequest,
            _candidates: &[RoutingCandidate],
        ) -> RoutingDecision {
            RoutingDecision::Selected(route("provider-a", "model-1"))
        }
    }

    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(10)),
    ];
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
                usage(1, Some(10)),
            ),
            &RepeatingRouter,
        )),
        FallbackStopReason::InvalidRouterSelection
    );
}

#[test]
fn aggregate_request_attempt_cost_and_unknown_cost_limits_stop() {
    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(10)),
    ];
    let failure = failure(DecisionFailureCode::ProviderRateLimited, Some(429));

    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure,
                usage(5, Some(10)),
            ),
            &DeterministicModelRouter,
        )),
        FallbackStopReason::RequestBudgetExhausted
    );

    let mut one_retry = limits();
    one_retry.max_additional_attempts = 1;
    one_retry.max_total_provider_requests = 2;
    let one_retry = FallbackPolicy::opt_in(one_retry).expect("valid limits");
    let attempted_two = [
        route("provider-a", "model-1"),
        route("provider-a", "model-x"),
    ];
    assert_eq!(
        stopped(plan_fallback(
            &one_retry,
            &context(
                &request,
                &attempted_two[1],
                &attempted_two,
                &candidates,
                failure,
                usage(2, Some(20)),
            ),
            &DeterministicModelRouter,
        )),
        FallbackStopReason::AttemptsExhausted
    );
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure,
                usage(1, None),
            ),
            &DeterministicModelRouter,
        )),
        FallbackStopReason::UnknownAggregateCost
    );
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure,
                usage(1, Some(101)),
            ),
            &DeterministicModelRouter,
        )),
        FallbackStopReason::CostBudgetExhausted
    );
}

#[test]
fn remaining_aggregate_cost_rejects_cost_escalation_and_unknown_candidate_price() {
    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(6)),
    ];
    let mut constrained = limits();
    constrained.max_total_estimated_cost_micro_usd = 15;
    let constrained = FallbackPolicy::opt_in(constrained).expect("valid limits");
    assert!(matches!(
        plan_fallback(
            &constrained,
            &context(
                &request,
                &current,
                &attempted,
                &candidates,
                failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
                usage(1, Some(10)),
            ),
            &DeterministicModelRouter,
        ),
        FallbackDecision::Stop(FallbackStopReason::NoCandidate(_))
    ));

    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-unknown-cost", None),
    ];
    let decision = plan_fallback(
        &policy(),
        &context(
            &request,
            &current,
            &attempted,
            &candidates,
            failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
            usage(1, Some(10)),
        ),
        &DeterministicModelRouter,
    );
    let FallbackDecision::Stop(FallbackStopReason::NoCandidate(rejection)) = decision else {
        panic!("unknown cost must fail closed");
    };
    assert_eq!(
        rejection.reason,
        NoCandidateReasonCode::AllCandidatesRejected
    );
}

#[test]
fn cross_provider_transfer_requires_explicit_opt_in() {
    let request = request();
    let current = route("provider-a", "model-1");
    let attempted = [current.clone()];
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-b", "model-2", Some(10)),
    ];
    let context = context(
        &request,
        &current,
        &attempted,
        &candidates,
        failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
        usage(1, Some(10)),
    );
    assert_eq!(
        stopped(plan_fallback(
            &policy(),
            &context,
            &DeterministicModelRouter
        )),
        FallbackStopReason::CrossProviderTransferNotAuthorized
    );

    let mut opted_in = limits();
    opted_in.allow_cross_provider_data_transfer = true;
    let plan = switch(plan_fallback(
        &FallbackPolicy::opt_in(opted_in).expect("valid limits"),
        &context,
        &DeterministicModelRouter,
    ));
    assert!(plan.crosses_provider);
    assert_eq!(plan.selection.provider.as_deref(), Some("provider-b"));
}

#[test]
fn failure_can_be_built_from_the_existing_decision_failure_action_label() {
    let action = WorkspaceAction::Unsupported {
        action: "decision_failure:provider_rate_limited:429".to_owned(),
    };
    let failure = FallbackFailure::from_action(&action, ProviderCompletion::ExplicitHttpError)
        .expect("typed failure label");
    assert_eq!(failure.code(), DecisionFailureCode::ProviderRateLimited);
    assert_eq!(failure.http_status(), Some(429));
    assert_eq!(failure.completion(), ProviderCompletion::ExplicitHttpError);
    assert!(FallbackFailure::from_action(
        &WorkspaceAction::Continue,
        ProviderCompletion::Ambiguous
    )
    .is_none());
    assert!(FallbackFailure::from_action(
        &WorkspaceAction::CreateFile {
            path: std::path::PathBuf::from("result.txt"),
            contents: "committed action".to_owned(),
        },
        ProviderCompletion::Ambiguous
    )
    .is_none());
}

#[test]
fn backoff_grows_exponentially_and_stays_capped() {
    let request = request();
    let candidates = [
        candidate("provider-a", "model-1", Some(10)),
        candidate("provider-a", "model-2", Some(10)),
        candidate("provider-a", "model-3", Some(10)),
        candidate("provider-a", "model-4", Some(10)),
    ];
    for (attempts, requests, expected) in [
        (
            vec![route("provider-a", "model-1")],
            1,
            Duration::from_millis(100),
        ),
        (
            vec![
                route("provider-a", "model-1"),
                route("provider-a", "model-2"),
            ],
            2,
            Duration::from_millis(200),
        ),
        (
            vec![
                route("provider-a", "model-1"),
                route("provider-a", "model-2"),
                route("provider-a", "model-3"),
            ],
            3,
            Duration::from_millis(250),
        ),
    ] {
        let current = attempts.last().expect("route").clone();
        let context = context(
            &request,
            &current,
            &attempts,
            &candidates,
            failure(DecisionFailureCode::ProviderRateLimited, Some(429)),
            usage(requests, Some(10 * u64::from(requests))),
        );
        assert_eq!(
            switch(plan_fallback(
                &policy(),
                &context,
                &DeterministicModelRouter
            ))
            .backoff,
            expected
        );
    }
}
