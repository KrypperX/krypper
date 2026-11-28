//! Private, task-scoped routing snapshots and bounded fallback checkpoints.

use super::settings::{
    ensure_private_permissions_with_source, open_private_file, write_private_file,
    PrivatePermissionsError, UserSettingsPaths,
};
use crate::fallback::FallbackPolicyError;
use crate::fallback::{FallbackCheckpoint, FallbackLimits, FallbackPolicy};
use crate::routing::{AutomaticRoutingRequest, RoutingCandidate};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const STATE_PREFIX: &str = "task-routing-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TaskRoutingStateIoAction {
    Inspect,
    Read,
    InspectDirectory,
    Remove,
}

impl TaskRoutingStateIoAction {
    fn description(self) -> &'static str {
        match self {
            Self::Inspect => "cannot inspect task routing state",
            Self::Read => "cannot read task routing state",
            Self::InspectDirectory => "cannot inspect task routing state directory",
            Self::Remove => "cannot remove task routing state",
        }
    }
}

#[derive(Debug)]
pub(super) enum TaskRoutingStateError {
    Io {
        action: TaskRoutingStateIoAction,
        path: PathBuf,
        source: io::Error,
    },
    Permissions(PrivatePermissionsError),
    NotRegularFile(PathBuf),
    TooLarge,
    InvalidFormat(serde_json::Error),
    UnsupportedVersion,
    InconsistentDocument,
    FallbackPolicy(FallbackPolicyError),
}

impl TaskRoutingStateError {
    fn io(action: TaskRoutingStateIoAction, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for TaskRoutingStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(
                formatter,
                "{} '{}': {}",
                action.description(),
                path.display(),
                source.kind()
            ),
            Self::Permissions(error) => error.fmt(formatter),
            Self::NotRegularFile(path) => write!(
                formatter,
                "task routing state '{}' is not a regular file",
                path.display()
            ),
            Self::TooLarge => formatter.write_str("task routing state exceeds its storage limit"),
            Self::InvalidFormat(_) => {
                formatter.write_str("task routing state is invalid or unsupported")
            }
            Self::UnsupportedVersion | Self::InconsistentDocument => {
                formatter.write_str("task routing state is inconsistent or unsupported")
            }
            Self::FallbackPolicy(error) => {
                write!(formatter, "invalid bounded fallback configuration: {error}")
            }
        }
    }
}

impl Error for TaskRoutingStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Permissions(error) => Some(error),
            Self::InvalidFormat(error) => Some(error),
            Self::FallbackPolicy(error) => Some(error),
            Self::NotRegularFile(_)
            | Self::TooLarge
            | Self::UnsupportedVersion
            | Self::InconsistentDocument => None,
        }
    }
}

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
    pub(super) fn open(
        paths: &UserSettingsPaths,
        task_id: Uuid,
    ) -> Result<(Self, bool), TaskRoutingStateError> {
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
) -> Result<FallbackPolicy, TaskRoutingStateError> {
    fallback_policy_from_limits(FallbackLimits {
        max_additional_attempts: 2,
        max_total_provider_requests: 3,
        max_total_estimated_cost_micro_usd: aggregate_budget_micro_usd,
        initial_backoff: Duration::from_millis(250),
        maximum_backoff: Duration::from_secs(2),
        allow_cross_provider_data_transfer,
    })
}

fn fallback_policy_from_limits(
    limits: FallbackLimits,
) -> Result<FallbackPolicy, TaskRoutingStateError> {
    FallbackPolicy::opt_in(limits).map_err(TaskRoutingStateError::FallbackPolicy)
}

pub(super) fn remove_task_routing_states(
    paths: &UserSettingsPaths,
) -> Result<(), TaskRoutingStateError> {
    let Some(parent) = paths.model.parent() else {
        return Ok(());
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(TaskRoutingStateError::io(
                TaskRoutingStateIoAction::InspectDirectory,
                parent,
                source,
            ))
        }
    };
    remove_task_routing_state_entries(parent, entries)
}

fn remove_task_routing_state_entries<I>(
    parent: &Path,
    entries: I,
) -> Result<(), TaskRoutingStateError>
where
    I: IntoIterator<Item = io::Result<fs::DirEntry>>,
{
    for entry in entries {
        let entry = entry.map_err(|source| {
            TaskRoutingStateError::io(TaskRoutingStateIoAction::InspectDirectory, parent, source)
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(STATE_PREFIX) && name.ends_with(".json") {
            remove_state_file_if_present(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_state_file_if_present(path: &Path) -> Result<(), TaskRoutingStateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(TaskRoutingStateError::io(
            TaskRoutingStateIoAction::Remove,
            path,
            source,
        )),
    }
}

pub(super) fn remove_task_routing_state(
    paths: &UserSettingsPaths,
    task_id: Uuid,
) -> Result<(), TaskRoutingStateError> {
    remove_state_file_if_present(&state_path(paths, task_id))
}

pub(super) fn has_task_routing_state(
    paths: &UserSettingsPaths,
    task_id: Uuid,
) -> Result<bool, TaskRoutingStateError> {
    read_document(&state_path(paths, task_id)).map(|document| document.is_some())
}

fn state_path(paths: &UserSettingsPaths, task_id: Uuid) -> PathBuf {
    paths
        .model
        .with_file_name(format!("{STATE_PREFIX}{task_id}.json"))
}

fn read_document(path: &Path) -> Result<Option<TaskRoutingDocument>, TaskRoutingStateError> {
    read_document_with(
        path,
        |path| fs::symlink_metadata(path),
        open_private_file,
        fs::File::metadata,
        validate_state_permissions,
        read_state_bytes,
    )
}

fn validate_state_permissions(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), PrivatePermissionsError> {
    ensure_private_permissions_with_source(path, metadata, "task routing state")
}

fn read_state_bytes(file: fs::File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_document_with<M, O, FM, P, R>(
    path: &Path,
    symlink_metadata: M,
    open_file: O,
    opened_file_metadata: FM,
    validate_permissions: P,
    read_bounded: R,
) -> Result<Option<TaskRoutingDocument>, TaskRoutingStateError>
where
    M: FnOnce(&Path) -> io::Result<fs::Metadata>,
    O: FnOnce(&Path) -> io::Result<fs::File>,
    FM: FnOnce(&fs::File) -> io::Result<fs::Metadata>,
    P: FnOnce(&Path, &fs::Metadata) -> Result<(), PrivatePermissionsError>,
    R: FnOnce(fs::File, u64) -> io::Result<Vec<u8>>,
{
    let metadata = match symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TaskRoutingStateError::io(
                TaskRoutingStateIoAction::Inspect,
                path,
                source,
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(TaskRoutingStateError::NotRegularFile(path.to_owned()));
    }
    let file = open_file(path).map_err(|source| {
        TaskRoutingStateError::io(TaskRoutingStateIoAction::Read, path, source)
    })?;
    let opened_metadata = opened_file_metadata(&file).map_err(|source| {
        TaskRoutingStateError::io(TaskRoutingStateIoAction::Inspect, path, source)
    })?;
    if !opened_metadata.is_file() {
        return Err(TaskRoutingStateError::NotRegularFile(path.to_owned()));
    }
    validate_permissions(path, &opened_metadata).map_err(TaskRoutingStateError::Permissions)?;
    let bytes = read_bounded(file, MAX_STATE_BYTES + 1).map_err(|source| {
        TaskRoutingStateError::io(TaskRoutingStateIoAction::Read, path, source)
    })?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(TaskRoutingStateError::TooLarge);
    }
    let document: TaskRoutingDocument =
        serde_json::from_slice(&bytes).map_err(TaskRoutingStateError::InvalidFormat)?;
    if document.version != STATE_VERSION {
        return Err(TaskRoutingStateError::UnsupportedVersion);
    }
    if document.checkpoint.is_some() != document.snapshot.is_some()
        || document.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.candidates.is_empty()
                || snapshot
                    .strategy_id
                    .as_deref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128)
        })
    {
        return Err(TaskRoutingStateError::InconsistentDocument);
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
    use std::error::Error;
    use std::fs::File;

    fn paths(directory: &TempDir) -> UserSettingsPaths {
        UserSettingsPaths {
            model: directory.path().join("config.toml"),
            credentials: directory.path().join("credentials.json"),
            permission_mode: directory.path().join("permission-mode"),
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

    fn write_state(directory: &TempDir, task_id: Uuid, bytes: &[u8]) -> PathBuf {
        let path = state_path(&paths(directory), task_id);
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn expect_error<T>(result: Result<T, TaskRoutingStateError>) -> TaskRoutingStateError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("operation unexpectedly succeeded"),
        }
    }

    fn error_source(error: &TaskRoutingStateError) -> Option<&(dyn Error + 'static)> {
        error.source()
    }

    #[test]
    fn read_document_with_preserves_io_error_classification_and_causes() {
        let directory = TempDir::new();
        let task_id = Uuid::new_v4();
        let path = write_state(
            &directory,
            task_id,
            br#"{"version":1,"snapshot":null,"checkpoint":null}"#,
        );

        let error = read_document_with(
            &path,
            |_| Err(io::Error::other("metadata probe")),
            open_private_file,
            File::metadata,
            validate_state_permissions,
            read_state_bytes,
        )
        .unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Inspect,
            &path,
            io::ErrorKind::Other,
            &format!(
                "cannot inspect task routing state '{}': {}",
                path.display(),
                io::ErrorKind::Other
            ),
        );

        let error = read_document_with(
            &path,
            |path| fs::symlink_metadata(path),
            |_| Err(io::Error::other("open probe")),
            File::metadata,
            validate_state_permissions,
            read_state_bytes,
        )
        .unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Read,
            &path,
            io::ErrorKind::Other,
            &format!(
                "cannot read task routing state '{}': {}",
                path.display(),
                io::ErrorKind::Other
            ),
        );

        let error = read_document_with(
            &path,
            |path| fs::symlink_metadata(path),
            open_private_file,
            |_| Err(io::Error::other("opened metadata probe")),
            validate_state_permissions,
            read_state_bytes,
        )
        .unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Inspect,
            &path,
            io::ErrorKind::Other,
            &format!(
                "cannot inspect task routing state '{}': {}",
                path.display(),
                io::ErrorKind::Other
            ),
        );

        let error = read_document_with(
            &path,
            |path| fs::symlink_metadata(path),
            open_private_file,
            File::metadata,
            validate_state_permissions,
            |_, _| Err(io::Error::other("read probe")),
        )
        .unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Read,
            &path,
            io::ErrorKind::Other,
            &format!(
                "cannot read task routing state '{}': {}",
                path.display(),
                io::ErrorKind::Other
            ),
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_document_with_preserves_permission_errors_and_symlink_failures_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let task_id = Uuid::new_v4();
        let path = write_state(
            &directory,
            task_id,
            br#"{"version":1,"snapshot":null,"checkpoint":null}"#,
        );
        let error = read_document_with(
            &path,
            |path| fs::symlink_metadata(path),
            open_private_file,
            File::metadata,
            |path, _| {
                Err(PrivatePermissionsError::Io {
                    action: "probe private permissions",
                    path: path.to_owned(),
                    source: io::Error::other("permission probe"),
                })
            },
            read_state_bytes,
        )
        .unwrap_err();
        assert!(matches!(error, TaskRoutingStateError::Permissions(_)));
        assert_eq!(
            error.to_string(),
            format!(
                "probe private permissions '{}': {}",
                path.display(),
                io::ErrorKind::Other
            )
        );
        let source = error_source(&error).unwrap();
        assert!(source.downcast_ref::<PrivatePermissionsError>().is_some());
        assert_eq!(
            source
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .map(io::Error::kind),
            Some(io::ErrorKind::Other)
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = TaskRoutingStateStore::open(&paths(&directory), task_id)
            .err()
            .unwrap();
        assert!(matches!(error, TaskRoutingStateError::Permissions(_)));
        assert_eq!(
            error.to_string(),
            format!(
                "task routing state '{}' is accessible by other users; restrict its permissions or run `krypper setup --reset`",
                path.display()
            )
        );

        let symlink = directory.path().join("task-routing-symlink.json");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        let mut open_called = false;
        let error = read_document_with(
            &symlink,
            |path| fs::symlink_metadata(path),
            |_| {
                open_called = true;
                open_private_file(&path)
            },
            File::metadata,
            validate_state_permissions,
            read_state_bytes,
        )
        .unwrap_err();
        assert!(!open_called);
        assert!(matches!(error, TaskRoutingStateError::NotRegularFile(_)));
        assert_eq!(
            error.to_string(),
            format!(
                "task routing state '{}' is not a regular file",
                symlink.display()
            )
        );
        assert!(error_source(&error).is_none());
    }

    #[test]
    fn read_document_classifies_file_type_size_format_version_and_consistency() {
        let directory = TempDir::new();
        let task_id = Uuid::new_v4();
        let path = state_path(&paths(&directory), task_id);
        fs::create_dir(&path).unwrap();
        let error = TaskRoutingStateStore::open(&paths(&directory), task_id)
            .err()
            .unwrap();
        assert!(matches!(error, TaskRoutingStateError::NotRegularFile(_)));
        assert_eq!(
            error.to_string(),
            format!(
                "task routing state '{}' is not a regular file",
                path.display()
            )
        );
        assert!(error_source(&error).is_none());
        fs::remove_dir(&path).unwrap();

        let task_id = Uuid::new_v4();
        let malformed = write_state(&directory, task_id, b"{");
        let error = expect_error(read_document(&malformed));
        assert!(matches!(error, TaskRoutingStateError::InvalidFormat(_)));
        assert_eq!(
            error.to_string(),
            "task routing state is invalid or unsupported"
        );
        assert!(error_source(&error).is_some());

        let unknown_field = write_state(
            &directory,
            Uuid::new_v4(),
            br#"{"version":1,"snapshot":null,"checkpoint":null,"extra":true}"#,
        );
        let error = expect_error(read_document(&unknown_field));
        assert!(matches!(error, TaskRoutingStateError::InvalidFormat(_)));
        assert_eq!(
            error.to_string(),
            "task routing state is invalid or unsupported"
        );
        assert!(error_source(&error).is_some());

        let oversized = write_state(
            &directory,
            Uuid::new_v4(),
            &vec![b'!'; MAX_STATE_BYTES as usize + 1],
        );
        let error = expect_error(read_document(&oversized));
        assert!(matches!(error, TaskRoutingStateError::TooLarge));
        assert_eq!(
            error.to_string(),
            "task routing state exceeds its storage limit"
        );
        assert!(error_source(&error).is_none());

        let unsupported = write_state(
            &directory,
            Uuid::new_v4(),
            br#"{"version":2,"snapshot":null,"checkpoint":null}"#,
        );
        let error = expect_error(read_document(&unsupported));
        assert!(matches!(error, TaskRoutingStateError::UnsupportedVersion));
        assert_eq!(
            error.to_string(),
            "task routing state is inconsistent or unsupported"
        );
        assert!(error_source(&error).is_none());

        let task_id = Uuid::new_v4();
        let paths = paths(&directory);
        let (mut store, _) = TaskRoutingStateStore::open(&paths, task_id).unwrap();
        assert!(store.persist_routing_snapshot(
            &AutomaticRoutingRequest::default(),
            &[candidate()],
            enabled_fallback_policy(1_000_000, false).unwrap(),
            None,
        ));
        let path = state_path(&paths, task_id);
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        document["snapshot"]["candidates"] = serde_json::json!([]);
        write_state(&directory, task_id, &serde_json::to_vec(&document).unwrap());
        let error = TaskRoutingStateStore::open(&paths, task_id).err().unwrap();
        assert!(matches!(error, TaskRoutingStateError::InconsistentDocument));
        assert_eq!(
            error.to_string(),
            "task routing state is inconsistent or unsupported"
        );
        assert!(error_source(&error).is_none());
    }

    #[test]
    fn directory_removal_errors_preserve_paths_kinds_and_sources() {
        let directory = TempDir::new();
        let settings_paths = paths(&directory);
        let task_id = Uuid::new_v4();
        remove_task_routing_state(&settings_paths, task_id).unwrap();
        remove_task_routing_states(&settings_paths).unwrap();

        let parent_file = directory.path().join("not-a-directory");
        fs::write(&parent_file, b"file").unwrap();
        let mut invalid_parent_paths = paths(&directory);
        invalid_parent_paths.model = parent_file.join("config.toml");
        let expected_kind = fs::read_dir(&parent_file).unwrap_err().kind();
        let error = remove_task_routing_states(&invalid_parent_paths).unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::InspectDirectory,
            &parent_file,
            expected_kind,
            &format!(
                "cannot inspect task routing state directory '{}': {expected_kind}",
                parent_file.display()
            ),
        );

        let error = remove_task_routing_state_entries(
            directory.path(),
            [Err(io::Error::other("entry probe"))],
        )
        .unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::InspectDirectory,
            directory.path(),
            io::ErrorKind::Other,
            &format!(
                "cannot inspect task routing state directory '{}': {}",
                directory.path().display(),
                io::ErrorKind::Other
            ),
        );

        let blocked_path = state_path(&settings_paths, task_id);
        fs::create_dir(&blocked_path).unwrap();
        let expected_kind = fs::remove_file(&blocked_path).unwrap_err().kind();
        let error = remove_task_routing_state(&settings_paths, task_id).unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Remove,
            &blocked_path,
            expected_kind,
            &format!(
                "cannot remove task routing state '{}': {expected_kind}",
                blocked_path.display()
            ),
        );
        let expected_kind = fs::remove_file(&blocked_path).unwrap_err().kind();
        let error = remove_task_routing_states(&settings_paths).unwrap_err();
        assert_state_io_error(
            &error,
            TaskRoutingStateIoAction::Remove,
            &blocked_path,
            expected_kind,
            &format!(
                "cannot remove task routing state '{}': {expected_kind}",
                blocked_path.display()
            ),
        );

        let corrupt_id = Uuid::new_v4();
        write_state(&directory, corrupt_id, b"{");
        let error = has_task_routing_state(&settings_paths, corrupt_id).unwrap_err();
        assert!(matches!(error, TaskRoutingStateError::InvalidFormat(_)));
        assert_eq!(
            error.to_string(),
            "task routing state is invalid or unsupported"
        );
        assert!(error_source(&error).is_some());
    }

    #[test]
    fn fallback_policy_errors_keep_their_type_source_and_display() {
        let error = fallback_policy_from_limits(FallbackLimits {
            max_additional_attempts: 0,
            max_total_provider_requests: 3,
            max_total_estimated_cost_micro_usd: 1_000,
            initial_backoff: Duration::from_millis(250),
            maximum_backoff: Duration::from_secs(2),
            allow_cross_provider_data_transfer: false,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            TaskRoutingStateError::FallbackPolicy(
                crate::fallback::FallbackPolicyError::InvalidAttemptLimit
            )
        ));
        assert_eq!(
            error.to_string(),
            "invalid bounded fallback configuration: fallback attempt limit must be from 1 through 5"
        );
        assert_eq!(
            error_source(&error)
                .and_then(|source| source.downcast_ref::<crate::fallback::FallbackPolicyError>()),
            Some(&crate::fallback::FallbackPolicyError::InvalidAttemptLimit)
        );
    }

    fn assert_state_io_error(
        error: &TaskRoutingStateError,
        expected_action: TaskRoutingStateIoAction,
        expected_path: &Path,
        expected_kind: io::ErrorKind,
        expected_display: &str,
    ) {
        match error {
            TaskRoutingStateError::Io {
                action,
                path,
                source,
            } => {
                assert_eq!(*action, expected_action);
                assert_eq!(path, expected_path);
                assert_eq!(source.kind(), expected_kind);
            }
            error => panic!("expected state I/O error, got {error:?}"),
        }
        assert_eq!(error.to_string(), expected_display);
        let source = error_source(error).expect("I/O source preserved");
        assert_eq!(
            source.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(expected_kind)
        );
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
