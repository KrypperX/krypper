//! Explicit live provider probes and their bounded local evidence.

use super::model_catalog::{cache_path_for_user_settings, load_catalog};
use super::model_evidence::{configuration_revision_for_probe, record_probe};
use super::settings::{
    effective_credential_value, valid_credential, CredentialStore, EffectiveCredentialValue,
    UserSettingsPaths,
};
use crate::agent_loop::DecisionFailureCode;
use crate::brain::capability_evidence::{
    assess_required_capability, now_unix_secs, resolve_provider_readiness, CapabilityFreshness,
    EvidenceHealth, ProviderEvidenceObservation, ProviderEvidenceRecord, RequiredCapabilityResult,
};
use crate::brain::{
    provider_descriptor_by_selector, AnthropicWorkspaceId, AuthFieldRole, CloudflareAccountId,
    CloudflareGatewayId, DecisionProtocol, ModelTokenUsage, ProviderDecisionSourceRegistry,
    ProviderDescriptor,
};
use crate::routing::{CapabilityName, CapabilityProvenance, CapabilityVerification};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

pub(super) fn test_connection(
    provider: ProviderDescriptor,
    model: &str,
    paths: &UserSettingsPaths,
    credentials: &CredentialStore,
    output: &mut dyn Write,
) -> io::Result<()> {
    let config_values = provider_config_values(provider, credentials);
    let configured = provider_is_configured(provider, &config_values);
    let discoverable = provider_descriptor_by_selector(provider.selector).is_some();
    let executable = ProviderDecisionSourceRegistry::with_builtins().contains(provider.adapter_id);
    let initial_readiness = resolve_provider_readiness(
        discoverable,
        executable,
        configured,
        CapabilityFreshness::Missing,
        None,
    );
    if !initial_readiness.discoverable
        || !initial_readiness.executable
        || !initial_readiness.configured
    {
        writeln!(
            output,
            "Provider is not configured for this model selection; no request was sent."
        )?;
        return Ok(());
    }

    // Resolve optional catalog facts before the live request. A broken local catalog must not
    // turn a successful provider probe into an error after the provider has already been called.
    let model_spec = load_catalog(Some(&cache_path_for_user_settings(&paths.model)))
        .ok()
        .and_then(|catalog| {
            catalog
                .view
                .models
                .into_iter()
                .find(|entry| {
                    crate::catalog_data::provider_matches(provider.selector, &entry.model.provider)
                        && entry.model.id == model
                })
                .map(|entry| entry.model)
        });

    let (revision, _) = configuration_revision_for_provider(paths, provider, model, credentials)
        .map_err(io::Error::other)?;

    let workspace = TempWorkspace::new()?;
    let mut arguments = vec![
        OsString::from("--goal"),
        OsString::from("Reply with a short confirmation and finish immediately."),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--provider"),
        OsString::from(provider.selector),
        OsString::from("--model"),
        OsString::from(model),
        OsString::from("--workspace"),
        workspace.0.as_os_str().to_owned(),
    ];
    if provider.protocol == DecisionProtocol::CompatibleChat {
        arguments.push(OsString::from("--model-usage-jsonl"));
    }
    let mut input = io::empty();
    let mut test_output = Vec::new();
    let mut test_error = Vec::new();
    let started = Instant::now();
    let result = super::task::run_task(&arguments, &mut input, &mut test_output, &mut test_error);
    let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let succeeded = result.as_ref().is_ok_and(|code| *code == 0);
    let (measured_latency, usage) = usage_from_probe_output(&test_error);
    let failure_code = if succeeded {
        None
    } else {
        failure_code_from_probe_output(&test_error)
            .or_else(|| failure_code_from_probe_output(&test_output))
            .or(Some(DecisionFailureCode::Unknown))
    };
    let health = if succeeded {
        EvidenceHealth::Healthy
    } else {
        EvidenceHealth::Unhealthy
    };
    let mut record = ProviderEvidenceRecord::new(
        provider.selector,
        model,
        &revision,
        ProviderEvidenceObservation {
            observed_at_unix_secs: now_unix_secs(),
            health,
            latency_ms: measured_latency.unwrap_or(elapsed_ms),
            rate_limit_status: (failure_code == Some(DecisionFailureCode::ProviderRateLimited))
                .then_some(429),
            retry_after_secs: None,
            failure_code,
            usage,
        },
    )
    .map_err(io::Error::other)?;
    record
        .add_capability_fact(
            CapabilityName::NativeStreaming,
            Some(provider.native_sse_decisions),
            CapabilityProvenance::Declared,
        )
        .map_err(io::Error::other)?;
    record
        .add_capability_fact(
            CapabilityName::ToolCalling,
            model_spec.as_ref().and_then(|model| model.tool_calling),
            CapabilityProvenance::Declared,
        )
        .map_err(io::Error::other)?;
    record
        .add_capability_fact(
            CapabilityName::Reasoning,
            model_spec.as_ref().and_then(|model| model.reasoning),
            CapabilityProvenance::Declared,
        )
        .map_err(io::Error::other)?;
    record
        .add_capability_fact(
            CapabilityName::ImageInput,
            model_spec.as_ref().and_then(model_declares_image_input),
            CapabilityProvenance::Declared,
        )
        .map_err(io::Error::other)?;
    record
        .add_capability_fact(
            CapabilityName::DecisionRequest,
            succeeded.then_some(true),
            if succeeded {
                CapabilityProvenance::Verified
            } else {
                CapabilityProvenance::Observed
            },
        )
        .map_err(io::Error::other)?;
    let decision_verified = assess_required_capability(true, record.decision_request_fact())
        == RequiredCapabilityResult::Supported;
    let persisted = record_probe(&paths.model_evidence, record);
    let readiness = persisted.as_ref().ok().map(|freshness| {
        resolve_provider_readiness(
            discoverable,
            executable,
            configured,
            *freshness,
            Some(health),
        )
    });

    match result {
        Ok(0) => {
            if readiness.is_some_and(|state| {
                state.verification == CapabilityVerification::Verified && decision_verified
            }) {
                writeln!(
                    output,
                    "The provider test request completed; this model's decision request was verified and redacted runtime evidence was saved."
                )
            } else {
                writeln!(
                    output,
                    "The provider test request completed; runtime evidence could not be saved."
                )
            }
        }
        _ => {
            if readiness.is_some() {
                writeln!(output, "The provider test did not complete; redacted runtime evidence was saved. Settings remain saved; no credential value was displayed.")
            } else {
                writeln!(output, "The provider test did not complete. Settings remain saved; no credential value was displayed.")
            }
        }
    }
}

fn model_declares_image_input(model: &crate::catalog_data::ModelSpec) -> Option<bool> {
    let input = model.modalities.as_ref()?.get("input")?.as_array()?;
    Some(
        input
            .iter()
            .any(|modality| modality.as_str() == Some("image")),
    )
}

pub(super) fn configuration_revision_for_provider(
    paths: &UserSettingsPaths,
    provider: ProviderDescriptor,
    model: &str,
    credentials: &CredentialStore,
) -> Result<(String, bool), String> {
    let values = provider_config_values(provider, credentials);
    let configured = provider_is_configured(provider, &values);
    let encoded = revision_references(&values);
    let references = encoded
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect::<Vec<_>>();
    let revision = configuration_revision_for_probe(paths, provider.selector, model, &references)?;
    Ok((revision, configured))
}

pub(super) fn existing_provider_configuration_revision(
    paths: &UserSettingsPaths,
    provider: ProviderDescriptor,
    model: &str,
    credentials: &CredentialStore,
) -> Result<(Option<String>, bool), String> {
    let values = provider_config_values(provider, credentials);
    let configured = provider_is_configured(provider, &values);
    let encoded = revision_references(&values);
    let references = encoded
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect::<Vec<_>>();
    let revision = super::model_evidence::existing_configuration_revision_for_probe(
        &paths.model_evidence_key,
        provider.selector,
        model,
        &references,
    )?;
    Ok((revision, configured))
}

fn revision_references(values: &[(&'static str, Option<String>)]) -> Vec<(&'static str, String)> {
    values
        .iter()
        .map(|(name, value)| {
            (
                *name,
                value
                    .as_deref()
                    .map(|value| format!("set:{value}"))
                    .unwrap_or_else(|| "unset:".to_owned()),
            )
        })
        .collect()
}

fn provider_config_values(
    provider: ProviderDescriptor,
    credentials: &CredentialStore,
) -> Vec<(&'static str, Option<String>)> {
    let mut values = vec![
        ("adapter", Some(provider.adapter_id.as_str().to_owned())),
        ("endpoint_policy", Some(format!("{:?}", provider.endpoint))),
        (
            "native_sse_decisions",
            Some(provider.native_sse_decisions.to_string()),
        ),
    ];
    values.extend(provider.auth_fields.iter().map(|field| {
        let value = match effective_credential_value(
            std::env::var(field.name),
            credentials.get(field.name),
        ) {
            EffectiveCredentialValue::Configured(value) => Some(value),
            EffectiveCredentialValue::Missing => None,
            EffectiveCredentialValue::InvalidEnvironment => {
                Some("\0invalid-non-utf8-environment-value".to_owned())
            }
        };
        (field.name, value)
    }));
    values
}

pub(super) fn provider_is_configured(
    provider: ProviderDescriptor,
    values: &[(&'static str, Option<String>)],
) -> bool {
    provider.auth_fields.iter().all(|field| {
        let value = values
            .iter()
            .find(|(name, _)| *name == field.name)
            .and_then(|(_, value)| value.as_deref());
        let Some(value) = value else {
            return !field.required;
        };
        if !valid_credential(value) {
            return false;
        }
        match field.role {
            AuthFieldRole::ApiKey => field.secret,
            AuthFieldRole::CloudflareAccountId => CloudflareAccountId::try_from(value).is_ok(),
            AuthFieldRole::CloudflareGatewayId => CloudflareGatewayId::try_from(value).is_ok(),
            AuthFieldRole::AnthropicWorkspaceId => AnthropicWorkspaceId::try_from(value).is_ok(),
        }
    })
}

fn usage_from_probe_output(bytes: &[u8]) -> (Option<u64>, Option<ModelTokenUsage>) {
    let mut latency = None;
    let mut usage = None;
    for line in String::from_utf8_lossy(bytes).lines() {
        let Some(json) = line.strip_prefix("KRYPPER_MODEL_USAGE ") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        latency = event.get("elapsed_ms").and_then(serde_json::Value::as_u64);
        usage = Some(ModelTokenUsage {
            prompt_tokens: event
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64),
            completion_tokens: event
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64),
            total_tokens: event
                .get("total_tokens")
                .and_then(serde_json::Value::as_u64),
        });
    }
    (latency, usage)
}

fn failure_code_from_probe_output(bytes: &[u8]) -> Option<DecisionFailureCode> {
    let diagnostics = String::from_utf8_lossy(bytes);
    [
        DecisionFailureCode::ProviderRateLimited,
        DecisionFailureCode::ProviderUnavailable,
        DecisionFailureCode::ProviderRejected,
        DecisionFailureCode::InvalidEndpoint,
        DecisionFailureCode::InvalidProviderResponse,
        DecisionFailureCode::InvalidModelOutput,
    ]
    .into_iter()
    .find(|code| diagnostics.contains(code.as_code()))
}

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> io::Result<Self> {
        for attempt in 0..16_u8 {
            let path = std::env::temp_dir().join(format!(
                "krypper-setup-probe-{}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(cause) => return Err(cause),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temporary provider test workspace",
        ))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::provider_descriptors;
    use std::env::VarError;

    #[test]
    fn explicit_probe_usage_parser_keeps_only_latency_and_token_counts() {
        let diagnostics = br#"model decision 1/1
KRYPPER_MODEL_USAGE {"type":"model_token_usage","decision":1,"elapsed_ms":321,"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}
"#;
        let (latency, usage) = usage_from_probe_output(diagnostics);
        assert_eq!(latency, Some(321));
        assert_eq!(
            usage,
            Some(ModelTokenUsage {
                prompt_tokens: Some(12),
                completion_tokens: Some(7),
                total_tokens: Some(19),
            })
        );
    }

    #[test]
    fn probe_failure_parser_returns_only_known_typed_codes() {
        assert_eq!(
            failure_code_from_probe_output(b"provider_rate_limited"),
            Some(DecisionFailureCode::ProviderRateLimited)
        );
        assert_eq!(
            failure_code_from_probe_output(b"raw provider error with secret-token"),
            None
        );
    }

    #[test]
    fn missing_credentials_block_live_probe_before_any_request() {
        let provider = provider_descriptors()
            .iter()
            .find(|provider| provider.selector == "openai")
            .copied()
            .unwrap();
        assert!(!provider_is_configured(
            provider,
            &[("OPENAI_API_KEY", None)]
        ));
        assert!(provider_is_configured(
            provider,
            &[("OPENAI_API_KEY", Some("present".to_owned()))]
        ));

        let cloudflare = provider_descriptors()
            .iter()
            .find(|provider| provider.selector == "cloudflare")
            .copied()
            .unwrap();
        assert!(!provider_is_configured(
            cloudflare,
            &[
                ("CLOUDFLARE_ACCOUNT_ID", Some("invalid/id".to_owned())),
                ("CLOUDFLARE_API_TOKEN", Some("present".to_owned())),
            ]
        ));
    }

    #[test]
    fn optional_anthropic_workspace_id_is_accepted_only_when_valid() {
        let anthropic = provider_descriptors()
            .iter()
            .find(|provider| provider.selector == "anthropic")
            .copied()
            .unwrap();
        assert!(provider_is_configured(
            anthropic,
            &[("ANTHROPIC_API_KEY", Some("present".to_owned()))]
        ));
        assert!(provider_is_configured(
            anthropic,
            &[
                ("ANTHROPIC_API_KEY", Some("present".to_owned())),
                ("ANTHROPIC_WORKSPACE_ID", Some("wrkspc_valid-01".to_owned())),
            ]
        ));
        assert!(!provider_is_configured(
            anthropic,
            &[
                ("ANTHROPIC_API_KEY", Some("present".to_owned())),
                (
                    "ANTHROPIC_WORKSPACE_ID",
                    Some("invalid/workspace".to_owned())
                ),
            ]
        ));
    }

    #[test]
    fn non_utf8_environment_values_do_not_fall_back_to_saved_credentials() {
        let value = match effective_credential_value(
            Err(VarError::NotUnicode(OsString::from("invalid"))),
            Some("saved-api-key"),
        ) {
            EffectiveCredentialValue::Configured(value) => Some(value),
            EffectiveCredentialValue::Missing => None,
            EffectiveCredentialValue::InvalidEnvironment => {
                Some("\0invalid-non-utf8-environment-value".to_owned())
            }
        };
        assert!(value
            .as_deref()
            .is_some_and(|value| value.contains("invalid-non-utf8")));
        assert!(!provider_is_configured(
            provider_descriptors()
                .iter()
                .find(|provider| provider.selector == "openai")
                .copied()
                .unwrap(),
            &[("OPENAI_API_KEY", value)]
        ));
    }

    #[test]
    fn missing_catalog_modalities_leave_image_support_unknown() {
        let model = crate::catalog_data::ModelSpec {
            provider: "test".to_owned(),
            id: "model".to_owned(),
            name: "model".to_owned(),
            context_window: None,
            max_output_tokens: None,
            tool_calling: None,
            reasoning: None,
            status: None,
            modalities: None,
        };
        assert_eq!(model_declares_image_input(&model), None);
    }
}
