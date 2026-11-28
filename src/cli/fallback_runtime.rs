//! Task host for the bounded provider fallback decision source.

use super::model_selection::{prepare_model_source_for_selection, ProviderSelection};
use super::settings::UserSettingsPaths;
use super::task_routing_state::{TaskRoutingStateError, TaskRoutingStateStore};
use crate::agent_loop::{
    BoundedDecisionSource, DecisionFailureCode, WorkspaceAction, WorkspaceObservation,
};
use crate::brain::{AdapterId, ModelDecisionSource, ModelProvider};
use crate::fallback::{
    FallbackCheckpoint, FallbackDecisionSource, FallbackProviderResponse, FallbackRuntime,
    FallbackTaskState, ProviderCompletion,
};
use crate::harness::context::ObservationCompactionReport;
use crate::routing::{AutomaticRoutingRequest, RoutingCandidate, RoutingSelection};
use serde_json::Value;
use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

static TASK_CANCELLED: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static TASK_SIGNAL_HANDLER: OnceLock<Result<(), &'static str>> = OnceLock::new();

pub(super) struct CliFallbackRuntime {
    store: TaskRoutingStateStore,
    task_state: FallbackTaskState,
    initial_source: Option<(RoutingSelection, ModelDecisionSource)>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
    repository_context: Option<Value>,
    observation_context_budget_bytes: Option<usize>,
    last_observation_compaction: Option<ObservationCompactionReport>,
    last_token_usage: Option<crate::brain::ModelTokenUsage>,
    turn_boundary_confirmed: bool,
    cancellation: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(super) enum CliFallbackRuntimeError {
    TaskRoutingState(TaskRoutingStateError),
    CancellationRegistration(String),
}

impl fmt::Display for CliFallbackRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskRoutingState(error) => error.fmt(formatter),
            Self::CancellationRegistration(message) => formatter.write_str(message),
        }
    }
}

impl Error for CliFallbackRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRoutingState(error) => Some(error),
            Self::CancellationRegistration(_) => None,
        }
    }
}

pub(super) struct CliFallbackRuntimeConfig<'a> {
    pub(super) paths: &'a UserSettingsPaths,
    pub(super) task_id: Uuid,
    pub(super) resumed: bool,
    pub(super) initial_selection: RoutingSelection,
    pub(super) initial_source: ModelDecisionSource,
    pub(super) action_budget: usize,
    pub(super) ollama_endpoint: SocketAddr,
    pub(super) repository_context: Option<Value>,
    pub(super) observation_context_budget_bytes: Option<usize>,
}

impl CliFallbackRuntime {
    pub(super) fn new(
        config: CliFallbackRuntimeConfig<'_>,
    ) -> Result<Self, CliFallbackRuntimeError> {
        let (store, _) = TaskRoutingStateStore::open(config.paths, config.task_id)
            .map_err(CliFallbackRuntimeError::TaskRoutingState)?;
        let cancellation = task_cancellation_signal()
            .map_err(CliFallbackRuntimeError::CancellationRegistration)?;
        Ok(Self {
            store,
            task_state: if config.resumed {
                FallbackTaskState::ResumedReconciled
            } else {
                FallbackTaskState::New
            },
            initial_source: Some((config.initial_selection, config.initial_source)),
            action_budget: config.action_budget,
            ollama_endpoint: config.ollama_endpoint,
            repository_context: config.repository_context,
            observation_context_budget_bytes: config.observation_context_budget_bytes,
            last_observation_compaction: None,
            last_token_usage: None,
            turn_boundary_confirmed: false,
            cancellation,
        })
    }

    pub(super) fn take_token_usage(&mut self) -> Option<crate::brain::ModelTokenUsage> {
        self.last_token_usage.take()
    }

    pub(super) fn take_observation_compaction(&mut self) -> Option<ObservationCompactionReport> {
        self.last_observation_compaction.take()
    }

    fn source_for(
        &mut self,
        selection: &RoutingSelection,
    ) -> Result<ModelDecisionSource, &'static str> {
        let source = match self.initial_source.take() {
            Some((initial_selection, source)) if initial_selection == *selection => source,
            Some((initial_selection, source)) => {
                self.initial_source = Some((initial_selection, source));
                self.new_source(selection)?
            }
            None => self.new_source(selection)?,
        };
        let mut source = source;
        if let Some(context) = self.repository_context.as_ref() {
            if source.adapter_id() != AdapterId::OPENAI_COMPATIBLE {
                return Err("automatic route does not support repository context");
            }
            source.set_repository_context(context.clone())?;
        }
        if let Some(budget_bytes) = self.observation_context_budget_bytes {
            source.set_observation_context_budget(budget_bytes)?;
        }
        Ok(source)
    }

    fn new_source(
        &self,
        selection: &RoutingSelection,
    ) -> Result<ModelDecisionSource, &'static str> {
        let provider = selection
            .provider
            .as_deref()
            .and_then(ModelProvider::parse)
            .ok_or("automatic route has an unsupported provider")?;
        prepare_model_source_for_selection(
            Some(selection.model.clone()),
            Some(ProviderSelection::BuiltIn(provider)),
            self.action_budget,
            self.ollama_endpoint,
            true,
        )?
        .ok_or("automatic route could not create a provider adapter")
    }
}

impl FallbackRuntime for CliFallbackRuntime {
    fn task_state(&self) -> FallbackTaskState {
        self.task_state
    }

    fn load_checkpoint(&mut self) -> Option<FallbackCheckpoint> {
        self.store.checkpoint()
    }

    fn persist_routing_snapshot(
        &mut self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &crate::fallback::FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool {
        self.store
            .persist_routing_snapshot(request, candidates, *policy, routing_strategy_id)
    }

    fn routing_snapshot_matches(
        &self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: &crate::fallback::FallbackPolicy,
        routing_strategy_id: Option<&str>,
    ) -> bool {
        self.store
            .routing_snapshot_matches(request, candidates, *policy, routing_strategy_id)
    }

    fn persist_checkpoint(&mut self, checkpoint: &FallbackCheckpoint) -> bool {
        self.store.persist_checkpoint(checkpoint)
    }

    fn reconcile_action_handoff(
        &mut self,
        action: &WorkspaceAction,
        observations: &[WorkspaceObservation],
    ) -> bool {
        if self.turn_boundary_confirmed {
            self.turn_boundary_confirmed = false;
            return true;
        }
        let Some(observation) = observations.last() else {
            return false;
        };
        action_matches_observation(action, observation)
    }

    fn confirm_turn_boundary(&mut self) {
        self.turn_boundary_confirmed = true;
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::SeqCst)
    }

    fn wait_cancellable(&mut self, delay: Duration) -> bool {
        wait_until_or_cancelled(&self.cancellation, delay)
    }

    fn decide(
        &mut self,
        selection: &RoutingSelection,
        goal: &str,
        observations: &[WorkspaceObservation],
        root_project_guidance: Option<&str>,
    ) -> FallbackProviderResponse {
        let mut source = match self.source_for(selection) {
            Ok(source) => source,
            Err(_) => {
                return FallbackProviderResponse::complete(WorkspaceAction::Unsupported {
                    action: format!(
                        "decision_failure:{}",
                        DecisionFailureCode::InvalidModelName.as_code()
                    ),
                })
            }
        };
        let action = source.choose_action_with_root_project_guidance(
            goal,
            observations,
            root_project_guidance,
        );
        self.last_token_usage = source.take_token_usage();
        self.last_observation_compaction = source.take_observation_compaction();
        let completion = match &action {
            WorkspaceAction::Unsupported { action: label }
                if matches!(
                    action.decision_failure_code(),
                    Some(
                        DecisionFailureCode::ProviderUnavailable
                            | DecisionFailureCode::ProviderRateLimited
                            | DecisionFailureCode::ProviderRejected
                    )
                ) =>
            {
                if DecisionFailureCode::http_status_from_action_label(label).is_some() {
                    ProviderCompletion::ExplicitHttpError
                } else {
                    ProviderCompletion::Ambiguous
                }
            }
            _ => ProviderCompletion::Complete,
        };
        FallbackProviderResponse::failed(action, completion)
    }

    fn needs_root_project_guidance(&self) -> bool {
        true
    }
}

fn task_cancellation_signal() -> Result<Arc<AtomicBool>, String> {
    let cancellation = Arc::clone(TASK_CANCELLED.get_or_init(|| Arc::new(AtomicBool::new(false))));
    let registration = TASK_SIGNAL_HANDLER.get_or_init(|| {
        signal_hook::flag::register_conditional_shutdown(
            signal_hook::consts::SIGINT,
            130,
            Arc::clone(&cancellation),
        )
        .map_err(|_| "could not register task cancellation handler")?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancellation))
            .map(|_| ())
            .map_err(|_| "could not register task cancellation handler")
    });
    registration.map_err(str::to_owned)?;
    Ok(cancellation)
}

fn wait_until_or_cancelled(cancellation: &AtomicBool, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return !cancellation.load(Ordering::SeqCst);
        }
        thread::sleep(remaining.min(CANCELLATION_POLL_INTERVAL));
    }
}

pub(super) enum TaskDecisionSource {
    Single {
        source: ModelDecisionSource,
        selection: Option<RoutingSelection>,
    },
    Fallback(Box<FallbackDecisionSource<CliFallbackRuntime>>),
}

impl TaskDecisionSource {
    pub(super) fn take_token_usage(&mut self) -> Option<crate::brain::ModelTokenUsage> {
        match self {
            Self::Single { source, .. } => source.take_token_usage(),
            Self::Fallback(source) => source.runtime_mut().take_token_usage(),
        }
    }

    pub(super) fn take_observation_compaction(&mut self) -> Option<ObservationCompactionReport> {
        match self {
            Self::Single { source, .. } => source.take_observation_compaction(),
            Self::Fallback(source) => source.runtime_mut().take_observation_compaction(),
        }
    }

    pub(super) fn current_selection(&self) -> Option<RoutingSelection> {
        match self {
            Self::Single { selection, .. } => selection.clone(),
            Self::Fallback(source) => Some(source.current_selection().clone()),
        }
    }

    pub(super) fn reconcile_completed_turn(
        &mut self,
        observations: &[WorkspaceObservation],
    ) -> Result<(), String> {
        match self {
            Self::Single { .. } => Ok(()),
            Self::Fallback(source) => {
                source
                    .reconcile_completed_turn(observations)
                    .map_err(|reason| {
                        format!("fallback task state could not be reconciled: {reason:?}")
                    })
            }
        }
    }
}

impl BoundedDecisionSource for TaskDecisionSource {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self {
            Self::Single { source, .. } => source.choose_action(goal, observations),
            Self::Fallback(source) => source.choose_action(goal, observations),
        }
    }

    fn needs_root_project_guidance(&self) -> bool {
        match self {
            Self::Single { source, .. } => source.needs_root_project_guidance(),
            Self::Fallback(source) => source.needs_root_project_guidance(),
        }
    }

    fn choose_action_with_root_project_guidance(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
        guidance: Option<&str>,
    ) -> WorkspaceAction {
        match self {
            Self::Single { source, .. } => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
            Self::Fallback(source) => {
                source.choose_action_with_root_project_guidance(goal, observations, guidance)
            }
        }
    }
}

fn action_matches_observation(
    action: &WorkspaceAction,
    observation: &WorkspaceObservation,
) -> bool {
    use WorkspaceAction as A;
    use WorkspaceObservation as O;
    match (action, observation) {
        (A::OfflineCargoTest { .. }, O::ProcessFinished { .. }) => true,
        (A::ReadFile { path }, O::FileRead { path: observed, .. })
        | (A::ListDirectory { path }, O::DirectoryListed { path: observed, .. })
        | (A::CreateDirectory { path }, O::DirectoryCreated { path: observed })
        | (A::CreateFile { path, .. }, O::FileCreated { path: observed, .. })
        | (A::ReplaceText { path, .. }, O::TextReplaced { path: observed, .. })
        | (A::ApplyPatch { path, .. }, O::PatchApplied { path: observed, .. }) => path == observed,
        (A::ReadRange { path, .. }, O::FileRangeRead { path: observed, .. }) => path == observed,
        (
            A::SearchText { directory, .. },
            O::TextSearched {
                directory: observed,
                ..
            },
        ) => directory == observed,
        (
            A::SearchTreeText { directory, .. },
            O::TreeTextSearched {
                directory: observed,
                ..
            },
        ) => directory == observed,
        (A::Extension { name, .. }, O::ExtensionCompleted { name: observed, .. }) => {
            name == observed
        }
        (
            action,
            O::ActionFailed {
                action: observed, ..
            },
        ) => action == observed,
        (A::CreateFile { path, .. }, O::FileCreationFailed { path: observed, .. }) => {
            path == observed
        }
        (A::CreateDirectory { path }, O::DirectoryCreationFailed { path: observed, .. }) => {
            path == observed
        }
        (A::ReplaceText { path, .. }, O::TextReplacementFailed { path: observed, .. }) => {
            path == observed
        }
        (A::ApplyPatch { path, .. }, O::PatchFailed { path: observed, .. }) => path == observed,
        (
            A::SearchText { directory, .. },
            O::TextSearchFailed {
                directory: observed,
                ..
            },
        ) => directory == observed,
        (
            A::SearchTreeText { directory, .. },
            O::TreeTextSearchFailed {
                directory: observed,
                ..
            },
        ) => directory == observed,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::error::Error;
    use std::fs;

    #[test]
    fn cancellable_wait_returns_when_cancelled_and_completes_when_not_cancelled() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancellation);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            signal.store(true, Ordering::SeqCst);
        });
        assert!(!wait_until_or_cancelled(
            &cancellation,
            Duration::from_secs(1)
        ));
        canceller.join().unwrap();

        let cancellation = AtomicBool::new(false);
        assert!(wait_until_or_cancelled(
            &cancellation,
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn runtime_construction_fails_closed_and_preserves_saved_state_error() {
        let directory = TempDir::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let paths = UserSettingsPaths {
            model: directory.path().join("config.toml"),
            credentials: directory.path().join("credentials.json"),
            permission_mode: directory.path().join("permission-mode"),
            provider_profiles: directory.path().join("provider-profiles.json"),
            model_evidence: directory.path().join("model-evidence.json"),
            model_evidence_key: directory.path().join("model-evidence.key"),
        };
        let task_id = Uuid::new_v4();
        let state_path = paths
            .model
            .with_file_name(format!("task-routing-{task_id}.json"));
        fs::write(&state_path, b"{").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let source = prepare_model_source_for_selection(
            Some("qwen2.5:0.5b".to_owned()),
            Some(ProviderSelection::BuiltIn(ModelProvider::Ollama)),
            1,
            SocketAddr::from(([127, 0, 0, 1], 11434)),
            false,
        )
        .unwrap()
        .unwrap();
        let error = CliFallbackRuntime::new(CliFallbackRuntimeConfig {
            paths: &paths,
            task_id,
            resumed: true,
            initial_selection: RoutingSelection {
                provider: Some("ollama".to_owned()),
                model: "qwen2.5:0.5b".to_owned(),
                reason: crate::routing::SelectionReasonCode::OnlyEligibleCandidate,
            },
            initial_source: source,
            action_budget: 1,
            ollama_endpoint: SocketAddr::from(([127, 0, 0, 1], 11434)),
            repository_context: None,
            observation_context_budget_bytes: None,
        })
        .err()
        .unwrap();

        assert!(matches!(
            error,
            CliFallbackRuntimeError::TaskRoutingState(TaskRoutingStateError::InvalidFormat(_))
        ));
        assert_eq!(
            error.to_string(),
            "task routing state is invalid or unsupported"
        );
        let state_error = error
            .source()
            .and_then(|source| source.downcast_ref::<TaskRoutingStateError>())
            .expect("runtime error preserves task-state source");
        assert!(state_error.source().is_some());
    }

    #[test]
    fn cancellation_registration_error_is_an_exact_message_without_source() {
        let error = CliFallbackRuntimeError::CancellationRegistration("probe message".to_owned());
        assert_eq!(error.to_string(), "probe message");
        assert!(error.source().is_none());
    }
}
