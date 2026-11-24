//! Trusted, per-user provider profile definitions.

use super::settings::{ensure_private_permissions, open_credentials};
#[cfg(test)]
use super::settings::{
    load_credentials, remove_file_if_present, save_credentials, write_private_file, CredentialStore,
};
use crate::brain::{
    provider_descriptor_by_selector, validate_endpoint, validate_model_name_for_provider,
    DecisionProtocol, ModelProvider,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

const MAX_PROVIDER_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_PROVIDER_PROFILES: usize = 16;
const MAX_PROFILE_ID_BYTES: usize = 32;
const MAX_PROFILE_LABEL_CHARS: usize = 64;
const MAX_PROFILE_ENDPOINT_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ProfileProtocol {
    #[serde(rename = "ollama-chat")]
    OllamaChat,
    #[serde(rename = "gemini-generate-content")]
    GeminiGenerateContent,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-compatible-chat")]
    OpenAiCompatibleChat,
}

impl ProfileProtocol {
    fn decision_protocol(self) -> DecisionProtocol {
        match self {
            Self::OllamaChat => DecisionProtocol::OllamaChat,
            Self::GeminiGenerateContent => DecisionProtocol::GeminiGenerateContent,
            Self::OpenAiResponses => DecisionProtocol::Responses,
            Self::AnthropicMessages => DecisionProtocol::AnthropicMessages,
            Self::OpenAiCompatibleChat => DecisionProtocol::CompatibleChat,
        }
    }

    fn has_api_key(self) -> bool {
        !matches!(self, Self::OllamaChat)
    }

    fn validate_model(self, model: &str) -> Result<(), &'static str> {
        let provider = match self {
            Self::OllamaChat => ModelProvider::Ollama,
            Self::GeminiGenerateContent => ModelProvider::Gemini,
            Self::OpenAiResponses => ModelProvider::OpenAi,
            Self::AnthropicMessages => ModelProvider::Anthropic,
            Self::OpenAiCompatibleChat => ModelProvider::OpenAi,
        };
        validate_model_name_for_provider(model, provider)
    }

    fn endpoint_for_model(self, base_or_endpoint: &str, model: &str) -> String {
        match self {
            Self::GeminiGenerateContent => format!(
                "{}/models/{model}:generateContent",
                base_or_endpoint.trim_end_matches('/')
            ),
            Self::OllamaChat => {
                format!("{}/api/chat", base_or_endpoint.trim_end_matches('/'))
            }
            Self::OpenAiResponses | Self::AnthropicMessages | Self::OpenAiCompatibleChat => {
                base_or_endpoint.to_owned()
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderProfile {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) enabled: bool,
    pub(super) protocol: ProfileProtocol,
    pub(super) endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) credential_ref: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderProfileDocument {
    version: u32,
    profiles: Vec<ProviderProfile>,
}

pub(super) fn load_provider_profiles(path: &Path) -> Result<Vec<ProviderProfile>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(cause) => {
            return Err(format!(
                "cannot inspect provider profiles '{}': {}",
                path.display(),
                cause.kind()
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "provider profiles '{}' is not a regular file",
            path.display()
        ));
    }
    ensure_private_permissions(path, &metadata)?;
    let file = open_credentials(path).map_err(|cause| {
        format!(
            "cannot read provider profiles '{}': {}",
            path.display(),
            cause.kind()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_PROVIDER_PROFILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| {
            format!(
                "cannot read provider profiles '{}': {}",
                path.display(),
                cause.kind()
            )
        })?;
    if bytes.len() as u64 > MAX_PROVIDER_PROFILE_BYTES {
        return Err(format!(
            "provider profiles '{}' exceeds {MAX_PROVIDER_PROFILE_BYTES} bytes",
            path.display()
        ));
    }
    let document: ProviderProfileDocument = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "provider profiles '{}' is invalid JSON or has unsupported fields",
            path.display()
        )
    })?;
    if document.version != 1 {
        return Err(format!(
            "provider profiles '{}' has an unsupported version",
            path.display()
        ));
    }
    validate_profiles(&document.profiles).map_err(|_| {
        format!(
            "provider profiles '{}' contains an invalid profile",
            path.display()
        )
    })?;
    let mut profiles = document.profiles;
    profiles.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(profiles)
}

#[cfg(test)]
pub(super) fn save_provider_profiles(
    path: &Path,
    profiles: &[ProviderProfile],
) -> Result<(), String> {
    validate_profiles(profiles).map_err(str::to_owned)?;
    if profiles.is_empty() {
        return remove_file_if_present(path, "provider profiles");
    }
    let mut profiles = profiles.to_vec();
    profiles.sort_by(|left, right| left.id.cmp(&right.id));
    let document = ProviderProfileDocument {
        version: 1,
        profiles,
    };
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|_| "cannot serialize provider profiles".to_owned())?;
    if bytes.len() as u64 > MAX_PROVIDER_PROFILE_BYTES {
        return Err(format!(
            "provider profiles exceed {MAX_PROVIDER_PROFILE_BYTES} bytes"
        ));
    }
    write_private_file(path, &bytes, "provider profiles")
}

#[cfg(test)]
pub(super) fn add_profile(path: &Path, profile: ProviderProfile) -> Result<(), String> {
    validate_profile(&profile).map_err(str::to_owned)?;
    let mut profiles = load_provider_profiles(path)?;
    if profiles.len() >= MAX_PROVIDER_PROFILES {
        return Err(format!(
            "at most {MAX_PROVIDER_PROFILES} profiles are supported"
        ));
    }
    if profiles.iter().any(|existing| existing.id == profile.id) {
        return Err("provider profile ID already exists".to_owned());
    }
    profiles.push(profile);
    save_provider_profiles(path, &profiles)
}

#[cfg(test)]
pub(super) fn replace_profile(path: &Path, profile: ProviderProfile) -> Result<(), String> {
    validate_profile(&profile).map_err(str::to_owned)?;
    let mut profiles = load_provider_profiles(path)?;
    let Some(existing) = profiles.iter_mut().find(|entry| entry.id == profile.id) else {
        return Err("provider profile ID does not exist".to_owned());
    };
    *existing = profile;
    save_provider_profiles(path, &profiles)
}

#[cfg(test)]
pub(super) fn set_profile_enabled(path: &Path, id: &str, enabled: bool) -> Result<(), String> {
    let mut profiles = load_provider_profiles(path)?;
    let Some(profile) = profiles.iter_mut().find(|profile| profile.id == id) else {
        return Err("provider profile ID does not exist".to_owned());
    };
    profile.enabled = enabled;
    save_provider_profiles(path, &profiles)
}

#[cfg(test)]
pub(super) fn remove_profile(
    profile_path: &Path,
    credential_path: &Path,
    id: &str,
) -> Result<(), String> {
    let mut profiles = load_provider_profiles(profile_path)?;
    let Some(index) = profiles.iter().position(|profile| profile.id == id) else {
        return Err("provider profile ID does not exist".to_owned());
    };
    let mut credentials = load_credentials(credential_path)?.unwrap_or_else(CredentialStore::empty);
    credentials.remove(&credential_name(id));
    save_credentials(credential_path, &credentials)?;
    profiles.remove(index);
    save_provider_profiles(profile_path, &profiles)
}

pub(super) fn selected_profile(
    profiles: &[ProviderProfile],
    id: &str,
    model: &str,
) -> Result<(DecisionProtocol, String, Option<String>), &'static str> {
    let profile = profiles
        .iter()
        .find(|profile| profile.id == id)
        .ok_or("unknown provider profile")?;
    if !profile.enabled {
        return Err("provider profile is disabled");
    }
    profile.protocol.validate_model(model)?;
    let endpoint = profile
        .protocol
        .endpoint_for_model(&profile.endpoint, model);
    validate_endpoint(&endpoint).map_err(|_| "invalid provider profile endpoint")?;
    Ok((
        profile.protocol.decision_protocol(),
        endpoint,
        profile
            .protocol
            .has_api_key()
            .then(|| credential_name(&profile.id)),
    ))
}

pub(super) fn is_profile_id_candidate(id: &str) -> bool {
    validate_profile_id(id).is_ok()
}

pub(super) fn credential_name(id: &str) -> String {
    format!(
        "KRYPPER_PROVIDER_{}_API_KEY",
        id.replace('-', "_").to_ascii_uppercase()
    )
}

fn validate_profiles(profiles: &[ProviderProfile]) -> Result<(), &'static str> {
    if profiles.len() > MAX_PROVIDER_PROFILES {
        return Err("too many provider profiles");
    }
    let mut ids = BTreeSet::new();
    for profile in profiles {
        validate_profile(profile)?;
        if !ids.insert(profile.id.as_str()) {
            return Err("duplicate provider profile ID");
        }
    }
    Ok(())
}

fn validate_profile(profile: &ProviderProfile) -> Result<(), &'static str> {
    let id = profile.id.as_str();
    validate_profile_id(id)?;
    if profile.label.trim().is_empty()
        || profile.label.trim() != profile.label
        || profile.label.chars().count() > MAX_PROFILE_LABEL_CHARS
        || profile.label.chars().any(char::is_control)
    {
        return Err("invalid provider profile label");
    }
    if profile.endpoint.len() > MAX_PROFILE_ENDPOINT_BYTES
        || profile.endpoint.contains(['?', '#'])
        || validate_endpoint(&profile.endpoint).is_err()
    {
        return Err("invalid provider profile endpoint");
    }
    let expected_reference = format!("profile:{id}:api-key");
    match (
        profile.protocol.has_api_key(),
        profile.credential_ref.as_deref(),
    ) {
        (true, Some(reference)) if reference == expected_reference => {}
        (false, None) => {}
        _ => return Err("invalid provider profile credential reference"),
    }
    Ok(())
}

fn validate_profile_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty()
        || id.len() > MAX_PROFILE_ID_BYTES
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id.as_bytes()[id.len() - 1].is_ascii_lowercase()
            && !id.as_bytes()[id.len() - 1].is_ascii_digit()
        || id.as_bytes().windows(2).any(|pair| pair == b"--")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || provider_descriptor_by_selector(id).is_some()
    {
        return Err("invalid or reserved provider profile ID");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("krypper-profiles-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn compatible_profile(id: &str, endpoint: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.to_owned(),
            label: "Test profile".to_owned(),
            enabled: true,
            protocol: ProfileProtocol::OpenAiCompatibleChat,
            endpoint: endpoint.to_owned(),
            credential_ref: Some(format!("profile:{id}:api-key")),
        }
    }

    #[test]
    fn profiles_round_trip_in_deterministic_id_order_without_secret_fields() {
        let directory = TempDir::new();
        let path = directory.0.join("krypper/provider-profiles.json");
        save_provider_profiles(
            &path,
            &[
                compatible_profile("zeta", "https://z.example/v1/chat/completions"),
                compatible_profile("alpha", "https://a.example/v1/chat/completions"),
            ],
        )
        .unwrap();
        let profiles = load_provider_profiles(&path).unwrap();
        assert_eq!(profiles[0].id, "alpha");
        assert_eq!(profiles[1].id, "zeta");
        let file = fs::read_to_string(path).unwrap();
        assert!(!file.contains("api_key"));
        assert!(!file.contains("secret"));
    }

    #[test]
    fn rejects_unknown_fields_duplicate_ids_and_untrusted_endpoints() {
        let directory = TempDir::new();
        let path = directory.0.join("krypper/provider-profiles.json");
        save_provider_profiles(
            &path,
            &[
                compatible_profile("alpha", "https://a.example/v1/chat/completions"),
                compatible_profile("alpha", "https://b.example/v1/chat/completions"),
            ],
        )
        .unwrap_err();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"version":1,"profiles":[],"arbitrary_headers":{}}"#,
        )
        .unwrap();
        assert!(load_provider_profiles(&path).is_err());
        assert!(validate_profile(&compatible_profile(
            "unsafe",
            "https://user:pass@example.invalid/v1/chat/completions"
        ))
        .is_err());
        assert!(validate_profile(&compatible_profile(
            "unsafe",
            "https://example.invalid/v1/chat/completions?key=secret"
        ))
        .is_err());
    }

    #[test]
    fn profile_lifecycle_adds_replaces_disables_enables_and_removes_last_secret() {
        let directory = TempDir::new();
        let profile_path = directory.0.join("krypper/provider-profiles.json");
        let credential_path = directory.0.join("krypper/credentials.json");
        let first = compatible_profile("mock", "https://first.example/v1/chat/completions");
        add_profile(&profile_path, first.clone()).unwrap();
        assert!(add_profile(&profile_path, first.clone()).is_err());
        let replaced = compatible_profile("mock", "https://second.example/v1/chat/completions");
        replace_profile(&profile_path, replaced.clone()).unwrap();
        assert_eq!(load_provider_profiles(&profile_path).unwrap(), [replaced]);
        set_profile_enabled(&profile_path, "mock", false).unwrap();
        assert!(
            selected_profile(&load_provider_profiles(&profile_path).unwrap(), "mock", "m").is_err()
        );
        set_profile_enabled(&profile_path, "mock", true).unwrap();

        let mut credentials = CredentialStore::empty();
        credentials.insert(&credential_name("mock"), "test-api-key".to_owned());
        save_credentials(&credential_path, &credentials).unwrap();
        remove_profile(&profile_path, &credential_path, "mock").unwrap();
        assert!(load_provider_profiles(&profile_path).unwrap().is_empty());
        assert!(!credential_path.exists());
    }

    #[test]
    fn profile_environment_name_is_derived_from_canonical_id() {
        assert_eq!(
            credential_name("my-provider2"),
            "KRYPPER_PROVIDER_MY_PROVIDER2_API_KEY"
        );
        assert!(validate_profile(&compatible_profile(
            "openai",
            "https://example.invalid/v1/chat/completions"
        ))
        .is_err());
    }
}
