//! Opt-in, bounded planning and execution for model fallback and provider
//! switching.
//!
//! The planner is pure. [`FallbackDecisionSource`] owns one model decision
//! across bounded routes before returning one action to Harness. Every
//! retry receives the same goal and authoritative observations, and Harness has
//! not dispatched an action from the failed decision.

use crate::agent_loop::{DecisionFailureCode, WorkspaceAction};
use crate::routing::{
    AutomaticRoutingRequest, DeterministicModelRouter, ModelRouter, NoCandidate, RoutingCandidate,
    RoutingDecision, RoutingRequest, RoutingSelection, SelectionReasonCode,
};
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

/// Hard upper bound for configured additional fallback attempts in one chain.
pub const MAX_FALLBACK_ATTEMPTS: u8 = 5;
/// Hard upper bound for provider requests counted across one task.
pub const MAX_FALLBACK_PROVIDER_REQUESTS: u8 = 16;
/// Hard upper bound for the delay proposed before one fallback request.
pub const MAX_FALLBACK_BACKOFF: Duration = Duration::from_secs(120);

/// Limits required to opt into fallback planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FallbackLimits {
    /// Maximum number of additional routes in one failure chain.
    pub max_additional_attempts: u8,
    /// Maximum provider requests across the whole task, including failed calls.
    pub max_total_provider_requests: u8,
    /// Maximum estimated aggregate provider cost across the whole task, in micro-USD.
    pub max_total_estimated_cost_micro_usd: u64,
    /// Delay before the first fallback request.
    pub initial_backoff: Duration,
    /// Largest delay proposed before a fallback request.
    pub maximum_backoff: Duration,
    /// Explicit consent to send task context to a different provider.
    pub allow_cross_provider_data_transfer: bool,
}

/// An opt-in policy for bounded fallback planning.
///
/// The default policy is disabled. Enabled policies always require finite
/// attempt, request, cost, and backoff limits. Cross-provider context transfer
/// remains separately disabled unless the caller explicitly opts in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FallbackPolicy {
    limits: Option<FallbackLimits>,
}

impl FallbackPolicy {
    /// Create an enabled policy after validating all bounds.
    pub fn opt_in(limits: FallbackLimits) -> Result<Self, FallbackPolicyError> {
        if limits.max_additional_attempts == 0
            || limits.max_additional_attempts > MAX_FALLBACK_ATTEMPTS
        {
            return Err(FallbackPolicyError::InvalidAttemptLimit);
        }
        let minimum_requests = limits.max_additional_attempts.saturating_add(1);
        if limits.max_total_provider_requests < minimum_requests
            || limits.max_total_provider_requests > MAX_FALLBACK_PROVIDER_REQUESTS
        {
            return Err(FallbackPolicyError::InvalidRequestLimit);
        }
        if limits.initial_backoff.is_zero()
            || limits.initial_backoff > limits.maximum_backoff
            || limits.maximum_backoff > MAX_FALLBACK_BACKOFF
        {
            return Err(FallbackPolicyError::InvalidBackoff);
        }
        Ok(Self {
            limits: Some(limits),
        })
    }

    /// Whether the caller explicitly enabled fallback planning.
    pub const fn is_enabled(&self) -> bool {
        self.limits.is_some()
    }

    /// Validated limits, or None when fallback is disabled.
    pub const fn limits(&self) -> Option<FallbackLimits> {
        self.limits
    }
}

/// Why an enabled fallback policy could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackPolicyError {
    /// The additional-attempt limit must be from one through five.
    InvalidAttemptLimit,
    /// The task request limit must cover the attempt limit and be at most sixteen.
    InvalidRequestLimit,
    /// Backoff must be positive, ordered, and at most 120 seconds.
    InvalidBackoff,
}

impl fmt::Display for FallbackPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidAttemptLimit => "fallback attempt limit must be from 1 through 5",
            Self::InvalidRequestLimit => {
                "fallback request limit must cover attempts and be at most 16"
            }
            Self::InvalidBackoff => {
                "fallback backoff must be positive, ordered, and at most 120 seconds"
            }
        })
    }
}

impl std::error::Error for FallbackPolicyError {}

/// Task/turn boundary supplied by the Harness before considering another route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackBoundary {
    /// The first decision request starts before any task action has run.
    InitialTaskStart,
    /// Harness has completed and recorded prior actions and their observations,
    /// and has not dispatched the action for this decision request.
    BeforeActionDispatch,
    /// A turn is still active or an action may have an uncommitted effect.
    WithinTurn,
    /// The caller cannot establish that prior task state was reconciled.
    UnreconciledTurnStart,
}

/// Completeness evidence for the provider response associated with a failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProviderCompletion {
    /// A complete provider response was received.
    Complete,
    /// An explicit HTTP error status was received; no decision was returned to Harness.
    ExplicitHttpError,
    /// The request may have completed, but no definitive response was received.
    Ambiguous,
    /// The provider returned a partial stream or tool call.
    Partial,
}

/// Typed provider failure information used by the fallback planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FallbackFailure {
    code: DecisionFailureCode,
    http_status: Option<u16>,
    completion: ProviderCompletion,
}

impl FallbackFailure {
    /// Build failure evidence from the existing secret-free action classification.
    ///
    /// Returns None for actions that do not carry a typed decision failure.
    pub fn from_action(action: &WorkspaceAction, completion: ProviderCompletion) -> Option<Self> {
        let WorkspaceAction::Unsupported { action: label } = action else {
            return None;
        };
        Some(Self {
            code: action.decision_failure_code()?,
            http_status: DecisionFailureCode::http_status_from_action_label(label),
            completion,
        })
    }

    /// Construct typed failure evidence from an already-classified decision error.
    pub const fn new(
        code: DecisionFailureCode,
        http_status: Option<u16>,
        completion: ProviderCompletion,
    ) -> Self {
        Self {
            code,
            http_status,
            completion,
        }
    }

    /// Stable decision failure code.
    pub const fn code(self) -> DecisionFailureCode {
        self.code
    }

    /// Explicit HTTP status, when the action label carries one.
    pub const fn http_status(self) -> Option<u16> {
        self.http_status
    }

    /// Whether the request had a complete rejection, ambiguous completion, or partial response.
    pub const fn completion(self) -> ProviderCompletion {
        self.completion
    }

    pub(super) fn requires_reconciliation(self) -> bool {
        matches!(
            classify_failure(self),
            Err(FallbackStopReason::AmbiguousCompletion | FallbackStopReason::PartialResponse)
        )
    }
}

/// Aggregate provider usage already consumed by the task.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackUsage {
    /// All provider requests, including the failed current request.
    pub total_provider_requests: u8,
    /// Sum of estimates for all requests so far; unknown remains unknown.
    pub total_estimated_cost_micro_usd: Option<u64>,
}

/// Inputs required to plan one fallback route.
pub struct FallbackContext<'a> {
    /// Original automatic-routing requirements. They are preserved for every route.
    pub request: &'a AutomaticRoutingRequest,
    /// Route whose request just failed.
    pub current_selection: &'a RoutingSelection,
    /// Routes requested in this failure chain, from initial route through current route.
    pub attempted_routes: &'a [RoutingSelection],
    /// Caller-provided candidates from the same configuration/evidence snapshot.
    pub candidates: &'a [RoutingCandidate],
    /// Caller-confirmed task/turn boundary.
    pub boundary: FallbackBoundary,
    /// Current cancellation state; callers must check it again after backoff.
    pub cancelled: bool,
    /// Typed response/failure evidence from the just-failed request.
    pub failure: FallbackFailure,
    /// Aggregate request and estimated-cost usage across the whole task.
    pub usage: FallbackUsage,
}

/// Why fallback planning selected another route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackSwitchReason {
    /// The previous provider returned HTTP 429.
    RateLimited,
    /// The previous provider returned an explicit HTTP 5xx status.
    ServerFailure,
}

/// A bounded next-route recommendation. The caller still owns cancellation,
/// waiting, credentials, request execution, and task state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackPlan {
    /// Selected provider/model route.
    pub selection: RoutingSelection,
    /// Reason the route can be tried after the current provider failure.
    pub reason: FallbackSwitchReason,
    /// Bounded delay the caller should wait cancellably before the next request.
    pub backoff: Duration,
    /// Whether this route sends task context to another provider.
    pub crosses_provider: bool,
    /// Candidate cost estimate used against the remaining aggregate budget.
    pub estimated_cost_micro_usd: u64,
    /// Remaining estimated task budget after this planned request.
    pub remaining_estimated_cost_micro_usd: u64,
}

/// Why the fallback planner stopped without recommending another route.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackStopReason {
    /// The default policy is disabled.
    Disabled,
    /// An explicit manual provider/model selection cannot be replaced.
    ManualSelection,
    /// The task or caller cancelled fallback.
    Cancelled,
    /// The caller is not at a safe task/turn boundary.
    UnsafeBoundary,
    /// Provider completion is ambiguous, such as a timeout or lost response.
    AmbiguousCompletion,
    /// Provider output or a tool call was partial.
    PartialResponse,
    /// The typed failure is not eligible for fallback, including auth rejection.
    FailureNotRetryable,
    /// The code/status/completion evidence is inconsistent or insufficient.
    InvalidFailureEvidence,
    /// The task reached its configured additional-attempt limit.
    AttemptsExhausted,
    /// The task reached its aggregate provider-request limit.
    RequestBudgetExhausted,
    /// Aggregate prior cost is unknown while the cost ceiling is active.
    UnknownAggregateCost,
    /// Aggregate estimated cost already exceeds the configured ceiling.
    CostBudgetExhausted,
    /// Attempt history is empty, duplicated, or does not end at the current route.
    InvalidAttemptHistory,
    /// Aggregate usage is inconsistent with the current failure chain.
    InvalidUsage,
    /// The current automatic route has no provider identity.
    CurrentProviderUnknown,
    /// No untried route remains under the current provider and attempt limits.
    NoUntriedRoute,
    /// Only other-provider routes remain and data transfer was not authorized.
    CrossProviderTransferNotAuthorized,
    /// The configured router returned no eligible route.
    NoCandidate(NoCandidate),
    /// A replaceable router returned a route outside the supplied eligible candidates.
    InvalidRouterSelection,
    /// A replaceable router selected a candidate rejected by deterministic safety filters.
    RouterSelectedIneligibleCandidate(NoCandidate),
    /// A request may still be in flight or its result needs host reconciliation.
    UnresolvedProviderRequest,
    /// The host has not durably reconciled the previous action and observations.
    ActionHandoffNotReconciled,
    /// The host could not persist the request checkpoint safely.
    CheckpointPersistenceFailed,
}

/// Result of planning one fallback step.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackDecision {
    /// A bounded, reason-coded route can be attempted after cancellable backoff.
    Switch(FallbackPlan),
    /// No further provider request is safe or within configured limits.
    Stop(FallbackStopReason),
}

/// Plan one fallback route without performing a provider call or task action.
pub fn plan_fallback(
    policy: &FallbackPolicy,
    context: &FallbackContext<'_>,
    router: &dyn ModelRouter,
) -> FallbackDecision {
    let Some(limits) = policy.limits else {
        return stop(FallbackStopReason::Disabled);
    };
    if context.current_selection.reason == SelectionReasonCode::ManualOverride
        || context
            .attempted_routes
            .iter()
            .any(|route| route.reason == SelectionReasonCode::ManualOverride)
    {
        return stop(FallbackStopReason::ManualSelection);
    }
    if context.cancelled {
        return stop(FallbackStopReason::Cancelled);
    }
    if !matches!(
        context.boundary,
        FallbackBoundary::InitialTaskStart | FallbackBoundary::BeforeActionDispatch
    ) {
        return stop(FallbackStopReason::UnsafeBoundary);
    }

    let switch_reason = match classify_failure(context.failure) {
        Ok(reason) => reason,
        Err(reason) => return stop(reason),
    };
    let Some(current_provider) = context.current_selection.provider.as_deref() else {
        return stop(FallbackStopReason::CurrentProviderUnknown);
    };
    if current_provider.is_empty()
        || context.attempted_routes.is_empty()
        || context.attempted_routes.last() != Some(context.current_selection)
    {
        return stop(FallbackStopReason::InvalidAttemptHistory);
    }

    let mut attempted_identities = HashSet::new();
    for route in context.attempted_routes {
        let Some(provider) = route.provider.as_deref() else {
            return stop(FallbackStopReason::InvalidAttemptHistory);
        };
        if provider.is_empty()
            || !attempted_identities.insert((provider.to_owned(), route.model.clone()))
        {
            return stop(FallbackStopReason::InvalidAttemptHistory);
        }
    }
    let additional_attempts_used = context.attempted_routes.len().saturating_sub(1);
    if additional_attempts_used >= usize::from(limits.max_additional_attempts) {
        return stop(FallbackStopReason::AttemptsExhausted);
    }
    if usize::from(context.usage.total_provider_requests) < context.attempted_routes.len() {
        return stop(FallbackStopReason::InvalidUsage);
    }
    if context.usage.total_provider_requests >= limits.max_total_provider_requests {
        return stop(FallbackStopReason::RequestBudgetExhausted);
    }
    let Some(estimated_cost_so_far) = context.usage.total_estimated_cost_micro_usd else {
        return stop(FallbackStopReason::UnknownAggregateCost);
    };
    if estimated_cost_so_far > limits.max_total_estimated_cost_micro_usd {
        return stop(FallbackStopReason::CostBudgetExhausted);
    }
    let remaining_budget = limits
        .max_total_estimated_cost_micro_usd
        .saturating_sub(estimated_cost_so_far);

    let mut routing_request = context.request.clone();
    routing_request.maximum_estimated_cost_micro_usd = Some(
        routing_request
            .maximum_estimated_cost_micro_usd
            .map_or(remaining_budget, |per_request_limit| {
                per_request_limit.min(remaining_budget)
            }),
    );

    let mut candidates = Vec::new();
    let mut has_untried_same_provider = false;
    let mut has_untried_other_provider = false;
    for candidate in context.candidates {
        if attempted_identities.contains(&(candidate.provider.clone(), candidate.model.id.clone()))
        {
            continue;
        }
        if candidate.provider == current_provider {
            has_untried_same_provider = true;
            candidates.push(candidate.clone());
        } else {
            has_untried_other_provider = true;
            if limits.allow_cross_provider_data_transfer {
                candidates.push(candidate.clone());
            }
        }
    }
    if candidates.is_empty() {
        return if has_untried_other_provider && !has_untried_same_provider {
            stop(FallbackStopReason::CrossProviderTransferNotAuthorized)
        } else {
            stop(FallbackStopReason::NoUntriedRoute)
        };
    }

    let request = RoutingRequest::Automatic(routing_request.clone());
    let decision = router.route(&request, &candidates);
    let selection = match decision {
        RoutingDecision::Selected(selection) => selection,
        RoutingDecision::NoCandidate(no_candidate) => {
            return stop(FallbackStopReason::NoCandidate(no_candidate));
        }
    };
    let Some(provider) = selection.provider.clone() else {
        return stop(FallbackStopReason::InvalidRouterSelection);
    };
    if selection.reason == SelectionReasonCode::ManualOverride
        || attempted_identities.contains(&(provider.clone(), selection.model.clone()))
    {
        return stop(FallbackStopReason::InvalidRouterSelection);
    }
    let mut matching_candidates = candidates.iter().filter(|candidate| {
        candidate.provider == provider && candidate.model.id == selection.model
    });
    let Some(candidate) = matching_candidates.next() else {
        return stop(FallbackStopReason::InvalidRouterSelection);
    };
    if matching_candidates.next().is_some() {
        return stop(FallbackStopReason::InvalidRouterSelection);
    }
    if provider != current_provider && !limits.allow_cross_provider_data_transfer {
        return stop(FallbackStopReason::InvalidRouterSelection);
    }
    let validation = DeterministicModelRouter.route(
        &RoutingRequest::Automatic(routing_request),
        std::slice::from_ref(candidate),
    );
    if let RoutingDecision::NoCandidate(no_candidate) = validation {
        return stop(FallbackStopReason::RouterSelectedIneligibleCandidate(
            no_candidate,
        ));
    }
    let Some(estimated_cost) = candidate.estimated_cost_micro_usd else {
        return stop(FallbackStopReason::InvalidRouterSelection);
    };
    let Some(remaining_after_request) = remaining_budget.checked_sub(estimated_cost) else {
        return stop(FallbackStopReason::InvalidRouterSelection);
    };

    FallbackDecision::Switch(FallbackPlan {
        selection,
        reason: switch_reason,
        backoff: backoff_for_attempt(limits, additional_attempts_used),
        crosses_provider: provider != current_provider,
        estimated_cost_micro_usd: estimated_cost,
        remaining_estimated_cost_micro_usd: remaining_after_request,
    })
}

fn classify_failure(failure: FallbackFailure) -> Result<FallbackSwitchReason, FallbackStopReason> {
    match failure.completion {
        ProviderCompletion::Complete => return Err(FallbackStopReason::InvalidFailureEvidence),
        ProviderCompletion::Ambiguous => return Err(FallbackStopReason::AmbiguousCompletion),
        ProviderCompletion::Partial => return Err(FallbackStopReason::PartialResponse),
        ProviderCompletion::ExplicitHttpError => {}
    }
    match (failure.code, failure.http_status) {
        (DecisionFailureCode::ProviderRateLimited, Some(429)) => {
            Ok(FallbackSwitchReason::RateLimited)
        }
        (DecisionFailureCode::ProviderUnavailable, Some(500..=599)) => {
            Ok(FallbackSwitchReason::ServerFailure)
        }
        (DecisionFailureCode::ProviderUnavailable, None | Some(408 | 425)) => {
            Err(FallbackStopReason::AmbiguousCompletion)
        }
        (DecisionFailureCode::ProviderRejected, _) => Err(FallbackStopReason::FailureNotRetryable),
        (
            DecisionFailureCode::ProviderRateLimited | DecisionFailureCode::ProviderUnavailable,
            _,
        ) => Err(FallbackStopReason::InvalidFailureEvidence),
        _ => Err(FallbackStopReason::FailureNotRetryable),
    }
}

fn backoff_for_attempt(limits: FallbackLimits, prior_fallbacks: usize) -> Duration {
    let mut delay = limits.initial_backoff;
    for _ in 0..prior_fallbacks {
        delay = delay.saturating_mul(2).min(limits.maximum_backoff);
    }
    delay.min(limits.maximum_backoff)
}

fn stop(reason: FallbackStopReason) -> FallbackDecision {
    FallbackDecision::Stop(reason)
}

#[cfg(test)]
#[path = "fallback/tests.rs"]
mod tests;

mod runtime;

pub use runtime::{
    FallbackCheckpoint, FallbackCheckpointState, FallbackDecisionSource,
    FallbackInitializationError, FallbackProviderResponse, FallbackRuntime, FallbackTaskState,
};
