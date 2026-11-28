//! Per-user model defaults and provider credentials used by `krypper setup`.

use super::config::{
    config_path, validate_route_settings, RoutingMode, MAX_FALLBACK_BUDGET_MICRO_USD,
};
use super::credential_vault::{self, CredentialsEnvelope, MasterKeyStore, SecretBytes, VaultError};
use crate::brain::{is_provider_credential_name, validate_model_name_for_provider, ModelProvider};
use crate::tools::permission_policy::PermissionMode;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use zeroize::Zeroize;

const CREDENTIALS_FILE: &str = "credentials.json";
const CREDENTIAL_TRANSACTION_FILE: &str = "credentials.json.pending";
const CREDENTIAL_LOCK_FILE: &str = "credentials.json.lock";
const PROVIDER_PROFILES_FILE: &str = "provider-profiles.json";
const MODEL_EVIDENCE_FILE: &str = "model-evidence.json";
const MODEL_EVIDENCE_KEY_FILE: &str = "model-evidence.key";
const PERMISSION_MODE_FILE: &str = "permission-mode";
const MAX_CREDENTIAL_BYTES: u64 = 96 * 1024;
const MAX_CREDENTIAL_PLAINTEXT_BYTES: usize = 64 * 1024;
const CREDENTIAL_TRANSACTION_VERSION: u8 = 1;
const MAX_PERMISSION_MODE_BYTES: u64 = 128;
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

pub(super) struct CredentialStore {
    values: BTreeMap<String, String>,
    key_reference: Option<String>,
}

#[derive(Debug)]
pub(super) enum CredentialStoreError {
    Io {
        action: SettingsIoAction,
        path: PathBuf,
        kind: io::ErrorKind,
    },
    Permissions(String),
    NotRegularFile(PathBuf),
    TooLarge(PathBuf),
    InvalidJson(PathBuf),
    InvalidValue(PathBuf),
    LegacyPlaintext(PathBuf),
    UnsupportedVersion(PathBuf),
    InvalidEnvelope(PathBuf),
    MissingVaultKey(PathBuf),
    VaultUnavailable,
    SecureRandomUnavailable,
    AuthenticationFailed(PathBuf),
    TransactionInterrupted(PathBuf),
    StaleStore(PathBuf),
}

#[derive(Debug)]
pub(super) enum PermissionModeSettingsError {
    Io {
        action: SettingsIoAction,
        path: PathBuf,
        source: io::Error,
    },
    Permissions(PrivatePermissionsError),
    NotRegularFile(PathBuf),
    TooLarge(PathBuf),
    InvalidUtf8(PathBuf),
    InvalidValue {
        path: PathBuf,
        reason: &'static str,
    },
    Write(PrivateFileWriteError),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SettingsIoAction {
    Inspect,
    Read,
}

impl SettingsIoAction {
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Read => "read",
        }
    }
}

#[derive(Debug)]
pub(super) enum PrivatePermissionsError {
    Message(String),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl PrivatePermissionsError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }

    fn into_io_source(self) -> Option<io::Error> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Message(_) => None,
        }
    }
}

impl fmt::Display for PrivatePermissionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Io {
                action,
                path,
                source,
            } => write!(
                formatter,
                "{action} '{}': {}",
                path.display(),
                source.kind()
            ),
        }
    }
}

impl std::error::Error for PrivatePermissionsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Message(_) => None,
        }
    }
}

impl CredentialStoreError {
    fn io(action: SettingsIoAction, path: &Path, cause: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            kind: cause.kind(),
        }
    }
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { action, path, kind } => {
                write!(
                    formatter,
                    "cannot {} credential store '{}': {kind}",
                    action.verb(),
                    path.display()
                )
            }
            Self::Permissions(message) => formatter.write_str(message),
            Self::NotRegularFile(path) => write!(
                formatter,
                "credential store '{}' is not a regular file",
                path.display()
            ),
            Self::TooLarge(path) => write!(
                formatter,
                "credential store '{}' is larger than {MAX_CREDENTIAL_BYTES} bytes",
                path.display()
            ),
            Self::InvalidJson(path) => write!(
                formatter,
                "credential store '{}' is invalid JSON; run `krypper setup` to replace it",
                path.display()
            ),
            Self::InvalidValue(path) => write!(
                formatter,
                "credential store '{}' contains an unsupported or invalid value",
                path.display()
            ),
            Self::LegacyPlaintext(path) => write!(
                formatter,
                "credential store '{}' contains legacy plaintext values; run `krypper setup` and approve migration to the system credential vault",
                path.display()
            ),
            Self::UnsupportedVersion(path) => write!(
                formatter,
                "credential store '{}' uses an unsupported encrypted format version",
                path.display()
            ),
            Self::InvalidEnvelope(path) => write!(
                formatter,
                "credential store '{}' has an invalid encrypted envelope",
                path.display()
            ),
            Self::MissingVaultKey(path) => write!(
                formatter,
                "the system credential vault is missing the key for '{}'; saved credentials are unavailable",
                path.display()
            ),
            Self::VaultUnavailable => formatter.write_str(
                "the system credential vault is unavailable or locked; the credential operation could not be completed",
            ),
            Self::SecureRandomUnavailable => formatter.write_str(
                "the secure random source is unavailable; credentials were not changed",
            ),
            Self::AuthenticationFailed(path) => write!(
                formatter,
                "credential store '{}' failed authentication; saved credentials were not loaded",
                path.display()
            ),
            Self::TransactionInterrupted(path) => write!(
                formatter,
                "credential storage at '{}' has an interrupted update; retry `krypper setup` when the system credential vault is available",
                path.display()
            ),
            Self::StaleStore(path) => write!(
                formatter,
                "credential store '{}' changed during this operation; reload settings and retry",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CredentialStoreError {}

impl PermissionModeSettingsError {
    fn io(action: SettingsIoAction, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for PermissionModeSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => {
                write!(
                    formatter,
                    "cannot {} permission settings '{}': {}",
                    action.verb(),
                    path.display(),
                    source.kind()
                )
            }
            Self::Permissions(error) => fmt::Display::fmt(error, formatter),
            Self::Write(error) => fmt::Display::fmt(error, formatter),
            Self::NotRegularFile(path) => write!(
                formatter,
                "permission settings '{}' are not a regular file",
                path.display()
            ),
            Self::TooLarge(path) => write!(
                formatter,
                "permission settings '{}' are larger than {MAX_PERMISSION_MODE_BYTES} bytes",
                path.display()
            ),
            Self::InvalidUtf8(path) => write!(
                formatter,
                "permission settings '{}' are not valid UTF-8",
                path.display()
            ),
            Self::InvalidValue { path, reason } => write!(
                formatter,
                "invalid permission settings '{}': {reason}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PermissionModeSettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Permissions(source) => Some(source),
            Self::Write(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialTransaction {
    version: u8,
    old_key_reference: Option<String>,
    new_key_reference: Option<String>,
}

enum ExistingCredentialDocument {
    Missing,
    Legacy(BTreeMap<String, String>),
    Encrypted(CredentialsEnvelope),
}

#[derive(Debug)]
pub(super) enum PrivateFileWriteError {
    BeforeCommit {
        message: String,
        source: Option<io::Error>,
    },
    AfterCommit {
        message: String,
        source: Option<io::Error>,
    },
}

impl PrivateFileWriteError {
    fn before(message: impl Into<String>) -> Self {
        Self::BeforeCommit {
            message: message.into(),
            source: None,
        }
    }

    fn before_io(action: &str, path: &Path, source: io::Error) -> Self {
        let message = format!("{action} '{}': {}", path.display(), source.kind());
        Self::BeforeCommit {
            message,
            source: Some(source),
        }
    }

    fn before_permissions(error: PrivatePermissionsError) -> Self {
        let message = error.to_string();
        let source = error.into_io_source();
        Self::BeforeCommit { message, source }
    }
}

impl fmt::Display for PrivateFileWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCommit { message, .. } | Self::AfterCommit { message, .. } => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for PrivateFileWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeCommit { source, .. } | Self::AfterCommit { source, .. } => source
                .as_ref()
                .map(|source| source as &(dyn std::error::Error + 'static)),
        }
    }
}

trait CredentialFileWriter {
    fn write_journal(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;
    fn write_document(&self, path: &Path, bytes: &[u8]) -> Result<(), PrivateFileWriteError>;
}

struct PrivateCredentialFileWriter;

impl CredentialFileWriter for PrivateCredentialFileWriter {
    fn write_journal(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        write_private_file(path, bytes, "credential transaction journal")
    }

    fn write_document(&self, path: &Path, bytes: &[u8]) -> Result<(), PrivateFileWriteError> {
        write_private_file_with_outcome(path, bytes, "credential store", sync_parent_directory)
    }
}

struct CredentialFileLock(File);

impl CredentialFileLock {
    fn acquire(path: &Path) -> Result<Self, CredentialStoreError> {
        let parent = path
            .parent()
            .ok_or_else(|| CredentialStoreError::InvalidValue(path.to_owned()))?;
        create_private_directory(parent)
            .map_err(|cause| CredentialStoreError::io(SettingsIoAction::Inspect, parent, cause))?;
        set_private_directory_permissions(parent)
            .map_err(|error| CredentialStoreError::Permissions(error.to_string()))?;
        let lock_path = path.with_file_name(CREDENTIAL_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(&lock_path).map_err(|cause| {
            CredentialStoreError::io(SettingsIoAction::Inspect, &lock_path, cause)
        })?;
        let metadata = file.metadata().map_err(|cause| {
            CredentialStoreError::io(SettingsIoAction::Inspect, &lock_path, cause)
        })?;
        if !metadata.is_file() {
            return Err(CredentialStoreError::NotRegularFile(lock_path));
        }
        ensure_private_permissions(&lock_path, &metadata, "credential transaction lock")
            .map_err(CredentialStoreError::Permissions)?;
        FileExt::lock_exclusive(&file).map_err(|cause| {
            CredentialStoreError::io(SettingsIoAction::Inspect, &lock_path, cause)
        })?;
        Ok(Self(file))
    }
}

impl Drop for CredentialFileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Credential resolution without a `Debug` implementation so values stay redacted.
pub(super) enum EffectiveCredentialValue {
    Configured(String),
    Missing,
    InvalidEnvironment,
}

pub(super) fn effective_credential_value(
    environment_value: Result<String, std::env::VarError>,
    saved_value: Option<&str>,
) -> EffectiveCredentialValue {
    match environment_value {
        Ok(value) => EffectiveCredentialValue::Configured(value),
        Err(std::env::VarError::NotPresent) => saved_value.map(str::to_owned).map_or(
            EffectiveCredentialValue::Missing,
            EffectiveCredentialValue::Configured,
        ),
        Err(std::env::VarError::NotUnicode(_)) => EffectiveCredentialValue::InvalidEnvironment,
    }
}

impl CredentialStore {
    pub(super) fn empty() -> Self {
        Self {
            values: BTreeMap::new(),
            key_reference: None,
        }
    }

    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub(super) fn insert(&mut self, name: &str, value: String) {
        if let Some(mut old_value) = self.values.insert(name.to_owned(), value) {
            old_value.zeroize();
        }
    }

    pub(super) fn remove(&mut self, name: &str) {
        if let Some(mut value) = self.values.remove(name) {
            value.zeroize();
        }
    }

    pub(super) fn len(&self) -> usize {
        self.values.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl Drop for CredentialStore {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}

#[derive(Clone)]
pub(super) struct UserSettingsPaths {
    pub(super) model: PathBuf,
    pub(super) credentials: PathBuf,
    pub(super) permission_mode: PathBuf,
    pub(super) provider_profiles: PathBuf,
    pub(super) model_evidence: PathBuf,
    pub(super) model_evidence_key: PathBuf,
}

/// Returns the ordinary per-user paths. This deliberately ignores
/// `KRYPPER_CONFIG`, which can point inside a project and is only a read
/// override for task defaults.
pub(super) fn user_settings_paths() -> Result<Option<UserSettingsPaths>, String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let Some(model) = config_path(None, std::env::var_os("XDG_CONFIG_HOME"), home)? else {
        return Ok(None);
    };
    Ok(Some(UserSettingsPaths {
        credentials: model.with_file_name(CREDENTIALS_FILE),
        permission_mode: model.with_file_name(PERMISSION_MODE_FILE),
        provider_profiles: model.with_file_name(PROVIDER_PROFILES_FILE),
        model_evidence: model.with_file_name(MODEL_EVIDENCE_FILE),
        model_evidence_key: model.with_file_name(MODEL_EVIDENCE_KEY_FILE),
        model,
    }))
}

/// Load the permission preference from its dedicated per-user file.
///
/// This deliberately does not use `KRYPPER_CONFIG`, which may point into a
/// project. A missing file means the conservative Ask mode.
pub(super) fn load_permission_mode(
    path: &Path,
) -> Result<PermissionMode, PermissionModeSettingsError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(PermissionMode::Ask),
        Err(cause) => {
            return Err(PermissionModeSettingsError::io(
                SettingsIoAction::Inspect,
                path,
                cause,
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(PermissionModeSettingsError::NotRegularFile(path.to_owned()));
    }
    let file = open_private_file(path)
        .map_err(|cause| PermissionModeSettingsError::io(SettingsIoAction::Read, path, cause))?;
    let opened_metadata = file
        .metadata()
        .map_err(|cause| PermissionModeSettingsError::io(SettingsIoAction::Inspect, path, cause))?;
    if !opened_metadata.is_file() {
        return Err(PermissionModeSettingsError::NotRegularFile(path.to_owned()));
    }
    ensure_private_permissions_with_source(path, &opened_metadata, "permission settings")
        .map_err(PermissionModeSettingsError::Permissions)?;
    let mut bytes = Vec::new();
    file.take(MAX_PERMISSION_MODE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| PermissionModeSettingsError::io(SettingsIoAction::Read, path, cause))?;
    if bytes.len() as u64 > MAX_PERMISSION_MODE_BYTES {
        return Err(PermissionModeSettingsError::TooLarge(path.to_owned()));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| PermissionModeSettingsError::InvalidUtf8(path.to_owned()))?
        .trim();
    PermissionMode::parse(value).map_err(|reason| PermissionModeSettingsError::InvalidValue {
        path: path.to_owned(),
        reason,
    })
}

pub(super) fn save_permission_mode(
    path: &Path,
    mode: PermissionMode,
) -> Result<(), PermissionModeSettingsError> {
    let contents = format!("{}\n", mode.as_str());
    write_private_file_with_outcome(
        path,
        contents.as_bytes(),
        "permission settings",
        sync_parent_directory,
    )
    .map_err(PermissionModeSettingsError::Write)
}

pub(super) fn load_credentials(
    path: &Path,
) -> Result<Option<CredentialStore>, CredentialStoreError> {
    let key_store = credential_vault::default_master_key_store();
    load_credentials_using(
        path,
        &*key_store,
        open_private_file,
        &PrivateCredentialFileWriter,
    )
}

#[cfg(test)]
fn load_credentials_with_open_file(
    path: &Path,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<Option<CredentialStore>, CredentialStoreError> {
    let key_store = credential_vault::default_master_key_store();
    load_credentials_using(path, &*key_store, open_file, &PrivateCredentialFileWriter)
}

fn load_credentials_using(
    path: &Path,
    key_store: &dyn MasterKeyStore,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
    writer: &dyn CredentialFileWriter,
) -> Result<Option<CredentialStore>, CredentialStoreError> {
    if !credential_file_or_journal_exists(path)? {
        return Ok(None);
    }
    let _lock = CredentialFileLock::acquire(path)?;
    recover_credential_transaction(path, key_store, open_private_file, writer)?;
    load_credentials_unlocked(path, key_store, open_file)
}

fn load_credentials_unlocked(
    path: &Path,
    key_store: &dyn MasterKeyStore,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<Option<CredentialStore>, CredentialStoreError> {
    let Some(bytes) = read_private_bytes(path, MAX_CREDENTIAL_BYTES, open_file)? else {
        return Ok(None);
    };
    let bytes = SecretBytes::new(bytes);
    match parse_existing_document(path, bytes.as_slice())? {
        ExistingCredentialDocument::Missing => Ok(None),
        ExistingCredentialDocument::Legacy(mut values) => {
            zeroize_values(&mut values);
            Err(CredentialStoreError::LegacyPlaintext(path.to_owned()))
        }
        ExistingCredentialDocument::Encrypted(envelope) => {
            let key_reference = envelope.key_reference().to_owned();
            let key = key_store
                .retrieve(&key_reference)
                .map_err(|cause| map_vault_error(path, cause))?;
            let plaintext = credential_vault::decrypt(&envelope, key.as_slice())
                .map_err(|cause| map_vault_error(path, cause))?;
            let mut values: BTreeMap<String, String> = serde_json::from_slice(plaintext.as_slice())
                .map_err(|_| CredentialStoreError::InvalidValue(path.to_owned()))?;
            if plaintext.as_slice().len() > MAX_CREDENTIAL_PLAINTEXT_BYTES
                || validate_credential_values(path, &values).is_err()
            {
                zeroize_values(&mut values);
                return Err(CredentialStoreError::InvalidValue(path.to_owned()));
            }
            Ok(Some(CredentialStore {
                values,
                key_reference: Some(key_reference),
            }))
        }
    }
}

pub(super) fn load_legacy_credentials(
    path: &Path,
) -> Result<CredentialStore, CredentialStoreError> {
    let key_store = credential_vault::default_master_key_store();
    if !credential_file_or_journal_exists(path)? {
        return Err(CredentialStoreError::InvalidJson(path.to_owned()));
    }
    let _lock = CredentialFileLock::acquire(path)?;
    recover_credential_transaction(
        path,
        &*key_store,
        open_private_file,
        &PrivateCredentialFileWriter,
    )?;
    let Some(bytes) = read_private_bytes(path, MAX_CREDENTIAL_BYTES, open_private_file)? else {
        return Err(CredentialStoreError::InvalidJson(path.to_owned()));
    };
    let bytes = SecretBytes::new(bytes);
    match parse_existing_document(path, bytes.as_slice())? {
        ExistingCredentialDocument::Legacy(values) => Ok(CredentialStore {
            values,
            key_reference: None,
        }),
        ExistingCredentialDocument::Encrypted(_) | ExistingCredentialDocument::Missing => {
            Err(CredentialStoreError::InvalidJson(path.to_owned()))
        }
    }
}

#[cfg(test)]
pub(super) fn save_model_defaults(path: &Path, provider: &str, model: &str) -> Result<(), String> {
    save_model_configuration(path, provider, model, RoutingMode::Manual, false, None)
}

pub(super) fn save_model_configuration(
    path: &Path,
    provider: &str,
    model: &str,
    routing_mode: RoutingMode,
    allow_cross_provider_data_transfer: bool,
    fallback_budget_micro_usd: Option<u64>,
) -> Result<(), String> {
    let Some(provider_kind) = ModelProvider::parse(provider) else {
        return Err("unsupported provider".to_owned());
    };
    validate_model_name_for_provider(model, provider_kind)
        .map_err(|_| "invalid model ID".to_owned())?;
    validate_route_settings(
        routing_mode,
        allow_cross_provider_data_transfer,
        fallback_budget_micro_usd,
    )
    .map_err(str::to_owned)?;
    if fallback_budget_micro_usd
        .is_some_and(|budget| budget == 0 || budget > MAX_FALLBACK_BUDGET_MICRO_USD)
    {
        return Err(format!(
            "fallback budget must be between 1 and {MAX_FALLBACK_BUDGET_MICRO_USD} micro-USD"
        ));
    }
    let configuration = ModelConfiguration {
        provider,
        model,
        routing_mode: (routing_mode == RoutingMode::Automatic).then_some("automatic"),
        allow_cross_provider_data_transfer: allow_cross_provider_data_transfer.then_some("true"),
        fallback_budget_micro_usd: fallback_budget_micro_usd.map(|budget| budget.to_string()),
    };
    let contents = toml::to_string(&configuration)
        .map_err(|_| "cannot serialize model configuration".to_owned())?;
    write_private_file(path, contents.as_bytes(), "model configuration")
}

#[derive(serde::Serialize)]
struct ModelConfiguration<'a> {
    provider: &'a str,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_cross_provider_data_transfer: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_budget_micro_usd: Option<String>,
}

pub(super) fn save_credentials(
    path: &Path,
    credentials: &mut CredentialStore,
) -> Result<(), String> {
    save_credentials_with_legacy_replacement(path, credentials, false)
}

pub(super) fn save_credentials_after_approved_migration(
    path: &Path,
    credentials: &mut CredentialStore,
) -> Result<(), String> {
    save_credentials_with_legacy_replacement(path, credentials, true)
}

fn save_credentials_with_legacy_replacement(
    path: &Path,
    credentials: &mut CredentialStore,
    allow_legacy_replacement: bool,
) -> Result<(), String> {
    let key_store = credential_vault::default_master_key_store();
    save_credentials_using(
        path,
        credentials,
        &*key_store,
        &PrivateCredentialFileWriter,
        allow_legacy_replacement,
    )
    .map_err(|error| error.to_string())
}

fn save_credentials_using(
    path: &Path,
    credentials: &mut CredentialStore,
    key_store: &dyn MasterKeyStore,
    writer: &dyn CredentialFileWriter,
    allow_legacy_replacement: bool,
) -> Result<(), CredentialStoreError> {
    let _lock = CredentialFileLock::acquire(path)?;
    recover_credential_transaction(path, key_store, open_private_file, writer)?;
    let mut existing = inspect_existing_document(path, open_private_file)?;
    verify_existing_document(path, &mut existing, key_store)?;
    let old_key_reference = match existing {
        ExistingCredentialDocument::Missing => {
            if credentials.key_reference.is_some() {
                return Err(CredentialStoreError::StaleStore(path.to_owned()));
            }
            None
        }
        ExistingCredentialDocument::Legacy(mut values) => {
            zeroize_values(&mut values);
            if !allow_legacy_replacement {
                return Err(CredentialStoreError::LegacyPlaintext(path.to_owned()));
            }
            if credentials.key_reference.is_some() {
                return Err(CredentialStoreError::StaleStore(path.to_owned()));
            }
            None
        }
        ExistingCredentialDocument::Encrypted(envelope) => {
            let active_reference = envelope.key_reference().to_owned();
            if credentials.key_reference.as_deref() != Some(active_reference.as_str()) {
                return Err(CredentialStoreError::StaleStore(path.to_owned()));
            }
            Some(active_reference)
        }
    };

    if credentials.is_empty() {
        remove_credentials_unlocked(path, old_key_reference, key_store, writer)?;
        credentials.key_reference = None;
        return Ok(());
    }

    let plaintext = SecretBytes::new(
        serde_json::to_vec(&credentials.values)
            .map_err(|_| CredentialStoreError::InvalidValue(path.to_owned()))?,
    );
    if plaintext.as_slice().len() > MAX_CREDENTIAL_PLAINTEXT_BYTES {
        return Err(CredentialStoreError::TooLarge(path.to_owned()));
    }
    let new_key_reference = uuid::Uuid::new_v4().to_string();
    let new_key = credential_vault::create_key().map_err(|cause| map_vault_error(path, cause))?;
    let nonce = credential_vault::create_nonce().map_err(|cause| map_vault_error(path, cause))?;
    let journal = CredentialTransaction {
        version: CREDENTIAL_TRANSACTION_VERSION,
        old_key_reference: old_key_reference.clone(),
        new_key_reference: Some(new_key_reference.clone()),
    };
    write_credential_journal(path, &journal, writer)?;

    if key_store
        .store(&new_key_reference, new_key.as_slice())
        .is_err()
    {
        let _ = recover_credential_transaction(path, key_store, open_private_file, writer);
        return Err(CredentialStoreError::VaultUnavailable);
    }
    let envelope = match credential_vault::encrypt(
        new_key.as_slice(),
        &new_key_reference,
        nonce.as_slice(),
        plaintext,
    ) {
        Ok(envelope) => envelope,
        Err(cause) => {
            let _ = recover_credential_transaction(path, key_store, open_private_file, writer);
            return Err(map_vault_error(path, cause));
        }
    };
    let bytes = match credential_vault::serialize_envelope(&envelope) {
        Ok(bytes) => bytes,
        Err(cause) => {
            recover_credential_transaction(path, key_store, open_private_file, writer)?;
            return Err(map_vault_error(path, cause));
        }
    };
    match writer.write_document(path, &bytes) {
        Ok(()) => {
            credentials.key_reference = Some(new_key_reference);
            recover_credential_transaction(path, key_store, open_private_file, writer)
                .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
        }
        Err(PrivateFileWriteError::BeforeCommit { .. }) => {
            recover_credential_transaction(path, key_store, open_private_file, writer)?;
            Err(CredentialStoreError::TransactionInterrupted(
                path.to_owned(),
            ))
        }
        Err(PrivateFileWriteError::AfterCommit { .. }) => {
            credentials.key_reference = Some(new_key_reference);
            Err(CredentialStoreError::TransactionInterrupted(
                path.to_owned(),
            ))
        }
    }
}

fn remove_credentials(path: &Path) -> Result<(), CredentialStoreError> {
    let key_store = credential_vault::default_master_key_store();
    let _lock = CredentialFileLock::acquire(path)?;
    recover_credential_transaction(
        path,
        &*key_store,
        open_private_file,
        &PrivateCredentialFileWriter,
    )?;
    let existing = inspect_existing_document(path, open_private_file)?;
    match existing {
        ExistingCredentialDocument::Missing => Ok(()),
        ExistingCredentialDocument::Legacy(mut values) => {
            zeroize_values(&mut values);
            remove_file_if_present(path, "credential store")
                .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
        }
        ExistingCredentialDocument::Encrypted(envelope) => {
            let old_key_reference = envelope.key_reference().to_owned();
            let key = key_store
                .retrieve(&old_key_reference)
                .map_err(|cause| map_vault_error(path, cause))?;
            let plaintext = credential_vault::decrypt(&envelope, key.as_slice())
                .map_err(|cause| map_vault_error(path, cause))?;
            let mut values: BTreeMap<String, String> = serde_json::from_slice(plaintext.as_slice())
                .map_err(|_| CredentialStoreError::InvalidValue(path.to_owned()))?;
            if validate_credential_values(path, &values).is_err() {
                zeroize_values(&mut values);
                return Err(CredentialStoreError::InvalidValue(path.to_owned()));
            }
            zeroize_values(&mut values);
            let journal = CredentialTransaction {
                version: CREDENTIAL_TRANSACTION_VERSION,
                old_key_reference: Some(old_key_reference),
                new_key_reference: None,
            };
            write_credential_journal(path, &journal, &PrivateCredentialFileWriter)?;
            if remove_file_if_present(path, "credential store").is_err() {
                recover_credential_transaction(
                    path,
                    &*key_store,
                    open_private_file,
                    &PrivateCredentialFileWriter,
                )?;
                return Err(CredentialStoreError::TransactionInterrupted(
                    path.to_owned(),
                ));
            }
            recover_credential_transaction(
                path,
                &*key_store,
                open_private_file,
                &PrivateCredentialFileWriter,
            )
            .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
        }
    }
}

pub(super) fn remove_settings(paths: &UserSettingsPaths) -> Result<(), String> {
    remove_credentials(&paths.credentials).map_err(|error| error.to_string())?;
    remove_file_if_present(&paths.provider_profiles, "provider profiles")?;
    remove_file_if_present(&paths.model_evidence, "provider evidence")?;
    remove_file_if_present(&paths.model_evidence_key, "provider evidence key")?;
    remove_file_if_present(&paths.permission_mode, "permission settings")?;
    remove_file_if_present(
        &paths.model.with_file_name("model-catalog-cache.json"),
        "model catalog cache",
    )?;
    super::task_routing_state::remove_task_routing_states(paths)
        .map_err(|error| error.to_string())?;
    remove_file_if_present(&paths.model, "model configuration")
}

fn credential_file_or_journal_exists(path: &Path) -> Result<bool, CredentialStoreError> {
    for candidate in [path.to_owned(), credential_journal_path(path)] {
        match fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(true),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
            Err(cause) => {
                return Err(CredentialStoreError::io(
                    SettingsIoAction::Inspect,
                    &candidate,
                    cause,
                ));
            }
        }
    }
    Ok(false)
}

fn read_private_bytes(
    path: &Path,
    maximum_bytes: u64,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<Option<Vec<u8>>, CredentialStoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => {
            return Err(CredentialStoreError::io(
                SettingsIoAction::Inspect,
                path,
                cause,
            ));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(CredentialStoreError::NotRegularFile(path.to_owned()));
    }
    let file = open_file(path)
        .map_err(|cause| CredentialStoreError::io(SettingsIoAction::Read, path, cause))?;
    let opened_metadata = file
        .metadata()
        .map_err(|cause| CredentialStoreError::io(SettingsIoAction::Inspect, path, cause))?;
    if !opened_metadata.is_file() {
        return Err(CredentialStoreError::NotRegularFile(path.to_owned()));
    }
    ensure_private_permissions(path, &opened_metadata, "credential store")
        .map_err(CredentialStoreError::Permissions)?;
    let mut bytes = Vec::new();
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| CredentialStoreError::io(SettingsIoAction::Read, path, cause))?;
    if bytes.len() as u64 > maximum_bytes {
        bytes.zeroize();
        return Err(CredentialStoreError::TooLarge(path.to_owned()));
    }
    Ok(Some(bytes))
}

fn inspect_existing_document(
    path: &Path,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<ExistingCredentialDocument, CredentialStoreError> {
    let Some(mut bytes) = read_private_bytes(path, MAX_CREDENTIAL_BYTES, open_file)? else {
        return Ok(ExistingCredentialDocument::Missing);
    };
    let document = parse_existing_document(path, &bytes);
    bytes.zeroize();
    document
}

fn parse_existing_document(
    path: &Path,
    bytes: &[u8],
) -> Result<ExistingCredentialDocument, CredentialStoreError> {
    match CredentialsEnvelope::parse(bytes) {
        Ok(envelope) => Ok(ExistingCredentialDocument::Encrypted(envelope)),
        Err(VaultError::UnsupportedVersion) => {
            Err(CredentialStoreError::UnsupportedVersion(path.to_owned()))
        }
        Err(VaultError::InvalidEnvelope) => {
            match serde_json::from_slice::<BTreeMap<String, String>>(bytes) {
                Ok(mut values) => match validate_credential_values(path, &values) {
                    Ok(()) => Ok(ExistingCredentialDocument::Legacy(values)),
                    Err(error) => {
                        zeroize_values(&mut values);
                        Err(error)
                    }
                },
                Err(_) => {
                    let looks_encrypted = serde_json::from_slice::<serde_json::Value>(bytes)
                        .ok()
                        .and_then(|value| value.as_object().cloned())
                        .is_some_and(|object| object.contains_key("format"));
                    if looks_encrypted {
                        Err(CredentialStoreError::InvalidEnvelope(path.to_owned()))
                    } else {
                        Err(CredentialStoreError::InvalidJson(path.to_owned()))
                    }
                }
            }
        }
        Err(_) => Err(CredentialStoreError::InvalidEnvelope(path.to_owned())),
    }
}

fn validate_credential_values(
    path: &Path,
    values: &BTreeMap<String, String>,
) -> Result<(), CredentialStoreError> {
    if values
        .iter()
        .any(|(name, value)| !is_provider_credential_name(name) || !valid_credential(value))
    {
        Err(CredentialStoreError::InvalidValue(path.to_owned()))
    } else {
        Ok(())
    }
}

fn zeroize_values(values: &mut BTreeMap<String, String>) {
    for value in values.values_mut() {
        value.zeroize();
    }
}

fn map_vault_error(path: &Path, error: VaultError) -> CredentialStoreError {
    match error {
        VaultError::Unavailable => CredentialStoreError::VaultUnavailable,
        VaultError::RandomSourceUnavailable => CredentialStoreError::SecureRandomUnavailable,
        VaultError::MissingKey => CredentialStoreError::MissingVaultKey(path.to_owned()),
        VaultError::InvalidKey | VaultError::InvalidEnvelope => {
            CredentialStoreError::InvalidEnvelope(path.to_owned())
        }
        VaultError::UnsupportedVersion => CredentialStoreError::UnsupportedVersion(path.to_owned()),
        VaultError::AuthenticationFailed => {
            CredentialStoreError::AuthenticationFailed(path.to_owned())
        }
    }
}

fn credential_journal_path(path: &Path) -> PathBuf {
    path.with_file_name(CREDENTIAL_TRANSACTION_FILE)
}

fn write_credential_journal(
    path: &Path,
    journal: &CredentialTransaction,
    writer: &dyn CredentialFileWriter,
) -> Result<(), CredentialStoreError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))?;
    writer
        .write_journal(&credential_journal_path(path), &bytes)
        .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
}

fn read_credential_journal(
    path: &Path,
) -> Result<Option<CredentialTransaction>, CredentialStoreError> {
    let journal_path = credential_journal_path(path);
    let Some(mut bytes) = read_private_bytes(&journal_path, 4096, open_private_file)? else {
        return Ok(None);
    };
    let parsed = serde_json::from_slice::<CredentialTransaction>(&bytes)
        .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()));
    bytes.zeroize();
    let journal = parsed?;
    if journal.version != CREDENTIAL_TRANSACTION_VERSION
        || journal
            .old_key_reference
            .as_deref()
            .is_some_and(|reference| !valid_key_reference(reference))
        || journal
            .new_key_reference
            .as_deref()
            .is_some_and(|reference| !valid_key_reference(reference))
    {
        return Err(CredentialStoreError::TransactionInterrupted(
            path.to_owned(),
        ));
    }
    Ok(Some(journal))
}

fn valid_key_reference(reference: &str) -> bool {
    uuid::Uuid::parse_str(reference).is_ok_and(|parsed| parsed.to_string() == reference)
}

fn recover_credential_transaction(
    path: &Path,
    key_store: &dyn MasterKeyStore,
    open_file: impl FnOnce(&Path) -> io::Result<File>,
    _writer: &dyn CredentialFileWriter,
) -> Result<(), CredentialStoreError> {
    let Some(journal) = read_credential_journal(path)? else {
        return Ok(());
    };
    let mut existing = inspect_existing_document(path, open_file)?;
    let active_reference = match &existing {
        ExistingCredentialDocument::Encrypted(envelope) => {
            Some(envelope.key_reference().to_owned())
        }
        ExistingCredentialDocument::Missing | ExistingCredentialDocument::Legacy(_) => None,
    };
    verify_existing_document(path, &mut existing, key_store)?;
    match journal.new_key_reference.as_deref() {
        Some(new_reference) if active_reference.as_deref() == Some(new_reference) => {
            if let Some(old_reference) = journal.old_key_reference.as_deref() {
                if old_reference != new_reference {
                    delete_master_key(path, key_store, old_reference)?;
                }
            }
        }
        Some(new_reference)
            if active_reference == journal.old_key_reference
                && (active_reference.is_some()
                    || matches!(
                        existing,
                        ExistingCredentialDocument::Missing | ExistingCredentialDocument::Legacy(_)
                    )) =>
        {
            delete_master_key(path, key_store, new_reference)?;
        }
        None if active_reference == journal.old_key_reference => {
            // The file removal did not commit; keep the referenced key.
        }
        None if active_reference.is_none()
            && matches!(existing, ExistingCredentialDocument::Missing) =>
        {
            if let Some(old_reference) = journal.old_key_reference.as_deref() {
                delete_master_key(path, key_store, old_reference)?;
            }
        }
        _ => {
            return Err(CredentialStoreError::TransactionInterrupted(
                path.to_owned(),
            ));
        }
    }
    remove_file_if_present(
        &credential_journal_path(path),
        "credential transaction journal",
    )
    .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
}

fn verify_existing_document(
    path: &Path,
    document: &mut ExistingCredentialDocument,
    key_store: &dyn MasterKeyStore,
) -> Result<(), CredentialStoreError> {
    match document {
        ExistingCredentialDocument::Missing => Ok(()),
        ExistingCredentialDocument::Legacy(values) => {
            let result = validate_credential_values(path, values);
            zeroize_values(values);
            result
        }
        ExistingCredentialDocument::Encrypted(envelope) => {
            let key = key_store
                .retrieve(envelope.key_reference())
                .map_err(|cause| map_vault_error(path, cause))?;
            let plaintext = credential_vault::decrypt(envelope, key.as_slice())
                .map_err(|cause| map_vault_error(path, cause))?;
            let mut values: BTreeMap<String, String> = serde_json::from_slice(plaintext.as_slice())
                .map_err(|_| CredentialStoreError::InvalidValue(path.to_owned()))?;
            let result = if plaintext.as_slice().len() > MAX_CREDENTIAL_PLAINTEXT_BYTES {
                Err(CredentialStoreError::InvalidValue(path.to_owned()))
            } else {
                validate_credential_values(path, &values)
            };
            zeroize_values(&mut values);
            result
        }
    }
}

fn delete_master_key(
    path: &Path,
    key_store: &dyn MasterKeyStore,
    key_reference: &str,
) -> Result<(), CredentialStoreError> {
    key_store
        .delete(key_reference)
        .map_err(|cause| map_vault_error(path, cause))
}

fn remove_credentials_unlocked(
    path: &Path,
    old_key_reference: Option<String>,
    key_store: &dyn MasterKeyStore,
    writer: &dyn CredentialFileWriter,
) -> Result<(), CredentialStoreError> {
    let Some(old_key_reference) = old_key_reference else {
        remove_file_if_present(path, "credential store")
            .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))?;
        return Ok(());
    };
    let journal = CredentialTransaction {
        version: CREDENTIAL_TRANSACTION_VERSION,
        old_key_reference: Some(old_key_reference),
        new_key_reference: None,
    };
    write_credential_journal(path, &journal, writer)?;
    if remove_file_if_present(path, "credential store").is_err() {
        recover_credential_transaction(path, key_store, open_private_file, writer)?;
        return Err(CredentialStoreError::TransactionInterrupted(
            path.to_owned(),
        ));
    }
    recover_credential_transaction(path, key_store, open_private_file, writer)
        .map_err(|_| CredentialStoreError::TransactionInterrupted(path.to_owned()))
}

pub(super) fn valid_credential(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8 * 1024
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

pub(super) fn write_private_file(
    path: &Path,
    bytes: &[u8],
    description: &str,
) -> Result<(), String> {
    write_private_file_with_directory_sync(path, bytes, description, sync_parent_directory)
}

fn write_private_file_with_directory_sync(
    path: &Path,
    bytes: &[u8],
    description: &str,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), String> {
    write_private_file_with_outcome(path, bytes, description, sync_directory).map_err(|error| {
        match error {
            PrivateFileWriteError::BeforeCommit { message, .. }
            | PrivateFileWriteError::AfterCommit { message, .. } => message,
        }
    })
}

fn write_private_file_with_outcome(
    path: &Path,
    bytes: &[u8],
    description: &str,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), PrivateFileWriteError> {
    let parent = path.parent().ok_or_else(|| {
        PrivateFileWriteError::before(format!(
            "cannot locate the user configuration directory for {description}"
        ))
    })?;
    create_private_directory(parent).map_err(|cause| {
        PrivateFileWriteError::before_io(
            "cannot create user configuration directory",
            parent,
            cause,
        )
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|cause| {
        PrivateFileWriteError::before_io(
            "cannot inspect user configuration directory",
            parent,
            cause,
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(PrivateFileWriteError::before(format!(
            "user configuration path '{}' is not a directory",
            parent.display()
        )));
    }
    set_private_directory_permissions(parent).map_err(PrivateFileWriteError::before_permissions)?;

    if let Ok(existing) = fs::symlink_metadata(path) {
        if !existing.file_type().is_file() {
            return Err(PrivateFileWriteError::before(format!(
                "cannot replace {description} '{}': it is not a regular file",
                path.display()
            )));
        }
    }

    let temp_path = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path).map_err(|cause| {
        PrivateFileWriteError::before_io("cannot create temporary settings file", &temp_path, cause)
    })?;
    if let Err(cause) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(PrivateFileWriteError::before_io(
            "cannot write settings file",
            &temp_path,
            cause,
        ));
    }
    if let Err(error) = set_private_file_permissions(&temp_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(PrivateFileWriteError::before_permissions(error));
    }
    drop(file);
    fs::rename(&temp_path, path).map_err(|cause| {
        let _ = fs::remove_file(&temp_path);
        PrivateFileWriteError::before_io("cannot save settings file", path, cause)
    })?;
    sync_directory(parent).map_err(|cause| PrivateFileWriteError::AfterCommit {
        message: format!(
            "saved {description} to '{}', but could not sync its parent directory: {}",
            path.display(),
            cause.kind()
        ),
        source: Some(cause),
    })
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

pub(super) fn remove_file_if_present(path: &Path, description: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(file_error(
            &format!("cannot remove {description}"),
            path,
            cause,
        )),
    }
}

fn file_error(action: &str, path: &Path, cause: io::Error) -> String {
    format!("{action} '{}': {}", path.display(), cause.kind())
}

pub(super) fn ensure_private_permissions(
    path: &Path,
    metadata: &fs::Metadata,
    description: &str,
) -> Result<(), String> {
    ensure_private_permissions_with_source(path, metadata, description)
        .map_err(|error| error.to_string())
}

pub(super) fn ensure_private_permissions_with_source(
    path: &Path,
    metadata: &fs::Metadata,
    description: &str,
) -> Result<(), PrivatePermissionsError> {
    #[cfg(unix)]
    {
        let current_uid = current_user_id();
        ensure_private_metadata_for_user(path, metadata, current_uid, description)
            .map_err(PrivatePermissionsError::Message)?;
        if let Some(parent) = path.parent() {
            let parent_directory = open_private_directory(parent).map_err(|cause| {
                PrivatePermissionsError::io(
                    "cannot inspect user configuration directory",
                    parent,
                    cause,
                )
            })?;
            let parent_metadata = parent_directory.metadata().map_err(|cause| {
                PrivatePermissionsError::io(
                    "cannot inspect user configuration directory",
                    parent,
                    cause,
                )
            })?;
            if !parent_metadata.is_dir() {
                return Err(PrivatePermissionsError::Message(format!(
                    "user configuration directory '{}' must be private to the current user",
                    parent.display()
                )));
            }
            ensure_private_metadata_for_user(
                parent,
                &parent_metadata,
                current_uid,
                "user configuration directory",
            )
            .map_err(PrivatePermissionsError::Message)?;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, metadata, description);
    Ok(())
}

#[cfg(unix)]
fn ensure_private_metadata_for_user(
    path: &Path,
    metadata: &fs::Metadata,
    expected_uid: u32,
    description: &str,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if metadata.uid() != expected_uid {
        return Err(format!(
            "{description} '{}' is not owned by the current user",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{description} '{}' is accessible by other users; restrict its permissions or run `krypper setup --reset`",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn current_user_id() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the effective uid of this process.
    unsafe { libc::geteuid() }
}

fn set_private_directory_permissions(path: &Path) -> Result<(), PrivatePermissionsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let directory = open_private_directory(path).map_err(|cause| {
            PrivatePermissionsError::io("cannot open user configuration directory", path, cause)
        })?;
        let metadata = directory.metadata().map_err(|cause| {
            PrivatePermissionsError::io("cannot inspect user configuration directory", path, cause)
        })?;
        if !metadata.is_dir() {
            return Err(PrivatePermissionsError::Message(format!(
                "user configuration path '{}' is not a directory",
                path.display()
            )));
        }
        if metadata.uid() != current_user_id() {
            return Err(PrivatePermissionsError::Message(format!(
                "user configuration directory '{}' is not owned by the current user",
                path.display()
            )));
        }
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|cause| {
                PrivatePermissionsError::io(
                    "cannot restrict user configuration directory",
                    path,
                    cause,
                )
            })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), PrivatePermissionsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|cause| {
            PrivatePermissionsError::io("cannot restrict settings file permissions", path, cause)
        })?;
    }
    let _ = path;
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(unix)]
fn open_private_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

#[cfg(not(unix))]
pub(super) fn open_private_file(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestKeyStore {
        keys: Mutex<HashMap<String, Vec<u8>>>,
        locked: bool,
    }

    impl TestKeyStore {
        fn key_count(&self) -> usize {
            self.keys.lock().unwrap().len()
        }
    }

    impl MasterKeyStore for TestKeyStore {
        fn store(&self, key_reference: &str, key: &[u8]) -> Result<(), VaultError> {
            if self.locked {
                return Err(VaultError::Unavailable);
            }
            self.keys
                .lock()
                .map_err(|_| VaultError::Unavailable)?
                .insert(key_reference.to_owned(), key.to_vec());
            Ok(())
        }

        fn retrieve(&self, key_reference: &str) -> Result<SecretBytes, VaultError> {
            if self.locked {
                return Err(VaultError::Unavailable);
            }
            self.keys
                .lock()
                .map_err(|_| VaultError::Unavailable)?
                .get(key_reference)
                .cloned()
                .map(SecretBytes::new)
                .ok_or(VaultError::MissingKey)
        }

        fn delete(&self, key_reference: &str) -> Result<(), VaultError> {
            if self.locked {
                return Err(VaultError::Unavailable);
            }
            if let Some(mut key) = self
                .keys
                .lock()
                .map_err(|_| VaultError::Unavailable)?
                .remove(key_reference)
            {
                key.zeroize();
            }
            Ok(())
        }
    }

    impl Drop for TestKeyStore {
        fn drop(&mut self) {
            if let Ok(keys) = self.keys.get_mut() {
                for key in keys.values_mut() {
                    key.zeroize();
                }
            }
        }
    }

    struct FailBeforeCredentialReplace;

    impl CredentialFileWriter for FailBeforeCredentialReplace {
        fn write_journal(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            PrivateCredentialFileWriter.write_journal(path, bytes)
        }

        fn write_document(&self, _path: &Path, _bytes: &[u8]) -> Result<(), PrivateFileWriteError> {
            Err(PrivateFileWriteError::before(
                "simulated interruption before replacement",
            ))
        }
    }

    struct CommitThenReportSyncFailure;

    impl CredentialFileWriter for CommitThenReportSyncFailure {
        fn write_journal(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            PrivateCredentialFileWriter.write_journal(path, bytes)
        }

        fn write_document(&self, path: &Path, bytes: &[u8]) -> Result<(), PrivateFileWriteError> {
            write_private_file(path, bytes, "credential store")
                .map_err(PrivateFileWriteError::before)?;
            Err(PrivateFileWriteError::AfterCommit {
                message: "simulated directory sync failure".to_owned(),
                source: None,
            })
        }
    }

    fn credentials_with_secret(secret: &str) -> CredentialStore {
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", secret.to_owned());
        credentials
    }

    fn credential_error<T>(result: Result<T, CredentialStoreError>) -> CredentialStoreError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("credential operation unexpectedly succeeded"),
        }
    }

    #[test]
    fn remove_settings_preserves_bulk_state_error_text_and_partial_progress() {
        let empty_directory = TempDir::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(empty_directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let empty_paths = UserSettingsPaths {
            model: empty_directory.path().join("config.toml"),
            credentials: empty_directory.path().join("credentials.json"),
            permission_mode: empty_directory.path().join(PERMISSION_MODE_FILE),
            provider_profiles: empty_directory.path().join(PROVIDER_PROFILES_FILE),
            model_evidence: empty_directory.path().join(MODEL_EVIDENCE_FILE),
            model_evidence_key: empty_directory.path().join(MODEL_EVIDENCE_KEY_FILE),
        };
        remove_settings(&empty_paths).unwrap();

        let directory = TempDir::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let paths = UserSettingsPaths {
            model: directory.path().join("config.toml"),
            credentials: directory.path().join("credentials.json"),
            permission_mode: directory.path().join(PERMISSION_MODE_FILE),
            provider_profiles: directory.path().join(PROVIDER_PROFILES_FILE),
            model_evidence: directory.path().join(MODEL_EVIDENCE_FILE),
            model_evidence_key: directory.path().join(MODEL_EVIDENCE_KEY_FILE),
        };
        fs::write(&paths.model, b"provider = 'ollama'\n").unwrap();
        fs::write(&paths.provider_profiles, b"profiles").unwrap();
        let blocked_state = paths.model.with_file_name("task-routing-blocked.json");
        fs::create_dir(&blocked_state).unwrap();
        let expected_kind = fs::remove_file(&blocked_state).unwrap_err().kind();

        let error = remove_settings(&paths).unwrap_err();
        assert_eq!(
            error,
            format!(
                "cannot remove task routing state '{}': {expected_kind}",
                blocked_state.display()
            )
        );
        assert!(!paths.provider_profiles.exists());
        assert!(paths.model.exists());
        assert!(blocked_state.is_dir());
    }

    #[test]
    fn permission_mode_is_per_user_and_defaults_to_ask() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(PERMISSION_MODE_FILE);

        assert_eq!(load_permission_mode(&path).unwrap(), PermissionMode::Ask);
        save_permission_mode(&path, PermissionMode::RiskBased).unwrap();
        assert_eq!(
            load_permission_mode(&path).unwrap(),
            PermissionMode::RiskBased
        );

        save_permission_mode(&path, PermissionMode::FullAccess).unwrap();
        assert_eq!(
            load_permission_mode(&path).unwrap(),
            PermissionMode::FullAccess
        );
    }

    #[test]
    fn permission_mode_settings_reject_oversized_files() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(PERMISSION_MODE_FILE);
        save_permission_mode(&path, PermissionMode::Ask).unwrap();

        let oversized = vec![b'a'; MAX_PERMISSION_MODE_BYTES as usize + 1];
        write_private_file(&path, &oversized, "permission settings").unwrap();
        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::TooLarge(error_path) if error_path == &path
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "permission settings '{}' are larger than {MAX_PERMISSION_MODE_BYTES} bytes",
                path.display()
            )
        );
    }

    #[test]
    fn permission_mode_settings_reject_invalid_utf8() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(PERMISSION_MODE_FILE);
        write_private_file(&path, &[0xff], "permission settings").unwrap();

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::InvalidUtf8(error_path) if error_path == &path
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "permission settings '{}' are not valid UTF-8",
                path.display()
            )
        );
    }

    #[test]
    fn permission_mode_settings_reject_unknown_values() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(PERMISSION_MODE_FILE);
        write_private_file(&path, b"unknown-mode\n", "permission settings").unwrap();
        let expected_reason = PermissionMode::parse("unknown-mode").unwrap_err();

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::InvalidValue {
                path: error_path,
                reason,
            }
                if error_path == &path && *reason == expected_reason
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "invalid permission settings '{}': {expected_reason}",
                path.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_mode_settings_reject_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(PERMISSION_MODE_FILE);
        save_permission_mode(&path, PermissionMode::Ask).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::Permissions(PrivatePermissionsError::Message(message))
                if message.contains("accessible by other users")
        ));
        assert!(error
            .to_string()
            .contains("accessible by other users; restrict its permissions"));
    }

    #[test]
    fn permission_mode_save_errors_have_a_write_category() {
        let directory = TempDir::new();
        let regular_file = directory.path().join("not-a-directory");
        fs::write(&regular_file, b"file").unwrap();
        let path = regular_file.join(PERMISSION_MODE_FILE);

        let error = save_permission_mode(&path, PermissionMode::RiskBased).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::Write(PrivateFileWriteError::BeforeCommit { .. })
        ));
        assert!(
            std::error::Error::source(std::error::Error::source(&error).unwrap())
                .unwrap()
                .downcast_ref::<io::Error>()
                .is_some()
        );
        assert!(!error.to_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn permission_mode_settings_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new();
        let settings_directory = directory.path().join("krypper");
        fs::create_dir_all(&settings_directory).unwrap();
        let path = settings_directory.join(PERMISSION_MODE_FILE);
        let target = directory.path().join("target");
        fs::write(&target, b"risk-based\n").unwrap();
        symlink(&target, &path).unwrap();

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::NotRegularFile(error_path) if error_path == &path
        ));
        assert_eq!(
            error.to_string(),
            format!(
                "permission settings '{}' are not a regular file",
                path.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_mode_io_errors_preserve_the_source() {
        let directory = TempDir::new();
        let regular_file = directory.path().join("not-a-directory");
        fs::write(&regular_file, b"file").unwrap();
        let path = regular_file.join(PERMISSION_MODE_FILE);

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::Io {
                action: SettingsIoAction::Inspect,
                path: error_path,
                source,
            } if error_path == &path && source.kind() == io::ErrorKind::NotADirectory
        ));
        assert!(std::error::Error::source(&error).is_some());
        assert_eq!(
            error.to_string(),
            format!(
                "cannot inspect permission settings '{}': {}",
                path.display(),
                io::ErrorKind::NotADirectory
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_mode_parent_inspection_errors_preserve_the_source() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new();
        let actual_directory = directory.path().join("actual");
        fs::create_dir(&actual_directory).unwrap();
        let actual_path = actual_directory.join(PERMISSION_MODE_FILE);
        save_permission_mode(&actual_path, PermissionMode::Ask).unwrap();
        let parent_alias = directory.path().join("alias");
        symlink(&actual_directory, &parent_alias).unwrap();
        let path = parent_alias.join(PERMISSION_MODE_FILE);

        let error = load_permission_mode(&path).unwrap_err();
        assert!(matches!(
            &error,
            PermissionModeSettingsError::Permissions(PrivatePermissionsError::Io {
                action: "cannot inspect user configuration directory",
                path: error_path,
                ..
            }) if error_path == &parent_alias
        ));
        let permission_source = std::error::Error::source(&error).unwrap();
        assert!(std::error::Error::source(permission_source)
            .unwrap()
            .downcast_ref::<io::Error>()
            .is_some());
    }

    #[test]
    fn saved_credentials_round_trip_and_error_without_secret_disclosure() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let secret = "test-token-never-print";
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", secret.to_owned());
        save_credentials(&path, &mut credentials).unwrap();

        let serialized = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&serialized).contains(secret));
        assert!(CredentialsEnvelope::parse(&serialized).is_ok());
        let loaded = load_credentials(&path).unwrap().unwrap();
        assert_eq!(loaded.get("OPENAI_API_KEY"), Some(secret));
        fs::write(&path, b"not json").unwrap();
        let error = match load_credentials(&path) {
            Err(error) => error,
            Ok(_) => panic!("invalid credentials must fail to load"),
        };
        assert!(matches!(error, CredentialStoreError::InvalidJson(_)));
        assert!(!error.to_string().contains(secret));
        assert_eq!(
            error.to_string(),
            format!(
                "credential store '{}' is invalid JSON; run `krypper setup` to replace it",
                path.display()
            )
        );
    }

    #[test]
    fn updates_rotate_the_key_and_removal_revokes_it() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let key_store = TestKeyStore::default();
        let writer = PrivateCredentialFileWriter;
        let mut credentials = credentials_with_secret("first-key");

        save_credentials_using(&path, &mut credentials, &key_store, &writer, false).unwrap();
        let first_reference = credentials.key_reference.clone().unwrap();
        assert_eq!(key_store.key_count(), 1);

        credentials.insert("OPENAI_API_KEY", "rotated-key".to_owned());
        save_credentials_using(&path, &mut credentials, &key_store, &writer, false).unwrap();
        let second_reference = credentials.key_reference.clone().unwrap();
        assert_ne!(first_reference, second_reference);
        assert_eq!(key_store.key_count(), 1);
        assert_eq!(
            load_credentials_using(&path, &key_store, open_private_file, &writer)
                .unwrap()
                .unwrap()
                .get("OPENAI_API_KEY"),
            Some("rotated-key")
        );

        remove_credentials_unlocked(&path, Some(second_reference), &key_store, &writer).unwrap();
        assert!(!path.exists());
        assert_eq!(key_store.key_count(), 0);
    }

    #[test]
    fn unavailable_or_missing_vault_key_never_falls_back_to_plaintext() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let key_store = TestKeyStore::default();
        let writer = PrivateCredentialFileWriter;
        let mut credentials = credentials_with_secret("saved-secret");
        save_credentials_using(&path, &mut credentials, &key_store, &writer, false).unwrap();
        let serialized = fs::read(&path).unwrap();

        let locked_store = TestKeyStore {
            keys: Mutex::new(HashMap::new()),
            locked: true,
        };
        let locked = credential_error(load_credentials_using(
            &path,
            &locked_store,
            open_private_file,
            &writer,
        ));
        assert!(matches!(locked, CredentialStoreError::VaultUnavailable));
        assert_eq!(fs::read(&path).unwrap(), serialized);

        let reference = credentials.key_reference.as_deref().unwrap();
        key_store.delete(reference).unwrap();
        let missing = credential_error(load_credentials_using(
            &path,
            &key_store,
            open_private_file,
            &writer,
        ));
        assert!(matches!(missing, CredentialStoreError::MissingVaultKey(_)));
        assert_eq!(fs::read(&path).unwrap(), serialized);
    }

    #[test]
    fn encrypted_references_are_scoped_to_the_key_that_created_them() {
        let directory = TempDir::new();
        let first_path = directory.path().join("first/credentials.json");
        let second_path = directory.path().join("second/credentials.json");
        let key_store = TestKeyStore::default();
        let writer = PrivateCredentialFileWriter;
        let mut first = credentials_with_secret("first-vault-secret");
        let mut second = credentials_with_secret("second-vault-secret");
        save_credentials_using(&first_path, &mut first, &key_store, &writer, false).unwrap();
        save_credentials_using(&second_path, &mut second, &key_store, &writer, false).unwrap();
        assert_ne!(
            first.key_reference.as_deref(),
            second.key_reference.as_deref()
        );
        assert_eq!(key_store.key_count(), 2);

        let mut first_envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&first_path).unwrap()).unwrap();
        first_envelope["key_reference"] = second.key_reference.clone().unwrap().into();
        write_private_file(
            &first_path,
            &serde_json::to_vec(&first_envelope).unwrap(),
            "credential store",
        )
        .unwrap();
        let error = credential_error(load_credentials_using(
            &first_path,
            &key_store,
            open_private_file,
            &writer,
        ));
        assert!(matches!(
            error,
            CredentialStoreError::AuthenticationFailed(_)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected_without_secret_disclosure() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let key_store = TestKeyStore::default();
        let writer = PrivateCredentialFileWriter;
        let mut credentials = credentials_with_secret("tamper-test-secret");
        save_credentials_using(&path, &mut credentials, &key_store, &writer, false).unwrap();

        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).expect("encrypted envelope is JSON");
        let ciphertext = envelope["ciphertext"]
            .as_str()
            .expect("ciphertext field is a string")
            .to_owned();
        let replacement = if ciphertext.starts_with('A') {
            'B'
        } else {
            'A'
        };
        envelope["ciphertext"] = format!("{replacement}{}", &ciphertext[1..]).into();
        write_private_file(
            &path,
            &serde_json::to_vec(&envelope).unwrap(),
            "credential store",
        )
        .unwrap();

        let error = credential_error(load_credentials_using(
            &path,
            &key_store,
            open_private_file,
            &writer,
        ));
        assert!(matches!(
            error,
            CredentialStoreError::AuthenticationFailed(_)
        ));
        assert!(!error.to_string().contains("tamper-test-secret"));
    }

    #[test]
    fn interrupted_migration_before_replacement_preserves_plaintext_and_cleans_new_key() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let mut legacy = BTreeMap::new();
        legacy.insert("OPENAI_API_KEY".to_owned(), "legacy-secret".to_owned());
        let original = serde_json::to_vec(&legacy).unwrap();
        write_private_file(&path, &original, "credential store").unwrap();
        let mut credentials = CredentialStore {
            values: legacy,
            key_reference: None,
        };
        let key_store = TestKeyStore::default();

        let error = save_credentials_using(
            &path,
            &mut credentials,
            &key_store,
            &FailBeforeCredentialReplace,
            true,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CredentialStoreError::TransactionInterrupted(_)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(key_store.key_count(), 0);
        assert!(!credential_journal_path(&path).exists());
    }

    #[test]
    fn next_load_recovers_a_committed_file_after_sync_uncertainty() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let key_store = TestKeyStore::default();
        let private_writer = PrivateCredentialFileWriter;
        let mut credentials = credentials_with_secret("interrupted-rotation");

        let error = save_credentials_using(
            &path,
            &mut credentials,
            &key_store,
            &CommitThenReportSyncFailure,
            false,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CredentialStoreError::TransactionInterrupted(_)
        ));
        assert!(credential_journal_path(&path).exists());
        let loaded = load_credentials_using(&path, &key_store, open_private_file, &private_writer)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.get("OPENAI_API_KEY"), Some("interrupted-rotation"));
        assert!(!credential_journal_path(&path).exists());
        assert_eq!(key_store.key_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn saved_credentials_and_parent_directory_have_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", "test-token".to_owned());
        save_credentials(&path, &mut credentials).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn model_defaults_are_written_without_secrets() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join("config.toml");
        save_model_defaults(&path, "openai", "gpt-4.1").unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert_eq!(contents, "provider = \"openai\"\nmodel = \"gpt-4.1\"\n");
        assert!(!contents.contains("API_KEY"));
    }

    #[test]
    fn routing_model_configuration_round_trips_optional_settings() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join("config.toml");

        save_model_configuration(
            &path,
            "openai",
            "gpt-4.1",
            RoutingMode::Automatic,
            true,
            Some(25_000),
        )
        .unwrap();

        let defaults = super::super::config::load_model_defaults(&path)
            .unwrap()
            .expect("saved routing defaults");
        assert_eq!(defaults.provider.as_deref(), Some("openai"));
        assert_eq!(defaults.model, "gpt-4.1");
        assert_eq!(defaults.routing_mode, RoutingMode::Automatic);
        assert!(defaults.allow_cross_provider_data_transfer);
        assert_eq!(defaults.fallback_budget_micro_usd, Some(25_000));
    }

    #[test]
    fn model_defaults_round_trip_quotes_and_backslashes() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join("config.toml");
        let model = r#"model "quoted" \\ path"#;

        save_model_defaults(&path, "openai", model).unwrap();

        let defaults = super::super::config::load_model_defaults(&path)
            .unwrap()
            .expect("saved model defaults");
        assert_eq!(defaults.model, model);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_private_file_metadata_owned_by_another_user() {
        use std::os::unix::fs::MetadataExt;

        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", "test-token".to_owned());
        save_credentials(&path, &mut credentials).unwrap();
        let opened = open_private_file(&path).unwrap();
        let metadata = opened.metadata().unwrap();
        let another_uid = metadata.uid().wrapping_add(1);

        let error =
            ensure_private_metadata_for_user(&path, &metadata, another_uid, "credential store")
                .unwrap_err();

        assert!(error.contains("not owned by the current user"));
    }

    #[cfg(unix)]
    #[test]
    fn opening_a_fifo_does_not_block_before_descriptor_validation() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let directory = TempDir::new();
        let path = directory.path().join("credential-store");
        let encoded_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `encoded_path` is NUL-terminated and `mkfifo` does not retain it.
        let result = unsafe { libc::mkfifo(encoded_path.as_ptr(), 0o600) };
        assert_eq!(result, 0, "create FIFO fixture");

        let opened = open_private_file(&path).expect("nonblocking FIFO open");

        assert!(!opened.metadata().unwrap().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn credential_permissions_are_checked_on_the_opened_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let path = directory.path().join("krypper").join(CREDENTIALS_FILE);
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", "test-token".to_owned());
        save_credentials(&path, &mut credentials).unwrap();

        let result = super::load_credentials_with_open_file(&path, |path| {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            open_private_file(path)
        });

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("opened credential file with unsafe permissions must be rejected"),
        };
        assert!(error.to_string().contains("accessible by other users"));
    }

    #[test]
    fn reports_a_directory_sync_failure_after_the_file_was_saved() {
        let directory = TempDir::new();
        let path = directory.path().join("krypper").join("config.toml");
        let mut sync_calls = 0;

        let error = write_private_file_with_directory_sync(
            &path,
            b"saved contents",
            "model configuration",
            |parent| {
                sync_calls += 1;
                assert_eq!(parent, path.parent().unwrap());
                assert_eq!(fs::read(&path).unwrap(), b"saved contents");
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap_err();

        assert_eq!(sync_calls, 1);
        assert!(error.contains("saved"));
        assert!(error.contains("sync"));
        assert_eq!(fs::read(path).unwrap(), b"saved contents");
    }
}
