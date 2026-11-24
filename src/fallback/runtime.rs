//! Runtime owner for one provider decision and its bounded fallback attempts.

use super::{
    plan_fallback, FallbackBoundary, FallbackContext, FallbackDecision, FallbackFailure,
    FallbackPolicy, FallbackStopReason, FallbackUsage, ProviderCompletion,
};
use crate::agent_loop::{
    BoundedDecisionSource, DecisionFailureCode, WorkspaceAction, WorkspaceObservation,
};
use crate::routing::{
    AutomaticRoutingRequest, DeterministicModelRouter, ModelRouter, RoutingCandidate,
    RoutingDecision, RoutingRequest, RoutingSelection, SelectionReasonCode,
};
use std::time::Duration;

/// Result returned by one provider call before Harness dispatches any action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackProviderResponse {
    action: WorkspaceAction,
    completion: ProviderCompletion,
}

impl FallbackProviderResponse {
    /// A complete provider decision, including a complete typed decision error.
    pub const fn complete(action: WorkspaceAction) -> Self {
        Self {
            action,
            completion: ProviderCompletion::Complete,
        }
    }

    /// An unsuccessful provider response and its transport/completion evidence.
    pub const fn failed(action: WorkspaceAction, completion: ProviderCompletion) -> Self {
        Self { action, completion }
    }

    /// The proposed action or typed provider failure.
    pub const fn action(&self) -> &WorkspaceAction {
        &self.action
    }

    /// Whether the provider returned a complete decision or failed before one.
    pub const fn completion(&self) -> ProviderCompletion {
        self.completion
    }
}

/// Host operations needed to make a route-specific provider request safely.
///
/// Implementations own credentials and provider construction. `wait_cancellable`
/// must return false when cancelled or interrupted. The caller rechecks
/// `is_cancelled` after it returns and before issuing another request. `decide`
/// must also check cancellation immediately before dispatching its provider
/// request, to close the race between the caller's check and transport access.
pub trait FallbackRuntime {
    /// Whether this provider source belongs to a new task or a restored task.
    fn task_state(&self) -> FallbackTaskState;

    /// Load the checkpoint scoped to the owning task, if one exists.
    fn load_checkpoint(&mut self) -> Option<FallbackCheckpoint>;

    /// Persist the exact routing request, candidate evidence, fallback policy,
    /// and optional task-stable routing-strategy identity for a new task. The
    /// host stores this snapshot with its task identity, separately from task
    /// transcripts and the small fallback usage checkpoint.
    fn persist_routing_snapshot(
        &mut self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool;

    /// Confirm that the supplied routing inputs, fallback policy, and optional
    /// routing-strategy identity exactly match the task's persisted snapshot
    /// before a resumed task can make another request.
    fn routing_snapshot_matches(
        &self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool;

    /// Persist task-scoped usage and request state before or after each call.
    fn persist_checkpoint(&mut self, checkpoint: &FallbackCheckpoint) -> bool;

    /// Confirm the previous returned action and its observations are durably
    /// recorded in the task's authoritative history. Return `true` only after
    /// that reconciliation is complete. This method may be called again if the
    /// subsequent checkpoint write fails, so implementations must be idempotent.
    /// Do not copy action or observation payloads into the fallback checkpoint.
    /// The default denies reconciliation and keeps the next provider request
    /// blocked.
    fn reconcile_action_handoff(
        &mut self,
        _action: &WorkspaceAction,
        _observations: &[WorkspaceObservation],
    ) -> bool {
        false
    }

    /// Mark that the host completed its task-turn boundary after receiving and
    /// recording the returned action. The default keeps the observation check
    /// required between decisions.
    fn confirm_turn_boundary(&mut self) {}

    /// Whether the owning task has been cancelled.
    fn is_cancelled(&self) -> bool;

    /// Wait for a bounded delay, returning false if cancelled or interrupted.
    fn wait_cancellable(&mut self, delay: Duration) -> bool;

    /// Request one decision from exactly the selected route.
    ///
    /// The host must dispatch through Krypper's trusted provider adapter and
    /// enforce its normal decision budget. This trait is an execution seam; the
    /// fallback source cannot enforce the host's transport implementation.
    fn decide(
        &mut self,
        selection: &RoutingSelection,
        goal: &str,
        observations: &[WorkspaceObservation],
        root_project_guidance: Option<&str>,
    ) -> FallbackProviderResponse;

    /// Whether the provider builds context from root project guidance.
    fn needs_root_project_guidance(&self) -> bool {
        false
    }
}

/// A task-scoped decision source that executes the bounded fallback plan.
///
/// Keep one instance for the lifetime of one task. A resumed task must load a
/// host-persisted checkpoint only after reconciling its authoritative session
/// and verifying the exact routing request/candidate/policy snapshot and
/// routing-strategy identity saved for that task. The task host supplies one authoritative goal and observation slice
/// per decision; this source forwards that exact context to every provider retry.
/// The host must reconcile the returned action with task history before the
/// next decision or after a restart. A complete provider result is never
/// retried by this decision chain.
pub struct FallbackDecisionSource<R, M = DeterministicModelRouter> {
    policy: FallbackPolicy,
    request: AutomaticRoutingRequest,
    checkpoint: FallbackCheckpoint,
    last_persisted_checkpoint: FallbackCheckpoint,
    pending_action: Option<WorkspaceAction>,
    candidates: Vec<RoutingCandidate>,
    runtime: R,
    router: M,
    last_stop_reason: Option<FallbackStopReason>,
}

/// Whether the host has started a new task or restored authoritative task state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FallbackTaskState {
    /// A newly created task with no previous provider request.
    #[default]
    New,
    /// A restored task with reconciled lifecycle/observations and any pending
    /// action handoff, matching routing inputs, policy, routing-strategy
    /// identity, and no active prior decision chain.
    ResumedReconciled,
    /// A restored task whose prior effects, observations, or action handoff are
    /// not reconciled.
    ResumedUnreconciled,
}

/// Durable state of the last provider request for one task.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackCheckpointState {
    /// No provider request or action handoff is unresolved.
    Ready,
    /// A request was reserved and the process may have stopped before recording its result.
    RequestInFlight,
    /// A definitive result was recorded, but this decision has not returned an action to its host.
    DecisionInProgress,
    /// The action is about to be returned; the host has not durably reconciled its handoff yet.
    ActionPendingReconciliation,
    /// The provider result was ambiguous or partial and requires host reconciliation.
    ReconciliationRequired,
}

/// Host-persisted fallback state bound to one task.
///
/// The host should persist this beside its task session and separately persist
/// the exact routing snapshot, fallback policy, and routing-strategy identity.
/// A resumed task may use `Ready` only after the host has reconciled task state,
/// confirmed that the supplied routing inputs, policy, and strategy identity
/// match that snapshot, and established that no previous decision chain or
/// action handoff is still active. A pending action handoff is accepted only
/// after the host reports reconciliation, at which point it is committed as
/// `Ready`. `RequestInFlight`, `DecisionInProgress`, and
/// `ReconciliationRequired` fail closed on resume.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackCheckpoint {
    usage: FallbackUsage,
    current_selection: RoutingSelection,
    state: FallbackCheckpointState,
}

impl FallbackCheckpoint {
    /// Restore a checkpoint from the host's task-scoped persistent store.
    pub const fn restore(
        usage: FallbackUsage,
        current_selection: RoutingSelection,
        state: FallbackCheckpointState,
    ) -> Self {
        Self {
            usage,
            current_selection,
            state,
        }
    }

    /// Aggregate requests and estimated cost reserved for this task.
    pub const fn usage(&self) -> FallbackUsage {
        self.usage
    }

    /// Route that was active when the checkpoint was recorded.
    pub const fn current_selection(&self) -> &RoutingSelection {
        &self.current_selection
    }

    /// Whether the previous provider request is safe to continue from.
    pub const fn state(&self) -> FallbackCheckpointState {
        self.state
    }
}

/// Why a task-scoped fallback source could not be initialized safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FallbackInitializationError {
    /// The source was restored without a corresponding task checkpoint.
    MissingResumeCheckpoint,
    /// The host had not reconciled the restored task's state and observations.
    TaskStateNotReconciled,
    /// The checkpoint records a provider request that may be unresolved.
    UnresolvedProviderRequest,
    /// A new task unexpectedly reused an existing fallback checkpoint.
    UnexpectedCheckpointForNewTask,
    /// The routing request, candidates, and policy could not be persisted for a new task.
    RoutingSnapshotPersistenceFailed,
    /// A resumed task's routing inputs, policy, or strategy identity differ from its snapshot.
    RoutingSnapshotMismatch,
    /// Automatic fallback requires a router with a stable identity across restarts.
    RoutingStrategyIdentityMissing,
    /// The initial checkpoint could not be persisted before provider access.
    CheckpointPersistenceFailed,
}

impl std::fmt::Display for FallbackInitializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingResumeCheckpoint => "resumed task has no fallback checkpoint",
            Self::TaskStateNotReconciled => "resumed task state has not been reconciled",
            Self::UnresolvedProviderRequest => {
                "previous provider request, decision, or action handoff requires reconciliation"
            }
            Self::UnexpectedCheckpointForNewTask => {
                "new task cannot reuse another task's fallback checkpoint"
            }
            Self::RoutingSnapshotPersistenceFailed => "cannot persist task routing snapshot",
            Self::RoutingSnapshotMismatch => {
                "resumed task routing inputs differ from its persisted snapshot"
            }
            Self::RoutingStrategyIdentityMissing => {
                "automatic fallback requires a stable routing strategy identity"
            }
            Self::CheckpointPersistenceFailed => "cannot persist fallback checkpoint",
        })
    }
}

impl std::error::Error for FallbackInitializationError {}

impl<R> FallbackDecisionSource<R, DeterministicModelRouter>
where
    R: FallbackRuntime,
{
    /// Initialize a new source or restore one from the host's reconciled checkpoint.
    pub fn new(
        policy: FallbackPolicy,
        request: AutomaticRoutingRequest,
        initial_selection: RoutingSelection,
        candidates: Vec<RoutingCandidate>,
        runtime: R,
    ) -> Result<Self, FallbackInitializationError> {
        Self::with_router(
            policy,
            request,
            initial_selection,
            candidates,
            runtime,
            DeterministicModelRouter,
        )
    }
}

impl<R, M> FallbackDecisionSource<R, M>
where
    R: FallbackRuntime,
    M: ModelRouter,
{
    /// Create a task-scoped source with a replaceable ranking policy.
    pub fn with_router(
        policy: FallbackPolicy,
        request: AutomaticRoutingRequest,
        initial_selection: RoutingSelection,
        candidates: Vec<RoutingCandidate>,
        mut runtime: R,
        router: M,
    ) -> Result<Self, FallbackInitializationError> {
        let (checkpoint, last_persisted_checkpoint) = match runtime.task_state() {
            FallbackTaskState::New => {
                if runtime.load_checkpoint().is_some() {
                    return Err(FallbackInitializationError::UnexpectedCheckpointForNewTask);
                }
                let routing_strategy_id =
                    task_routing_strategy_id(policy, &initial_selection, &router)?;
                if !runtime.persist_routing_snapshot(
                    &request,
                    &candidates,
                    &policy,
                    routing_strategy_id.as_deref(),
                ) {
                    return Err(FallbackInitializationError::RoutingSnapshotPersistenceFailed);
                }
                let checkpoint = FallbackCheckpoint::restore(
                    empty_usage(),
                    initial_selection,
                    FallbackCheckpointState::Ready,
                );
                if !runtime.persist_checkpoint(&checkpoint) {
                    return Err(FallbackInitializationError::CheckpointPersistenceFailed);
                }
                (checkpoint.clone(), checkpoint)
            }
            FallbackTaskState::ResumedUnreconciled => {
                return Err(FallbackInitializationError::TaskStateNotReconciled);
            }
            FallbackTaskState::ResumedReconciled => {
                let mut checkpoint = runtime
                    .load_checkpoint()
                    .ok_or(FallbackInitializationError::MissingResumeCheckpoint)?;
                if !matches!(
                    checkpoint.state,
                    FallbackCheckpointState::Ready
                        | FallbackCheckpointState::ActionPendingReconciliation
                ) {
                    return Err(FallbackInitializationError::UnresolvedProviderRequest);
                }
                let routing_strategy_id =
                    task_routing_strategy_id(policy, &checkpoint.current_selection, &router)?;
                if !runtime.routing_snapshot_matches(
                    &request,
                    &candidates,
                    &policy,
                    routing_strategy_id.as_deref(),
                ) {
                    return Err(FallbackInitializationError::RoutingSnapshotMismatch);
                }
                if checkpoint.state == FallbackCheckpointState::ActionPendingReconciliation {
                    checkpoint.state = FallbackCheckpointState::Ready;
                    if !runtime.persist_checkpoint(&checkpoint) {
                        return Err(FallbackInitializationError::CheckpointPersistenceFailed);
                    }
                }
                (checkpoint.clone(), checkpoint)
            }
        };
        Ok(Self {
            policy,
            request,
            checkpoint,
            last_persisted_checkpoint,
            pending_action: None,
            candidates,
            runtime,
            router,
            last_stop_reason: None,
        })
    }

    /// Aggregate requests and estimated cost reserved by this task so far.
    pub const fn usage(&self) -> FallbackUsage {
        self.checkpoint.usage
    }

    /// Route selected for the next decision request.
    pub const fn current_selection(&self) -> &RoutingSelection {
        &self.checkpoint.current_selection
    }

    /// Latest checkpoint whose persistence the host runtime confirmed.
    ///
    /// The checkpoint remains `ActionPendingReconciliation` after an action is
    /// returned. It becomes `Ready` only after the host confirms reconciliation
    /// and the source successfully persists the transition.
    pub const fn persisted_checkpoint(&self) -> &FallbackCheckpoint {
        &self.last_persisted_checkpoint
    }

    /// Current in-memory checkpoint for the live decision chain. This may be
    /// ahead of [`Self::persisted_checkpoint`] when a checkpoint write fails.
    pub const fn current_checkpoint(&self) -> &FallbackCheckpoint {
        &self.checkpoint
    }

    /// Why the most recent decision could not continue fallback, if it stopped.
    pub fn last_stop_reason(&self) -> Option<&FallbackStopReason> {
        self.last_stop_reason.as_ref()
    }

    /// Mutable access to the host runtime for status or shutdown handling.
    pub const fn runtime_mut(&mut self) -> &mut R {
        &mut self.runtime
    }

    /// Confirm that the last returned action was accepted by the host's completed
    /// task turn. Call only after the authoritative task loop has returned.
    pub fn reconcile_completed_turn(
        &mut self,
        observations: &[WorkspaceObservation],
    ) -> Result<(), FallbackStopReason> {
        self.runtime.confirm_turn_boundary();
        if self.checkpoint.state == FallbackCheckpointState::ActionPendingReconciliation {
            self.reconcile_pending_action(observations)
        } else if self.checkpoint.state == FallbackCheckpointState::Ready {
            Ok(())
        } else {
            Err(FallbackStopReason::UnresolvedProviderRequest)
        }
    }

    fn decide_with_context(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        root_project_guidance: Option<&str>,
    ) -> WorkspaceAction {
        self.last_stop_reason = None;
        if self.runtime.is_cancelled() {
            return self.cancelled_action();
        }

        if self.checkpoint.state == FallbackCheckpointState::ActionPendingReconciliation {
            if let Err(reason) = self.reconcile_pending_action(observations) {
                return self.stopped_action(reason);
            }
        }
        if self.checkpoint.state != FallbackCheckpointState::Ready {
            return self.stopped_action(FallbackStopReason::UnresolvedProviderRequest);
        }

        let policy_enabled = self.policy.is_enabled();
        let fallback_enabled = policy_enabled
            && self.checkpoint.current_selection.reason != SelectionReasonCode::ManualOverride;
        if policy_enabled {
            if let Err(reason) = self.reserve_request(&self.checkpoint.current_selection.clone()) {
                return self.stopped_action(reason);
            }
        }

        if let Err(reason) = self.begin_provider_request(&self.checkpoint.current_selection.clone())
        {
            return self.stopped_action(reason);
        }
        if self.runtime.is_cancelled() {
            return self.cancelled_before_provider_request();
        }

        let mut response = self.runtime.decide(
            &self.checkpoint.current_selection,
            goal,
            observations,
            root_project_guidance,
        );
        let completion_result = self.finish_provider_request(&response);
        if self.runtime.is_cancelled() {
            let action = self.cancelled_action();
            return self.finish_decision(action);
        }
        if let Err(reason) = completion_result {
            return self.stopped_action(reason);
        }
        if response.completion == ProviderCompletion::ExplicitHttpError
            && (!matches!(response.action, WorkspaceAction::Unsupported { .. })
                || response.action.decision_failure_code().is_none())
        {
            let action = self.stopped_action(FallbackStopReason::InvalidFailureEvidence);
            return self.finish_decision(action);
        }
        if !fallback_enabled {
            return self.finish_decision(response.action);
        }

        let mut attempted_routes = vec![self.checkpoint.current_selection.clone()];
        loop {
            if response.completion == ProviderCompletion::Complete {
                if response.action.decision_failure_code().is_some() {
                    self.last_stop_reason = Some(FallbackStopReason::InvalidFailureEvidence);
                }
                return self.finish_decision(response.action);
            }
            if !matches!(response.action, WorkspaceAction::Unsupported { .. })
                || response.action.decision_failure_code().is_none()
            {
                let reason = match response.completion {
                    ProviderCompletion::Partial => FallbackStopReason::PartialResponse,
                    ProviderCompletion::Ambiguous => FallbackStopReason::AmbiguousCompletion,
                    ProviderCompletion::ExplicitHttpError | ProviderCompletion::Complete => {
                        FallbackStopReason::InvalidFailureEvidence
                    }
                };
                let action = self.stopped_action(reason);
                return self.finish_decision(action);
            }

            let Some(failure) = FallbackFailure::from_action(&response.action, response.completion)
            else {
                self.last_stop_reason = Some(FallbackStopReason::FailureNotRetryable);
                return self.finish_decision(response.action);
            };
            let context = FallbackContext {
                request: &self.request,
                current_selection: &self.checkpoint.current_selection,
                attempted_routes: &attempted_routes,
                candidates: &self.candidates,
                boundary: FallbackBoundary::BeforeActionDispatch,
                cancelled: self.runtime.is_cancelled(),
                failure,
                usage: self.checkpoint.usage,
            };
            let plan = match plan_fallback(&self.policy, &context, &self.router) {
                FallbackDecision::Switch(plan) => plan,
                FallbackDecision::Stop(reason) => {
                    self.last_stop_reason = Some(reason);
                    return self.finish_decision(response.action);
                }
            };

            if self.runtime.is_cancelled() || !self.runtime.wait_cancellable(plan.backoff) {
                let action = self.cancelled_action();
                return self.finish_decision(action);
            }
            if self.runtime.is_cancelled() {
                let action = self.cancelled_action();
                return self.finish_decision(action);
            }
            if let Err(reason) =
                self.reserve_planned_request(&plan.selection, plan.estimated_cost_micro_usd)
            {
                let action = self.stopped_action(reason);
                return self.finish_decision(action);
            }

            self.checkpoint.current_selection = plan.selection;
            if let Err(reason) =
                self.begin_provider_request(&self.checkpoint.current_selection.clone())
            {
                return self.stopped_action(reason);
            }
            if self.runtime.is_cancelled() {
                return self.cancelled_before_provider_request();
            }
            attempted_routes.push(self.checkpoint.current_selection.clone());
            response = self.runtime.decide(
                &self.checkpoint.current_selection,
                goal,
                observations,
                root_project_guidance,
            );
            let completion_result = self.finish_provider_request(&response);
            if self.runtime.is_cancelled() {
                let action = self.cancelled_action();
                return self.finish_decision(action);
            }
            if let Err(reason) = completion_result {
                return self.stopped_action(reason);
            }
        }
    }

    fn reserve_request(&mut self, selection: &RoutingSelection) -> Result<(), FallbackStopReason> {
        let Some(limits) = self.policy.limits() else {
            return Ok(());
        };
        let estimated_cost = if selection.reason == SelectionReasonCode::ManualOverride {
            self.manual_candidate_cost(selection)?
        } else {
            self.validated_candidate(selection)?
                .estimated_cost_micro_usd
                .ok_or(FallbackStopReason::UnknownAggregateCost)?
        };
        self.reserve_usage(
            estimated_cost,
            limits.max_total_provider_requests,
            limits.max_total_estimated_cost_micro_usd,
        )
    }

    fn reserve_planned_request(
        &mut self,
        selection: &RoutingSelection,
        planned_cost: u64,
    ) -> Result<(), FallbackStopReason> {
        let Some(limits) = self.policy.limits() else {
            return Err(FallbackStopReason::Disabled);
        };
        let candidate = self.validated_candidate(selection)?;
        if candidate.estimated_cost_micro_usd != Some(planned_cost) {
            return Err(FallbackStopReason::InvalidRouterSelection);
        }
        self.reserve_usage(
            planned_cost,
            limits.max_total_provider_requests,
            limits.max_total_estimated_cost_micro_usd,
        )
    }

    fn reserve_usage(
        &mut self,
        estimated_cost: u64,
        request_limit: u8,
        cost_limit: u64,
    ) -> Result<(), FallbackStopReason> {
        if self.checkpoint.usage.total_provider_requests >= request_limit {
            return Err(FallbackStopReason::RequestBudgetExhausted);
        }
        let prior_cost = self
            .checkpoint
            .usage
            .total_estimated_cost_micro_usd
            .ok_or(FallbackStopReason::UnknownAggregateCost)?;
        let total_cost = prior_cost
            .checked_add(estimated_cost)
            .filter(|total| *total <= cost_limit)
            .ok_or(FallbackStopReason::CostBudgetExhausted)?;
        self.checkpoint.usage.total_provider_requests += 1;
        self.checkpoint.usage.total_estimated_cost_micro_usd = Some(total_cost);
        Ok(())
    }

    fn begin_provider_request(
        &mut self,
        selection: &RoutingSelection,
    ) -> Result<(), FallbackStopReason> {
        if !matches!(
            self.checkpoint.state,
            FallbackCheckpointState::Ready | FallbackCheckpointState::DecisionInProgress
        ) {
            return Err(FallbackStopReason::UnresolvedProviderRequest);
        }
        self.checkpoint.current_selection = selection.clone();
        self.checkpoint.state = FallbackCheckpointState::RequestInFlight;
        if !self.runtime.persist_checkpoint(&self.checkpoint) {
            return Err(FallbackStopReason::CheckpointPersistenceFailed);
        }
        self.last_persisted_checkpoint = self.checkpoint.clone();
        Ok(())
    }

    fn finish_provider_request(
        &mut self,
        response: &FallbackProviderResponse,
    ) -> Result<(), FallbackStopReason> {
        let completion = response.completion;
        let reconciliation_reason = match completion {
            ProviderCompletion::Partial => Some(FallbackStopReason::PartialResponse),
            ProviderCompletion::Ambiguous => Some(FallbackStopReason::AmbiguousCompletion),
            ProviderCompletion::ExplicitHttpError
                if FallbackFailure::from_action(&response.action, completion)
                    .is_some_and(FallbackFailure::requires_reconciliation) =>
            {
                Some(FallbackStopReason::AmbiguousCompletion)
            }
            ProviderCompletion::Complete | ProviderCompletion::ExplicitHttpError => None,
        };
        self.checkpoint.state = if reconciliation_reason.is_some() {
            FallbackCheckpointState::ReconciliationRequired
        } else {
            FallbackCheckpointState::DecisionInProgress
        };
        if !self.runtime.persist_checkpoint(&self.checkpoint) {
            return Err(FallbackStopReason::CheckpointPersistenceFailed);
        }
        self.last_persisted_checkpoint = self.checkpoint.clone();
        if let Some(reason) = reconciliation_reason {
            Err(reason)
        } else {
            Ok(())
        }
    }

    fn finish_decision(&mut self, action: WorkspaceAction) -> WorkspaceAction {
        if self.checkpoint.state == FallbackCheckpointState::DecisionInProgress {
            let mut pending_checkpoint = self.checkpoint.clone();
            pending_checkpoint.state = FallbackCheckpointState::ActionPendingReconciliation;
            if !self.runtime.persist_checkpoint(&pending_checkpoint) {
                return self.stopped_action(FallbackStopReason::CheckpointPersistenceFailed);
            }
            self.last_persisted_checkpoint = pending_checkpoint.clone();
            self.checkpoint = pending_checkpoint;
            self.pending_action = Some(action.clone());
        }
        action
    }

    fn reconcile_pending_action(
        &mut self,
        observations: &[WorkspaceObservation],
    ) -> Result<(), FallbackStopReason> {
        let action = self
            .pending_action
            .as_ref()
            .ok_or(FallbackStopReason::UnresolvedProviderRequest)?;
        if !self.runtime.reconcile_action_handoff(action, observations) {
            return Err(FallbackStopReason::ActionHandoffNotReconciled);
        }

        let mut ready_checkpoint = self.checkpoint.clone();
        ready_checkpoint.state = FallbackCheckpointState::Ready;
        if !self.runtime.persist_checkpoint(&ready_checkpoint) {
            return Err(FallbackStopReason::CheckpointPersistenceFailed);
        }
        self.last_persisted_checkpoint = ready_checkpoint.clone();
        self.checkpoint = ready_checkpoint;
        self.pending_action = None;
        Ok(())
    }

    fn cancelled_before_provider_request(&mut self) -> WorkspaceAction {
        let mut ready_checkpoint = self.checkpoint.clone();
        ready_checkpoint.state = FallbackCheckpointState::Ready;
        if !self.runtime.persist_checkpoint(&ready_checkpoint) {
            return self.stopped_action(FallbackStopReason::CheckpointPersistenceFailed);
        }
        self.last_persisted_checkpoint = ready_checkpoint.clone();
        self.checkpoint = ready_checkpoint;
        self.cancelled_action()
    }

    fn validated_candidate(
        &self,
        selection: &RoutingSelection,
    ) -> Result<&RoutingCandidate, FallbackStopReason> {
        let Some(provider) = selection.provider.as_deref() else {
            return Err(FallbackStopReason::CurrentProviderUnknown);
        };
        let mut matching = self.candidates.iter().filter(|candidate| {
            candidate.provider == provider && candidate.model.id == selection.model
        });
        let candidate = matching
            .next()
            .ok_or(FallbackStopReason::InvalidRouterSelection)?;
        if matching.next().is_some() {
            return Err(FallbackStopReason::InvalidRouterSelection);
        }
        match DeterministicModelRouter.route(
            &RoutingRequest::Automatic(self.request.clone()),
            std::slice::from_ref(candidate),
        ) {
            RoutingDecision::Selected(valid)
                if valid.provider.as_deref() == Some(provider)
                    && valid.model == selection.model =>
            {
                Ok(candidate)
            }
            _ => Err(FallbackStopReason::InvalidRouterSelection),
        }
    }

    fn manual_candidate_cost(
        &self,
        selection: &RoutingSelection,
    ) -> Result<u64, FallbackStopReason> {
        let Some(provider) = selection.provider.as_deref() else {
            return Err(FallbackStopReason::UnknownAggregateCost);
        };
        let mut matching = self.candidates.iter().filter(|candidate| {
            candidate.provider == provider && candidate.model.id == selection.model
        });
        let Some(candidate) = matching.next() else {
            return Err(FallbackStopReason::UnknownAggregateCost);
        };
        if matching.next().is_some() {
            return Err(FallbackStopReason::UnknownAggregateCost);
        }
        candidate
            .estimated_cost_micro_usd
            .ok_or(FallbackStopReason::UnknownAggregateCost)
    }

    fn cancelled_action(&mut self) -> WorkspaceAction {
        self.last_stop_reason = Some(FallbackStopReason::Cancelled);
        WorkspaceAction::Cancel
    }

    fn stopped_action(&mut self, reason: FallbackStopReason) -> WorkspaceAction {
        let code = match reason {
            FallbackStopReason::RequestBudgetExhausted
            | FallbackStopReason::CostBudgetExhausted
            | FallbackStopReason::UnknownAggregateCost => {
                DecisionFailureCode::DecisionBudgetExhausted
            }
            _ => DecisionFailureCode::Unknown,
        };
        self.last_stop_reason = Some(reason);
        WorkspaceAction::Unsupported {
            action: format!("decision_failure:{}", code.as_code()),
        }
    }
}

fn task_routing_strategy_id<M: ModelRouter>(
    policy: FallbackPolicy,
    selection: &RoutingSelection,
    router: &M,
) -> Result<Option<String>, FallbackInitializationError> {
    if !policy.is_enabled() || selection.reason == SelectionReasonCode::ManualOverride {
        return Ok(None);
    }
    router
        .task_stable_id()
        .map(Some)
        .ok_or(FallbackInitializationError::RoutingStrategyIdentityMissing)
}

impl<R, M> BoundedDecisionSource for FallbackDecisionSource<R, M>
where
    R: FallbackRuntime,
    M: ModelRouter,
{
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        self.decide_with_context(goal, observations, None)
    }

    fn needs_root_project_guidance(&self) -> bool {
        self.runtime.needs_root_project_guidance()
    }

    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        guidance: Option<&str>,
    ) -> WorkspaceAction {
        self.decide_with_context(goal, observations, guidance)
    }
}

fn empty_usage() -> FallbackUsage {
    FallbackUsage {
        total_provider_requests: 0,
        total_estimated_cost_micro_usd: Some(0),
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
