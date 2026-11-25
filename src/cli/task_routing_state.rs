//! Private, task-scoped routing snapshots and bounded fallback checkpoints.

use super::settings::{
    ensure_private_permissions, open_private_file, remove_file_if_present, write_private_file,
    UserSettingsPaths,
};
use crate::fallback::{FallbackCheckpoint, FallbackLimits, FallbackPolicy};
use crate::routing::{AutomaticRoutingRequest, RoutingCandidate};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const STATE_PREFIX: &str = "task-routing-";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RoutingSnapshot {
    request: AutomaticRoutingRequest,
    candidates: Vec<RoutingCandidate>,
    policy: FallbackPolicySnapshot,
    strategy_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FallbackPolicySnapshot {
    enabled: bool,
    max_additional_attempts: Option<u8>,
    max_total_provider_requests: Option<u8>,
    max_total_estimated_cost_micro_usd: Option<u64>,
    initial_backoff_ms: Option<u64>,
    maximum_backoff_ms: Option<u64>,
    allow_cross_provider_data_transfer: bool,
}

impl From<FallbackPolicy> for FallbackPolicySnapshot {
    fn from(policy: FallbackPolicy) -> Self {
        match policy.limits() {
            Some(limits) => Self {
                enabled: true,
                max_additional_attempts: Some(limits.max_additional_attempts),
                max_total_provider_requests: Some(limits.max_total_provider_requests),
                max_total_estimated_cost_micro_usd: Some(limits.max_total_estimated_cost_micro_usd),
                initial_backoff_ms: Some(duration_ms(limits.initial_backoff)),
                maximum_backoff_ms: Some(duration_ms(limits.maximum_backoff)),
                allow_cross_provider_data_transfer: limits.allow_cross_provider_data_transfer,
            },
            None => Self {
                enabled: false,
                max_additional_attempts: None,
                max_total_provider_requests: None,
                max_total_estimated_cost_micro_usd: None,
                initial_backoff_ms: None,
                maximum_backoff_ms: None,
                allow_cross_provider_data_transfer: false,
            },
        }
    }
}

fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u64::MAX as u128) as u64
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TaskRoutingDocument {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<RoutingSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<FallbackCheckpoint>,
}

impl TaskRoutingDocument {
    fn empty() -> Self {
        Self {
            version: STATE_VERSION,
            snapshot: None,
            checkpoint: None,
        }
    }
}

pub(super) struct TaskRoutingStateStore {
    path: PathBuf,
    document: TaskRoutingDocument,
}

impl TaskRoutingStateStore {
    pub(super) fn open(paths: &UserSettingsPaths, task_id: Uuid) -> Result<(Self, bool), String> {
        let path = state_path(paths, task_id);
        let (document, exists) = match read_document(&path)? {
            Some(document) => (document, true),
            None => (TaskRoutingDocument::empty(), false),
        };
        Ok((Self { path, document }, exists))
    }

    pub(super) fn checkpoint(&self) -> Option<FallbackCheckpoint> {
        self.document.checkpoint.clone()
    }

    pub(super) fn persist_routing_snapshot(
        &mut self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: FallbackPolicy,
        strategy_id: Option<&str>,
    ) -> bool {
        if self.document.snapshot.is_some() {
            return false;
        }
        self.document.snapshot = Some(RoutingSnapshot {
            request: request.clone(),
            candidates: candidates.to_vec(),
            policy: policy.into(),
            strategy_id: strategy_id.map(str::to_owned),
        });
        self.persist()
    }

    pub(super) fn routing_snapshot_matches(
        &self,
        request: &AutomaticRoutingRequest,
        candidates: &[RoutingCandidate],
        policy: FallbackPolicy,
        strategy_id: Option<&str>,
    ) -> bool {
        self.document.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.request == *request
                && snapshot.candidates == candidates
                && snapshot.policy == FallbackPolicySnapshot::from(policy)
                && snapshot.strategy_id.as_deref() == strategy_id
        })
    }

    pub(super) fn persist_checkpoint(&mut self, checkpoint: &FallbackCheckpoint) -> bool {
        if self.document.snapshot.is_none() {
            return false;
        }
        self.document.checkpoint = Some(checkpoint.clone());
        self.persist()
    }

    fn persist(&self) -> bool {
        let Ok(bytes) = serde_json::to_vec_pretty(&self.document) else {
            return false;
        };
        bytes.len() as u64 <= MAX_STATE_BYTES
            && write_private_file(&self.path, &bytes, "task routing state").is_ok()
    }
}

pub(super) fn enabled_fallback_policy(
    aggregate_budget_micro_usd: u64,
    allow_cross_provider_data_transfer: bool,
) -> Result<FallbackPolicy, String> {
    FallbackPolicy::opt_in(FallbackLimits {
        max_additional_attempts: 2,
        max_total_provider_requests: 3,
        max_total_estimated_cost_micro_usd: aggregate_budget_micro_usd,
        initial_backoff: Duration::from_millis(250),
        maximum_backoff: Duration::from_secs(2),
        allow_cross_provider_data_transfer,
    })
    .map_err(|error| format!("invalid bounded fallback configuration: {error}"))
}

pub(super) fn remove_task_routing_states(paths: &UserSettingsPaths) -> Result<(), String> {
    let Some(parent) = paths.model.parent() else {
        return Ok(());
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect task routing state directory '{}': {}",
                parent.display(),
                error.kind()
            ))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "cannot inspect task routing state directory '{}': {}",
                parent.display(),
                error.kind()
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(STATE_PREFIX) && name.ends_with(".json") {
            remove_file_if_present(&entry.path(), "task routing state")?;
        }
    }
    Ok(())
}

pub(super) fn remove_task_routing_state(
    paths: &UserSettingsPaths,
    task_id: Uuid,
) -> Result<(), String> {
    remove_file_if_present(&state_path(paths, task_id), "task routing state")
}

pub(super) fn has_task_routing_state(
    paths: &UserSettingsPaths,
    task_id: Uuid,
) -> Result<bool, String> {
    read_document(&state_path(paths, task_id)).map(|document| document.is_some())
}

fn state_path(paths: &UserSettingsPaths, task_id: Uuid) -> PathBuf {
    paths
        .model
        .with_file_name(format!("{STATE_PREFIX}{task_id}.json"))
}

fn read_document(path: &Path) -> Result<Option<TaskRoutingDocument>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot inspect task routing state '{}': {}",
                path.display(),
                error.kind()
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "task routing state '{}' is not a regular file",
            path.display()
        ));
    }
    let file = open_private_file(path).map_err(|error| {
        format!(
            "cannot read task routing state '{}': {}",
            path.display(),
            error.kind()
        )
    })?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "cannot inspect task routing state '{}': {}",
            path.display(),
            error.kind()
        )
    })?;
    if !opened_metadata.is_file() {
        return Err(format!(
            "task routing state '{}' is not a regular file",
            path.display()
        ));
    }
    ensure_private_permissions(path, &opened_metadata, "task routing state")?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "cannot read task routing state '{}': {}",
                path.display(),
                error.kind()
            )
        })?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("task routing state exceeds its storage limit".to_owned());
    }
    let document: TaskRoutingDocument = serde_json::from_slice(&bytes)
        .map_err(|_| "task routing state is invalid or unsupported".to_owned())?;
    if document.version != STATE_VERSION
        || document.checkpoint.is_some() != document.snapshot.is_some()
        || document.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.candidates.is_empty()
                || snapshot
                    .strategy_id
                    .as_deref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128)
        })
    {
        return Err("task routing state is inconsistent or unsupported".to_owned());
    }
    Ok(Some(document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_data::ModelSpec;
    use crate::routing::{
        CapabilityAssertion, CapabilityEvidenceFreshness, CapabilityName, CapabilityProvenance,
        CapabilitySupport, CapabilityVerification, ProviderReadiness, RoutingSelection,
        SelectionReasonCode,
    };
    use crate::test_support::TempDir;

    fn paths(directory: &TempDir) -> UserSettingsPaths {
        UserSettingsPaths {
            model: directory.path().join("config.toml"),
            credentials: directory.path().join("credentials.json"),
            provider_profiles: directory.path().join("provider-profiles.json"),
            model_evidence: directory.path().join("model-evidence.json"),
            model_evidence_key: directory.path().join("model-evidence.key"),
        }
    }

    fn candidate() -> RoutingCandidate {
        RoutingCandidate {
            provider: "openai".to_owned(),
            model: ModelSpec {
                provider: "openai".to_owned(),
                id: "gpt-4.1".to_owned(),
                name: "GPT-4.1".to_owned(),
                context_window: Some(1_000_000),
                max_output_tokens: Some(32_000),
                tool_calling: Some(true),
                reasoning: Some(true),
                status: None,
                modalities: None,
            },
            readiness: ProviderReadiness {
                discoverable: true,
                executable: true,
                configured: true,
                verification: CapabilityVerification::Verified,
            },
            capabilities: vec![CapabilityAssertion::new(
                CapabilityName::DecisionRequest,
                CapabilitySupport::Supported,
                CapabilityProvenance::Verified,
            )],
            capability_evidence_freshness: CapabilityEvidenceFreshness::Fresh,
            estimated_cost_micro_usd: Some(100),
            observed_latency_ms: Some(50),
            quality_score_basis_points: None,
        }
    }

    #[test]
    fn task_snapshot_and_checkpoint_round_trip_privately_and_match_exact_inputs() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        let task_id = Uuid::new_v4();
        let request = AutomaticRoutingRequest::default();
        let candidates = vec![candidate()];
        let policy = enabled_fallback_policy(1_000_000, false).unwrap();
        let selection = RoutingSelection {
            provider: Some("openai".to_owned()),
            model: "gpt-4.1".to_owned(),
            reason: SelectionReasonCode::OnlyEligibleCandidate,
        };
        let checkpoint = FallbackCheckpoint::restore(
            crate::fallback::FallbackUsage {
                total_provider_requests: 1,
                total_estimated_cost_micro_usd: Some(100),
            },
            selection,
            crate::fallback::FallbackCheckpointState::Ready,
        );

        let (mut store, existed) = TaskRoutingStateStore::open(&paths, task_id).unwrap();
        assert!(!existed);
        assert!(store.persist_routing_snapshot(&request, &candidates, policy, None));
        assert!(store.persist_checkpoint(&checkpoint));

        let (restored, existed) = TaskRoutingStateStore::open(&paths, task_id).unwrap();
        assert!(existed);
        assert!(restored.routing_snapshot_matches(&request, &candidates, policy, None));
        assert!(!restored.routing_snapshot_matches(
            &request,
            &candidates,
            enabled_fallback_policy(2_000_000, false).unwrap(),
            None
        ));
        assert_eq!(restored.checkpoint(), Some(checkpoint));

        let state = fs::read_to_string(state_path(&paths, task_id)).unwrap();
        assert!(!state.contains("prompt"));
        assert!(!state.contains("credential"));
        assert!(!state.contains("endpoint"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state_path(&paths, task_id))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }
}
