//! Per-user persistence for explicit provider probes and runtime capability evidence.

use super::settings::{
    ensure_private_permissions, open_private_file, write_private_file, UserSettingsPaths,
};
use crate::brain::capability_evidence::{
    configuration_revision, new_configuration_revision_key, now_unix_secs, CapabilityFreshness,
    ProviderEvidenceRecord, ProviderEvidenceStore,
};
use std::fs;
use std::io::{self, Read};
use std::path::Path;

const MAX_EVIDENCE_BYTES: u64 = 512 * 1024;
const REVISION_KEY_BYTES: usize = 64;

pub(super) fn configuration_revision_for_probe(
    paths: &UserSettingsPaths,
    provider: &str,
    model: &str,
    config_values: &[(&str, &str)],
) -> Result<String, String> {
    let key = load_or_create_revision_key(&paths.model_evidence_key)?;
    Ok(configuration_revision(&key, provider, model, config_values))
}

pub(super) fn existing_configuration_revision_for_probe(
    path: &Path,
    provider: &str,
    model: &str,
    config_values: &[(&str, &str)],
) -> Result<Option<String>, String> {
    match read_revision_key(path) {
        Ok(key) => Ok(Some(configuration_revision(
            &key,
            provider,
            model,
            config_values,
        ))),
        Err(RevisionKeyError::Missing) => Ok(None),
        Err(RevisionKeyError::Invalid) => Err(
            "provider evidence key is invalid; reset per-user settings to replace it".to_owned(),
        ),
        Err(RevisionKeyError::Io) => {
            Err("cannot read the private provider evidence key".to_owned())
        }
    }
}

pub(super) fn record_probe(
    path: &Path,
    record: ProviderEvidenceRecord,
) -> Result<CapabilityFreshness, String> {
    let mut store = load(path)?;
    let provider = record.provider.clone();
    let model = record.model.clone();
    let revision = record.configuration_revision.clone();
    store
        .record(record)
        .map_err(|_| "provider evidence is invalid".to_owned())?;
    let bytes = serde_json::to_vec_pretty(&store)
        .map_err(|_| "cannot serialize provider evidence".to_owned())?;
    if bytes.len() as u64 > MAX_EVIDENCE_BYTES {
        return Err("provider evidence exceeds its storage limit".to_owned());
    }
    write_private_file(path, &bytes, "provider evidence")?;
    Ok(store.freshness(&provider, &model, &revision, now_unix_secs()))
}

pub(super) fn records(path: &Path) -> Result<Vec<ProviderEvidenceRecord>, String> {
    Ok(load(path)?.records().to_vec())
}

fn load(path: &Path) -> Result<ProviderEvidenceStore, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
            return Ok(ProviderEvidenceStore::new())
        }
        Err(cause) => {
            return Err(format!(
                "cannot inspect provider evidence '{}': {}",
                path.display(),
                cause.kind()
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "provider evidence '{}' is not a regular file",
            path.display()
        ));
    }
    let file = open_private_file(path).map_err(|cause| {
        format!(
            "cannot read provider evidence '{}': {}",
            path.display(),
            cause.kind()
        )
    })?;
    let opened_metadata = file.metadata().map_err(|cause| {
        format!(
            "cannot inspect provider evidence '{}': {}",
            path.display(),
            cause.kind()
        )
    })?;
    if !opened_metadata.is_file() {
        return Err(format!(
            "provider evidence '{}' is not a regular file",
            path.display()
        ));
    }
    ensure_private_permissions(path, &opened_metadata, "provider evidence")?;
    let mut bytes = Vec::new();
    file.take(MAX_EVIDENCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| {
            format!(
                "cannot read provider evidence '{}': {}",
                path.display(),
                cause.kind()
            )
        })?;
    if bytes.len() as u64 > MAX_EVIDENCE_BYTES {
        return Err("provider evidence exceeds its storage limit".to_owned());
    }
    ProviderEvidenceStore::from_json(&bytes)
        .map_err(|_| "provider evidence is invalid JSON or has an unsupported version".to_owned())
}

fn load_or_create_revision_key(path: &Path) -> Result<[u8; 32], String> {
    match read_revision_key(path) {
        Ok(key) => Ok(key),
        Err(RevisionKeyError::Missing) => {
            let key = new_configuration_revision_key();
            let encoded = encode_hex(&key);
            match write_private_file(path, encoded.as_bytes(), "provider evidence key") {
                Ok(()) => Ok(key),
                // A concurrent process may have created the file after our read.
                Err(_) => read_revision_key(path)
                    .map_err(|_| "cannot create the private provider evidence key".to_owned()),
            }
        }
        Err(RevisionKeyError::Invalid) => Err(
            "provider evidence key is invalid; reset per-user settings to replace it".to_owned(),
        ),
        Err(RevisionKeyError::Io) => {
            Err("cannot read the private provider evidence key".to_owned())
        }
    }
}

enum RevisionKeyError {
    Missing,
    Invalid,
    Io,
}

fn read_revision_key(path: &Path) -> Result<[u8; 32], RevisionKeyError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
            return Err(RevisionKeyError::Missing)
        }
        Err(_) => return Err(RevisionKeyError::Io),
    };
    if !metadata.file_type().is_file() {
        return Err(RevisionKeyError::Invalid);
    }
    let file = open_private_file(path).map_err(|_| RevisionKeyError::Io)?;
    let opened_metadata = file.metadata().map_err(|_| RevisionKeyError::Io)?;
    if !opened_metadata.is_file()
        || ensure_private_permissions(path, &opened_metadata, "provider evidence key").is_err()
    {
        return Err(RevisionKeyError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take((REVISION_KEY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| RevisionKeyError::Io)?;
    if bytes.len() != REVISION_KEY_BYTES {
        return Err(RevisionKeyError::Invalid);
    }
    decode_hex(&bytes).ok_or(RevisionKeyError::Invalid)
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(bytes: &[u8]) -> Option<[u8; 32]> {
    let mut decoded = [0_u8; 32];
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for (index, pair) in pairs.iter().enumerate() {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    Some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::capability_evidence::{EvidenceHealth, ProviderEvidenceObservation};
    use crate::brain::ModelTokenUsage;
    use crate::test_support::TempDir;

    fn paths(directory: &TempDir) -> UserSettingsPaths {
        UserSettingsPaths {
            model: directory.path().join("config.toml"),
            credentials: directory.path().join("credentials.json"),
            permission_mode: directory.path().join("permission-mode"),
            provider_profiles: directory.path().join("provider-profiles.json"),
            model_evidence: directory.path().join("model-evidence.json"),
            model_evidence_key: directory.path().join("model-evidence.key"),
            offline_process: directory.path().join("offline-process.json"),
        }
    }

    #[test]
    fn configuration_revision_key_is_private_stable_and_secret_free() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        let config = [("account", "account-1"), ("token", "test-secret-value")];
        let first =
            configuration_revision_for_probe(&paths, "cloudflare", "@cf/model", &config).unwrap();
        let second =
            configuration_revision_for_probe(&paths, "cloudflare", "@cf/model", &config).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("test-secret-value"));
        assert!(!fs::read_to_string(paths.model_evidence_key)
            .unwrap()
            .contains("test-secret-value"));
    }

    #[test]
    fn evidence_file_round_trips_only_the_allowlisted_record_fields() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        let revision = configuration_revision_for_probe(
            &paths,
            "cloudflare",
            "@cf/model",
            &[("token", "test-secret-value")],
        )
        .unwrap();
        let record = ProviderEvidenceRecord::new(
            "cloudflare",
            "@cf/model",
            &revision,
            ProviderEvidenceObservation {
                observed_at_unix_secs: 100,
                health: EvidenceHealth::Healthy,
                latency_ms: 19,
                rate_limit_status: None,
                retry_after_secs: None,
                failure_code: None,
                usage: Some(ModelTokenUsage {
                    prompt_tokens: Some(2),
                    completion_tokens: Some(3),
                    total_tokens: Some(5),
                }),
            },
        )
        .unwrap();
        record_probe(&paths.model_evidence, record).unwrap();
        let bytes = fs::read(&paths.model_evidence).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("test-secret-value"));
        assert!(!text.contains("prompt text"));
        assert!(!text.contains("response text"));
        let loaded = load(&paths.model_evidence).unwrap();
        assert_eq!(loaded.records().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn evidence_and_revision_key_have_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let paths = paths(&directory);
        let revision =
            configuration_revision_for_probe(&paths, "openai", "model-x", &[("api-key", "secret")])
                .unwrap();
        let record = ProviderEvidenceRecord::new(
            "openai",
            "model-x",
            &revision,
            ProviderEvidenceObservation {
                observed_at_unix_secs: 1,
                health: EvidenceHealth::Unhealthy,
                latency_ms: 4,
                rate_limit_status: Some(429),
                retry_after_secs: Some(3),
                failure_code: None,
                usage: None,
            },
        )
        .unwrap();
        record_probe(&paths.model_evidence, record).unwrap();
        assert_eq!(
            fs::metadata(paths.model_evidence_key)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(paths.model_evidence)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn reset_removes_local_evidence_and_its_revision_key() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        configuration_revision_for_probe(&paths, "openai", "model-x", &[]).unwrap();
        fs::write(&paths.model, "provider = \"openai\"\nmodel = \"model-x\"\n").unwrap();
        fs::write(&paths.model_evidence, b"placeholder evidence").unwrap();

        super::super::settings::remove_settings(&paths).unwrap();

        assert!(!paths.model_evidence.exists());
        assert!(!paths.model_evidence_key.exists());
        assert!(!paths.model.exists());
    }
}
