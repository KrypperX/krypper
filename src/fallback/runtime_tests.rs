use super::super::FallbackLimits;
use super::*;
use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason};
use crate::routing::{
    CapabilityAssertion, CapabilityEvidenceFreshness, CapabilityName, CapabilityProvenance,
    CapabilitySupport, CapabilityVerification, ProviderReadiness,
};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

fn policy(max_requests: u8, max_cost: u64) -> FallbackPolicy {
    FallbackPolicy::opt_in(FallbackLimits {
        max_additional_attempts: max_requests.saturating_sub(1).min(2),
        max_total_provider_requests: max_requests,
        max_total_estimated_cost_micro_usd: max_cost,
        initial_backoff: Duration::from_millis(10),
        maximum_backoff: Duration::from_millis(20),
        allow_cross_provider_data_transfer: false,
    })
    .expect("valid fallback limits")
}

fn selection(provider: &str, model: &str) -> RoutingSelection {
    RoutingSelection {
        provider: Some(provider.to_owned()),
        model: model.to_owned(),
        reason: SelectionReasonCode::StableTieBreak,
    }
}

fn candidate(provider: &str, model: &str, cost: u64) -> RoutingCandidate {
    RoutingCandidate {
        provider: provider.to_owned(),
        model: crate::catalog_data::ModelSpec {
            provider: provider.to_owned(),
            id: model.to_owned(),
            name: model.to_owned(),
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
        estimated_cost_micro_usd: Some(cost),
        observed_latency_ms: Some(10),
        quality_score_basis_points: Some(9_000),
    }
}

fn candidates() -> Vec<RoutingCandidate> {
    vec![
        candidate("provider-a", "model-1", 10),
        candidate("provider-a", "model-2", 10),
    ]
}

fn failed(status: u16, code: DecisionFailureCode) -> FallbackProviderResponse {
    FallbackProviderResponse::failed(
        WorkspaceAction::Unsupported {
            action: format!("decision_failure:{}:{status}", code.as_code()),
        },
        ProviderCompletion::ExplicitHttpError,
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedCall {
    selection: RoutingSelection,
    goal: String,
    observations: Vec<WorkspaceObservation>,
    guidance: Option<String>,
}

#[derive(Default)]
struct FakeRuntime {
    responses: VecDeque<FallbackProviderResponse>,
    calls: Vec<RecordedCall>,
    waits: Vec<Duration>,
    cancelled: bool,
    cancel_on_wait: bool,
    cancel_on_decide_call: Option<usize>,
    task_state: FallbackTaskState,
    checkpoint: Option<FallbackCheckpoint>,
    saved_routing_request: Option<AutomaticRoutingRequest>,
    saved_routing_candidates: Option<Vec<RoutingCandidate>>,
    saved_fallback_policy: Option<FallbackPolicy>,
    saved_routing_strategy_id: Option<String>,
    fail_snapshot_persist: bool,
    persist_calls: usize,
    fail_persist_on_call: Option<usize>,
    cancel_on_persist_call: Option<usize>,
    persisted_states: Vec<FallbackCheckpointState>,
    refuse_action_reconciliation: bool,
    reconciliation_attempts: Vec<(WorkspaceAction, Vec<WorkspaceObservation>)>,
}

impl FallbackRuntime for FakeRuntime {
    fn task_state(&self) -> FallbackTaskState {
        self.task_state
    }

    fn load_checkpoint(&mut self) -> Option<FallbackCheckpoint> {
        self.checkpoint.clone()
    }

    fn persist_routing_snapshot(
        &mut self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool {
        if self.fail_snapshot_persist {
            return false;
        }
        self.saved_routing_request = Some(request.clone());
        self.saved_routing_candidates = Some(candidates.to_vec());
        self.saved_fallback_policy = Some(*policy);
        self.saved_routing_strategy_id = routing_strategy_id.map(str::to_owned);
        true
    }

    fn routing_snapshot_matches(
        &self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool {
        self.saved_routing_request.as_ref() == Some(request)
            && self.saved_routing_candidates.as_deref() == Some(candidates)
            && self.saved_fallback_policy.as_ref() == Some(policy)
            && self.saved_routing_strategy_id.as_deref() == routing_strategy_id
    }

    fn persist_checkpoint(&mut self, checkpoint: &FallbackCheckpoint) -> bool {
        self.persist_calls += 1;
        if self.fail_persist_on_call == Some(self.persist_calls) {
            return false;
        }
        self.persisted_states.push(checkpoint.state());
        self.checkpoint = Some(checkpoint.clone());
        if self.cancel_on_persist_call == Some(self.persist_calls) {
            self.cancelled = true;
        }
        true
    }

    fn reconcile_action_handoff(
        &mut self,
        action: &WorkspaceAction,
        observations: &[WorkspaceObservation],
    ) -> bool {
        self.reconciliation_attempts
            .push((action.clone(), observations.to_vec()));
        !self.refuse_action_reconciliation
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    fn wait_cancellable(&mut self, delay: Duration) -> bool {
        self.waits.push(delay);
        if self.cancel_on_wait {
            self.cancelled = true;
        }
        !self.cancelled
    }

    fn decide(
        &mut self,
        selection: &RoutingSelection,
        goal: &str,
        observations: &[WorkspaceObservation],
        root_project_guidance: Option<&str>,
    ) -> FallbackProviderResponse {
        self.calls.push(RecordedCall {
            selection: selection.clone(),
            goal: goal.to_owned(),
            observations: observations.to_vec(),
            guidance: root_project_guidance.map(str::to_owned),
        });
        let response = self
            .responses
            .pop_front()
            .expect("provider response fixture");
        if self.cancel_on_decide_call == Some(self.calls.len()) {
            self.cancelled = true;
        }
        response
    }

    fn needs_root_project_guidance(&self) -> bool {
        true
    }
}

fn source(
    runtime: FakeRuntime,
    max_requests: u8,
    max_cost: u64,
) -> FallbackDecisionSource<FakeRuntime> {
    FallbackDecisionSource::new(
        policy(max_requests, max_cost),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        runtime,
    )
    .expect("task checkpoint initializes")
}

fn resume_runtime(checkpoint_state: FallbackCheckpointState) -> FakeRuntime {
    FakeRuntime {
        task_state: FallbackTaskState::ResumedReconciled,
        saved_routing_request: Some(AutomaticRoutingRequest::default()),
        saved_routing_candidates: Some(candidates()),
        saved_fallback_policy: Some(policy(3, 100)),
        saved_routing_strategy_id: Some("deterministic-model-router-v1".to_owned()),
        checkpoint: Some(FallbackCheckpoint::restore(
            FallbackUsage {
                total_provider_requests: 1,
                total_estimated_cost_micro_usd: Some(10),
            },
            selection("provider-a", "model-1"),
            checkpoint_state,
        )),
        ..FakeRuntime::default()
    }
}

fn construct(
    runtime: FakeRuntime,
) -> Result<FallbackDecisionSource<FakeRuntime>, FallbackInitializationError> {
    FallbackDecisionSource::new(
        policy(3, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        runtime,
    )
}

struct UnidentifiedRouter;

impl ModelRouter for UnidentifiedRouter {
    fn route(&self, request: &RoutingRequest, candidates: &[RoutingCandidate]) -> RoutingDecision {
        DeterministicModelRouter.route(request, candidates)
    }
}

#[test]
fn automatic_fallback_requires_a_task_stable_router_identity() {
    let result = FallbackDecisionSource::with_router(
        policy(3, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        FakeRuntime::default(),
        UnidentifiedRouter,
    );

    assert!(matches!(
        result,
        Err(FallbackInitializationError::RoutingStrategyIdentityMissing)
    ));
}

#[test]
fn resumed_task_rejects_unreconciled_or_unresolved_request_state() {
    for state in [
        FallbackCheckpointState::RequestInFlight,
        FallbackCheckpointState::DecisionInProgress,
        FallbackCheckpointState::ReconciliationRequired,
    ] {
        assert!(matches!(
            construct(resume_runtime(state)),
            Err(FallbackInitializationError::UnresolvedProviderRequest)
        ));
    }

    let mut runtime = resume_runtime(FallbackCheckpointState::Ready);
    runtime.task_state = FallbackTaskState::ResumedUnreconciled;
    assert!(matches!(
        construct(runtime),
        Err(FallbackInitializationError::TaskStateNotReconciled)
    ));
}

#[test]
fn reconciled_resume_commits_a_pending_action_handoff_before_next_request() {
    let runtime = resume_runtime(FallbackCheckpointState::ActionPendingReconciliation);

    let source = construct(runtime).expect("host reconciled the pending action handoff");

    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::Ready
    );
    assert_eq!(
        source.runtime().persisted_states,
        [FallbackCheckpointState::Ready]
    );
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::Ready)
    );
}

#[test]
fn unreconciled_resume_rejects_a_pending_action_handoff() {
    let mut runtime = resume_runtime(FallbackCheckpointState::ActionPendingReconciliation);
    runtime.task_state = FallbackTaskState::ResumedUnreconciled;

    assert!(matches!(
        construct(runtime),
        Err(FallbackInitializationError::TaskStateNotReconciled)
    ));
}

#[test]
fn resumed_task_rejects_changed_routing_snapshot() {
    let runtime = resume_runtime(FallbackCheckpointState::Ready);
    let mut changed_candidates = candidates();
    changed_candidates[0].estimated_cost_micro_usd = Some(99);

    let result = FallbackDecisionSource::new(
        policy(3, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        changed_candidates,
        runtime,
    );

    assert!(matches!(
        result,
        Err(FallbackInitializationError::RoutingSnapshotMismatch)
    ));
}

#[test]
fn resumed_task_rejects_changed_fallback_limits() {
    let result = FallbackDecisionSource::new(
        policy(4, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        resume_runtime(FallbackCheckpointState::Ready),
    );

    assert!(matches!(
        result,
        Err(FallbackInitializationError::RoutingSnapshotMismatch)
    ));
}

#[test]
fn resumed_task_rejects_changed_cross_provider_transfer_consent() {
    let mut changed_limits = policy(3, 100).limits().expect("enabled limits");
    changed_limits.allow_cross_provider_data_transfer = true;
    let changed_policy = FallbackPolicy::opt_in(changed_limits).expect("valid limits");

    let result = FallbackDecisionSource::new(
        changed_policy,
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        resume_runtime(FallbackCheckpointState::Ready),
    );

    assert!(matches!(
        result,
        Err(FallbackInitializationError::RoutingSnapshotMismatch)
    ));
}

#[test]
fn resumed_task_rejects_changed_routing_strategy_identity() {
    let mut runtime = resume_runtime(FallbackCheckpointState::Ready);
    runtime.saved_routing_strategy_id = Some("different-router-v2".to_owned());

    assert!(matches!(
        construct(runtime),
        Err(FallbackInitializationError::RoutingSnapshotMismatch)
    ));
}

#[test]
fn new_task_requires_persisted_routing_snapshot_before_initial_checkpoint() {
    let runtime = FakeRuntime {
        fail_snapshot_persist: true,
        ..FakeRuntime::default()
    };

    let result = construct(runtime);

    assert!(matches!(
        result,
        Err(FallbackInitializationError::RoutingSnapshotPersistenceFailed)
    ));
}

#[test]
fn request_is_not_sent_when_in_flight_checkpoint_cannot_be_persisted() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        fail_persist_on_call: Some(2),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let action = source.choose_action("goal", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(source.runtime().persist_calls, 2);
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::CheckpointPersistenceFailed)
    );
}

#[test]
fn cancellation_during_checkpoint_persistence_prevents_provider_dispatch() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        cancel_on_persist_call: Some(2),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    assert_eq!(source.choose_action("goal", &[]), WorkspaceAction::Cancel);

    assert!(source.runtime().calls.is_empty());
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::Ready)
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::Cancelled)
    );
}

#[test]
fn failed_cancelled_request_checkpoint_write_keeps_source_in_flight() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        cancel_on_persist_call: Some(2),
        fail_persist_on_call: Some(3),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let action = source.choose_action("goal", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::RequestInFlight
    );
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::RequestInFlight)
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::CheckpointPersistenceFailed)
    );
}

#[test]
fn action_is_not_returned_when_response_checkpoint_cannot_be_persisted() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::CreateFile {
                path: PathBuf::from("must-not-dispatch.txt"),
                contents: "checkpoint failed".to_owned(),
            },
        )]),
        fail_persist_on_call: Some(3),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let action = source.choose_action("create a file", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::RequestInFlight)
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::CheckpointPersistenceFailed)
    );
}

#[test]
fn failed_action_handoff_checkpoint_write_keeps_source_blocked_in_memory() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        fail_persist_on_call: Some(4),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let first = source.choose_action("goal", &[]);
    let second = source.choose_action("goal", &[]);

    assert!(matches!(first, WorkspaceAction::Unsupported { .. }));
    assert!(matches!(second, WorkspaceAction::Unsupported { .. }));
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::DecisionInProgress
    );
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::DecisionInProgress)
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::UnresolvedProviderRequest)
    );
}

#[test]
fn unconfirmed_action_handoff_stays_blocked_before_the_next_provider_request() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::ReadFile {
                path: PathBuf::from("notes.txt"),
            },
        )]),
        refuse_action_reconciliation: true,
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    assert_eq!(
        source.choose_action("read notes", &[]),
        WorkspaceAction::ReadFile {
            path: PathBuf::from("notes.txt"),
        }
    );
    let observations = [WorkspaceObservation::FileRead {
        path: PathBuf::from("notes.txt"),
        contents: "saved result".to_owned(),
    }];

    assert!(matches!(
        source.choose_action("continue", &observations),
        WorkspaceAction::Unsupported { .. }
    ));

    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.runtime().reconciliation_attempts,
        [(
            WorkspaceAction::ReadFile {
                path: PathBuf::from("notes.txt"),
            },
            observations.to_vec(),
        )]
    );
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ActionPendingReconciliation
    );
    assert_eq!(
        source.runtime().persisted_states,
        [
            FallbackCheckpointState::Ready,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::ActionPendingReconciliation,
        ]
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::ActionHandoffNotReconciled)
    );
}

#[test]
fn failed_ready_checkpoint_keeps_confirmed_action_handoff_pending() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            FallbackProviderResponse::complete(WorkspaceAction::ReadFile {
                path: PathBuf::from("notes.txt"),
            }),
            FallbackProviderResponse::complete(WorkspaceAction::Complete {
                result: "done".to_owned(),
            }),
        ]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);
    assert_eq!(
        source.choose_action("read notes", &[]),
        WorkspaceAction::ReadFile {
            path: PathBuf::from("notes.txt"),
        }
    );
    source.runtime_mut().fail_persist_on_call = Some(5);
    let observations = [WorkspaceObservation::FileRead {
        path: PathBuf::from("notes.txt"),
        contents: "saved result".to_owned(),
    }];

    assert!(matches!(
        source.choose_action("finish", &observations),
        WorkspaceAction::Unsupported { .. }
    ));
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ActionPendingReconciliation
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::CheckpointPersistenceFailed)
    );

    source.runtime_mut().fail_persist_on_call = None;
    assert_eq!(
        source.choose_action("finish", &observations),
        WorkspaceAction::Complete {
            result: "done".to_owned(),
        }
    );
    assert_eq!(source.runtime().calls.len(), 2);
    assert_eq!(source.runtime().reconciliation_attempts.len(), 2);
}

#[test]
fn ambiguous_result_stays_blocked_until_host_reconciliation() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::failed(
            WorkspaceAction::Unsupported {
                action: format!(
                    "decision_failure:{}",
                    DecisionFailureCode::ProviderUnavailable.as_code()
                ),
            },
            ProviderCompletion::Ambiguous,
        )]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let first = source.choose_action("goal", &[]);
    let second = source.choose_action("goal", &[]);

    assert!(matches!(first, WorkspaceAction::Unsupported { .. }));
    assert!(matches!(second, WorkspaceAction::Unsupported { .. }));
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ReconciliationRequired
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::UnresolvedProviderRequest)
    );
}

#[test]
fn retries_only_the_decision_and_preserves_goal_observations_and_guidance() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            failed(429, DecisionFailureCode::ProviderRateLimited),
            FallbackProviderResponse::complete(WorkspaceAction::Continue),
        ]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);
    let observations = [WorkspaceObservation::FileRead {
        path: PathBuf::from("notes.txt"),
        contents: "authoritative prior result".to_owned(),
    }];

    let action = source.choose_action_with_root_project_guidance(
        "continue from the verified observation",
        &observations,
        Some("project instructions"),
    );

    assert_eq!(action, WorkspaceAction::Continue);
    assert_eq!(source.runtime().calls.len(), 2);
    assert_eq!(
        source.current_checkpoint().state(),
        FallbackCheckpointState::ActionPendingReconciliation
    );
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ActionPendingReconciliation
    );
    assert_eq!(
        source
            .runtime()
            .checkpoint
            .as_ref()
            .map(FallbackCheckpoint::state),
        Some(FallbackCheckpointState::ActionPendingReconciliation)
    );
    assert_eq!(source.runtime().waits, [Duration::from_millis(10)]);
    assert_eq!(source.usage().total_provider_requests, 2);
    assert_eq!(source.usage().total_estimated_cost_micro_usd, Some(20));
    assert_eq!(
        source.runtime().persisted_states,
        [
            FallbackCheckpointState::Ready,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::ActionPendingReconciliation,
        ]
    );
    let first = &source.runtime().calls[0];
    let second = &source.runtime().calls[1];
    assert_eq!(first.goal, second.goal);
    assert_eq!(first.observations, observations);
    assert_eq!(first.observations, second.observations);
    assert_eq!(first.guidance.as_deref(), Some("project instructions"));
    assert_eq!(first.guidance, second.guidance);
    assert_eq!(second.selection.model, "model-2");
}

#[test]
fn cancellation_during_backoff_prevents_the_next_provider_request() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([failed(503, DecisionFailureCode::ProviderUnavailable)]),
        cancel_on_wait: true,
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    assert_eq!(source.choose_action("goal", &[]), WorkspaceAction::Cancel);

    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(source.runtime().waits, [Duration::from_millis(10)]);
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::Cancelled)
    );
}

#[test]
fn cancellation_during_ambiguous_response_keeps_reconciliation_checkpoint() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::failed(
            WorkspaceAction::Unsupported {
                action: format!(
                    "decision_failure:{}",
                    DecisionFailureCode::ProviderUnavailable.as_code()
                ),
            },
            ProviderCompletion::Ambiguous,
        )]),
        cancel_on_decide_call: Some(1),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    assert_eq!(source.choose_action("goal", &[]), WorkspaceAction::Cancel);

    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ReconciliationRequired
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::Cancelled)
    );
}

#[test]
fn cancellation_during_partial_retry_keeps_reconciliation_checkpoint() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            failed(429, DecisionFailureCode::ProviderRateLimited),
            FallbackProviderResponse::failed(
                WorkspaceAction::CreateFile {
                    path: PathBuf::from("must-not-dispatch.txt"),
                    contents: "partial tool response".to_owned(),
                },
                ProviderCompletion::Partial,
            ),
        ]),
        cancel_on_decide_call: Some(2),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    assert_eq!(source.choose_action("goal", &[]), WorkspaceAction::Cancel);

    assert_eq!(source.runtime().calls.len(), 2);
    assert_eq!(
        source.persisted_checkpoint().state(),
        FallbackCheckpointState::ReconciliationRequired
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::Cancelled)
    );
}

#[test]
fn request_and_cost_budgets_are_reserved_across_decisions() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            failed(429, DecisionFailureCode::ProviderRateLimited),
            FallbackProviderResponse::complete(WorkspaceAction::Continue),
        ]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 2, 20);

    assert_eq!(
        source.choose_action("first", &[]),
        WorkspaceAction::Continue
    );
    let stopped = source.choose_action("second", &[]);

    assert_eq!(source.runtime().calls.len(), 2);
    assert_eq!(source.usage().total_provider_requests, 2);
    assert_eq!(source.usage().total_estimated_cost_micro_usd, Some(20));
    assert_eq!(
        stopped,
        WorkspaceAction::Unsupported {
            action: "decision_failure:decision_budget_exhausted".to_owned(),
        }
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::RequestBudgetExhausted)
    );
}

#[test]
fn enabled_policy_counts_manual_route_usage_without_switching_it() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            FallbackProviderResponse::complete(WorkspaceAction::Continue),
            FallbackProviderResponse::complete(WorkspaceAction::Continue),
        ]),
        ..FakeRuntime::default()
    };
    let mut manual_selection = selection("provider-a", "model-1");
    manual_selection.reason = SelectionReasonCode::ManualOverride;
    let mut source = FallbackDecisionSource::new(
        policy(2, 20),
        AutomaticRoutingRequest::default(),
        manual_selection,
        candidates(),
        runtime,
    )
    .expect("task checkpoint initializes");

    assert_eq!(
        source.choose_action("first", &[]),
        WorkspaceAction::Continue
    );
    assert_eq!(
        source.choose_action("second", &[]),
        WorkspaceAction::Continue
    );
    let stopped = source.choose_action("third", &[]);

    assert_eq!(source.runtime().calls.len(), 2);
    assert_eq!(source.usage().total_provider_requests, 2);
    assert_eq!(source.usage().total_estimated_cost_micro_usd, Some(20));
    assert!(matches!(stopped, WorkspaceAction::Unsupported { .. }));
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::RequestBudgetExhausted)
    );
}

#[test]
fn manual_selection_uses_cost_without_automatic_route_eligibility_filters() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        ..FakeRuntime::default()
    };
    let mut manual_selection = selection("provider-a", "model-1");
    manual_selection.reason = SelectionReasonCode::ManualOverride;
    let mut ineligible = candidate("provider-a", "model-1", 10);
    ineligible.readiness.executable = false;
    ineligible.capabilities = vec![CapabilityAssertion::new(
        CapabilityName::ToolCalling,
        CapabilitySupport::Unsupported,
        CapabilityProvenance::Declared,
    )];
    ineligible.model.context_window = Some(1);
    ineligible.model.max_output_tokens = Some(1);
    let request = AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::ToolCalling],
        minimum_context_window_tokens: Some(10_000),
        minimum_output_tokens: Some(2_000),
        ..AutomaticRoutingRequest::default()
    };
    let mut source = FallbackDecisionSource::new(
        policy(2, 20),
        request,
        manual_selection.clone(),
        vec![ineligible],
        runtime,
    )
    .expect("task checkpoint initializes");

    assert_eq!(
        source.choose_action("manual route", &[]),
        WorkspaceAction::Continue
    );
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(source.runtime().calls[0].selection, manual_selection);
    assert_eq!(source.usage().total_provider_requests, 1);
    assert_eq!(source.usage().total_estimated_cost_micro_usd, Some(10));
}

#[test]
fn manual_route_with_unknown_cost_fails_closed_when_cost_limit_is_enabled() {
    let runtime = FakeRuntime::default();
    let mut manual_selection = selection("provider-a", "model-1");
    manual_selection.reason = SelectionReasonCode::ManualOverride;
    let mut candidates = candidates();
    candidates[0].estimated_cost_micro_usd = None;
    let mut source = FallbackDecisionSource::new(
        policy(2, 20),
        AutomaticRoutingRequest::default(),
        manual_selection,
        candidates,
        runtime,
    )
    .expect("task checkpoint initializes");

    let action = source.choose_action("goal", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::UnknownAggregateCost)
    );
}

#[test]
fn manual_route_missing_from_candidate_catalog_fails_closed_without_substitution() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        ..FakeRuntime::default()
    };
    let mut manual_selection = selection("provider-a", "custom-model");
    manual_selection.reason = SelectionReasonCode::ManualOverride;
    let mut source = FallbackDecisionSource::new(
        policy(2, 20),
        AutomaticRoutingRequest::default(),
        manual_selection.clone(),
        candidates(),
        runtime,
    )
    .expect("task checkpoint initializes");

    let action = source.choose_action("manual route", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(source.current_selection(), &manual_selection);
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::UnknownAggregateCost)
    );
}

#[test]
fn ambiguous_http_errors_require_reconciliation_and_block_repeated_requests() {
    let no_status = FallbackProviderResponse::failed(
        WorkspaceAction::Unsupported {
            action: format!(
                "decision_failure:{}",
                DecisionFailureCode::ProviderUnavailable.as_code()
            ),
        },
        ProviderCompletion::ExplicitHttpError,
    );
    for response in [
        failed(408, DecisionFailureCode::ProviderUnavailable),
        failed(425, DecisionFailureCode::ProviderUnavailable),
        no_status,
    ] {
        let runtime = FakeRuntime {
            responses: VecDeque::from([response]),
            ..FakeRuntime::default()
        };
        let mut source = source(runtime, 3, 100);

        let first = source.choose_action("goal", &[]);
        let second = source.choose_action("goal", &[]);

        assert!(matches!(first, WorkspaceAction::Unsupported { .. }));
        assert!(matches!(second, WorkspaceAction::Unsupported { .. }));
        assert_eq!(source.runtime().calls.len(), 1);
        assert_eq!(
            source.persisted_checkpoint().state(),
            FallbackCheckpointState::ReconciliationRequired
        );
        assert_eq!(
            source.last_stop_reason(),
            Some(&FallbackStopReason::UnresolvedProviderRequest)
        );
    }
}

#[test]
fn aggregate_cost_is_checked_before_a_later_task_request() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::complete(
            WorkspaceAction::Continue,
        )]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 15);

    assert_eq!(
        source.choose_action("first", &[]),
        WorkspaceAction::Continue
    );
    let stopped = source.choose_action("second", &[]);

    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(source.usage().total_provider_requests, 1);
    assert_eq!(source.usage().total_estimated_cost_micro_usd, Some(10));
    assert_eq!(
        stopped,
        WorkspaceAction::Unsupported {
            action: "decision_failure:decision_budget_exhausted".to_owned(),
        }
    );
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::CostBudgetExhausted)
    );
}

#[test]
fn restored_usage_cannot_reset_the_task_request_budget() {
    let runtime = FakeRuntime {
        task_state: FallbackTaskState::ResumedReconciled,
        saved_routing_request: Some(AutomaticRoutingRequest::default()),
        saved_routing_candidates: Some(candidates()),
        saved_fallback_policy: Some(policy(2, 100)),
        saved_routing_strategy_id: Some("deterministic-model-router-v1".to_owned()),
        checkpoint: Some(FallbackCheckpoint::restore(
            FallbackUsage {
                total_provider_requests: 2,
                total_estimated_cost_micro_usd: Some(20),
            },
            selection("provider-a", "model-1"),
            FallbackCheckpointState::Ready,
        )),
        ..FakeRuntime::default()
    };
    let mut source = FallbackDecisionSource::new(
        policy(2, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        runtime,
    )
    .expect("reconciled task restores checkpoint");

    let action = source.choose_action("resumed task", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(source.usage().total_provider_requests, 2);
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::RequestBudgetExhausted)
    );
}

#[test]
fn unknown_initial_route_cost_fails_closed_before_a_provider_call() {
    let runtime = FakeRuntime::default();
    let mut candidates = candidates();
    candidates[0].estimated_cost_micro_usd = None;
    let mut source = FallbackDecisionSource::new(
        policy(3, 100),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates,
        runtime,
    )
    .expect("task checkpoint initializes");

    let action = source.choose_action("unknown price", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert!(source.runtime().calls.is_empty());
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::UnknownAggregateCost)
    );
}

#[test]
fn partial_response_cannot_return_or_dispatch_a_proposed_action() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::failed(
            WorkspaceAction::CreateFile {
                path: PathBuf::from("must-not-exist.txt"),
                contents: "partial tool response".to_owned(),
            },
            ProviderCompletion::Partial,
        )]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let action = source.choose_action("goal", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert_eq!(source.runtime().calls.len(), 1);
    assert!(source.runtime().waits.is_empty());
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::PartialResponse)
    );
}

#[test]
fn disabled_fallback_rejects_an_action_attached_to_an_http_error() {
    let runtime = FakeRuntime {
        responses: VecDeque::from([FallbackProviderResponse::failed(
            WorkspaceAction::CreateFile {
                path: PathBuf::from("must-not-dispatch.txt"),
                contents: "error response payload".to_owned(),
            },
            ProviderCompletion::ExplicitHttpError,
        )]),
        ..FakeRuntime::default()
    };
    let mut source = FallbackDecisionSource::new(
        FallbackPolicy::default(),
        AutomaticRoutingRequest::default(),
        selection("provider-a", "model-1"),
        candidates(),
        runtime,
    )
    .expect("task checkpoint initializes");

    let action = source.choose_action("goal", &[]);

    assert!(matches!(action, WorkspaceAction::Unsupported { .. }));
    assert_eq!(source.runtime().calls.len(), 1);
    assert_eq!(
        source.last_stop_reason(),
        Some(&FallbackStopReason::InvalidFailureEvidence)
    );
}

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-fallback-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).expect("create temporary workspace");
        Self(path)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn one_complete_action_reaches_harness_after_retry() {
    let workspace = TempWorkspace::new();
    let mut session = TaskSession::new_in_workspace(&workspace.0).expect("workspace session");
    let task_id = session.create_task("create one file").id();
    let turn_id = {
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("start task");
        let turn_id = task.create_turn().expect("turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("start turn");
        turn_id
    };
    let runtime = FakeRuntime {
        responses: VecDeque::from([
            failed(503, DecisionFailureCode::ProviderUnavailable),
            FallbackProviderResponse::complete(WorkspaceAction::CreateFile {
                path: PathBuf::from("created.txt"),
                contents: "created once".to_owned(),
            }),
            FallbackProviderResponse::complete(WorkspaceAction::Complete {
                result: "file created".to_owned(),
            }),
        ]),
        ..FakeRuntime::default()
    };
    let mut source = source(runtime, 3, 100);

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        "create one file",
        2,
        &mut source,
    )
    .expect("run through Harness");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(result.actions_taken(), 2);
    assert_eq!(
        fs::read_to_string(workspace.0.join("created.txt")).expect("created file"),
        "created once"
    );
    assert_eq!(
        result
            .observations()
            .iter()
            .filter(|observation| matches!(observation, WorkspaceObservation::FileCreated { .. }))
            .count(),
        1
    );
    assert_eq!(source.runtime().calls.len(), 3);
    assert_eq!(
        source.runtime().reconciliation_attempts,
        [(
            WorkspaceAction::CreateFile {
                path: PathBuf::from("created.txt"),
                contents: "created once".to_owned(),
            },
            result.observations().to_vec(),
        )]
    );
    assert_eq!(
        source.runtime().persisted_states,
        [
            FallbackCheckpointState::Ready,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::ActionPendingReconciliation,
            FallbackCheckpointState::Ready,
            FallbackCheckpointState::RequestInFlight,
            FallbackCheckpointState::DecisionInProgress,
            FallbackCheckpointState::ActionPendingReconciliation,
        ]
    );
}

impl<R, M> FallbackDecisionSource<R, M>
where
    R: FallbackRuntime,
    M: ModelRouter,
{
    fn runtime(&self) -> &R {
        &self.runtime
    }
}
