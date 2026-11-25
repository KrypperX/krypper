//! Shared CLI route resolution over catalog metadata and per-user evidence.

use super::config::load_model_defaults;
use super::model_catalog::{cache_path_for_user_settings, load_catalog, LoadedCatalog};
use super::model_evidence::records;
use super::provider_profiles::{load_provider_profiles, selected_profile};
use super::settings::{
    effective_credential_value, load_credentials, user_settings_paths, valid_credential,
    CredentialStore, EffectiveCredentialValue, UserSettingsPaths,
};
use super::setup_probe::{
    configuration_revision_for_provider, existing_provider_configuration_revision,
};
use super::{ModelDefaults, RoutingMode};
use crate::brain::capability_evidence::{
    now_unix_secs, resolve_provider_readiness, CapabilityFreshness,
};
use crate::brain::{
    provider_catalog_matches, provider_descriptors, AdapterId, ProviderDecisionSourceRegistry,
};
use crate::routing::{
    AutomaticRoutingRequest, CapabilityAssertion, CapabilityEvidenceFreshness, CapabilityName,
    CapabilityProvenance, CapabilitySupport, DeterministicModelRouter, ModelRouter,
    RoutingCandidate, RoutingDecision, RoutingRequest, RoutingSelection, SelectionPreference,
};

pub(super) struct RouteResolution {
    pub(super) selection: RoutingSelection,
    pub(super) request: AutomaticRoutingRequest,
    pub(super) candidates: Vec<RoutingCandidate>,
}

struct CandidateBuildOptions<'a> {
    paths: &'a UserSettingsPaths,
    credentials: &'a CredentialStore,
    evidence_records: &'a [crate::brain::capability_evidence::ProviderEvidenceRecord],
    anchor_provider: &'a str,
    allow_cross_provider_data_transfer: bool,
    compatible_only: bool,
    create_revision_key: bool,
}

/// Shared per-user model selection for command and terminal frontends.
///
/// The implementation owns catalog, credential, and probe-evidence lookup.
/// Callers can load non-secret model defaults and request a route, but never
/// receive credential-store access or implement provider readiness themselves.
pub(crate) struct ModelRoutingService {
    paths: UserSettingsPaths,
}

impl ModelRoutingService {
    /// Opens the ordinary settings location for the current user.
    pub(crate) fn for_current_user() -> Result<Option<Self>, String> {
        Ok(user_settings_paths()?.map(|paths| Self { paths }))
    }

    pub(super) fn for_user_settings(paths: &UserSettingsPaths) -> Self {
        Self {
            paths: paths.clone(),
        }
    }

    /// Reads the user's non-secret model and routing preferences.
    pub(crate) fn load_user_defaults(&self) -> Result<Option<ModelDefaults>, String> {
        load_model_defaults(&self.paths.model)
    }

    /// Resolves the route shown by setup/status and future frontends.
    pub(crate) fn configured_route(
        &self,
        defaults: &ModelDefaults,
    ) -> Result<RoutingSelection, String> {
        if defaults.routing_mode == RoutingMode::Manual {
            return Ok(manual_route(defaults));
        }
        self.resolve(defaults, false, false)
            .map(|resolution| resolution.selection)
    }

    pub(crate) fn provider_readiness_for_default(
        &self,
        defaults: &ModelDefaults,
    ) -> Result<Option<crate::routing::ProviderReadiness>, String> {
        provider_readiness_for_default(&self.paths, defaults)
    }

    pub(super) fn resolve_for_task(
        &self,
        defaults: &ModelDefaults,
        compatible_only: bool,
        create_revision_key: bool,
    ) -> Result<RouteResolution, String> {
        self.resolve(defaults, compatible_only, create_revision_key)
    }

    fn resolve(
        &self,
        defaults: &ModelDefaults,
        compatible_only: bool,
        create_revision_key: bool,
    ) -> Result<RouteResolution, String> {
        resolve_automatic_route(defaults, &self.paths, compatible_only, create_revision_key)
    }
}

fn manual_route(defaults: &ModelDefaults) -> RoutingSelection {
    RoutingSelection {
        provider: Some(defaults.provider.as_deref().unwrap_or("ollama").to_owned()),
        model: defaults.model.clone(),
        reason: crate::routing::SelectionReasonCode::ManualOverride,
    }
}

fn resolve_automatic_route(
    defaults: &ModelDefaults,
    paths: &UserSettingsPaths,
    compatible_only: bool,
    create_revision_key: bool,
) -> Result<RouteResolution, String> {
    let catalog = load_catalog(Some(&cache_path_for_user_settings(&paths.model)))?;
    let credentials = load_credentials(&paths.credentials)?.unwrap_or_else(CredentialStore::empty);
    let evidence_records = records(&paths.model_evidence)?;
    let request = AutomaticRoutingRequest {
        required_capabilities: vec![CapabilityName::DecisionRequest, CapabilityName::ToolCalling],
        minimum_context_window_tokens: Some(1),
        minimum_output_tokens: Some(1),
        maximum_estimated_cost_micro_usd: defaults.fallback_budget_micro_usd,
        preferences: vec![SelectionPreference::LowestLatency],
    };
    let candidates = build_candidates(
        &catalog,
        CandidateBuildOptions {
            paths,
            credentials: &credentials,
            evidence_records: &evidence_records,
            anchor_provider: defaults.provider.as_deref().unwrap_or("ollama"),
            allow_cross_provider_data_transfer: defaults.allow_cross_provider_data_transfer,
            compatible_only,
            create_revision_key,
        },
    )?;
    let decision =
        DeterministicModelRouter.route(&RoutingRequest::Automatic(request.clone()), &candidates);
    let selection = match decision {
        RoutingDecision::Selected(selection) => selection,
        RoutingDecision::NoCandidate(reason) => {
            return Err(format_no_candidate(reason, &catalog));
        }
    };
    Ok(RouteResolution {
        selection,
        request,
        candidates,
    })
}

fn provider_readiness_for_default(
    paths: &UserSettingsPaths,
    defaults: &ModelDefaults,
) -> Result<Option<crate::routing::ProviderReadiness>, String> {
    let provider = defaults.provider.as_deref().unwrap_or("ollama");
    let credentials = load_credentials(&paths.credentials)?.unwrap_or_else(CredentialStore::empty);
    let Some(descriptor) = crate::brain::provider_descriptor_by_selector(provider) else {
        let profiles = load_provider_profiles(&paths.provider_profiles)?;
        let Some(profile) = profiles.iter().find(|profile| profile.id == provider) else {
            return Ok(None);
        };
        let selected = selected_profile(std::slice::from_ref(profile), provider, &defaults.model);
        let mut configured = selected.is_ok();
        if let Ok((_, _, Some(name))) = selected {
            configured &=
                match effective_credential_value(std::env::var(&name), credentials.get(&name)) {
                    EffectiveCredentialValue::Configured(value) => valid_credential(&value),
                    EffectiveCredentialValue::Missing
                    | EffectiveCredentialValue::InvalidEnvironment => false,
                };
        }
        let adapter_id = profile.protocol.decision_protocol().adapter_id();
        return Ok(Some(resolve_provider_readiness(
            true,
            ProviderDecisionSourceRegistry::with_builtins().contains(adapter_id),
            configured,
            CapabilityFreshness::Missing,
            None,
        )));
    };
    let (revision, configured) = existing_provider_configuration_revision(
        paths,
        *descriptor,
        &defaults.model,
        &credentials,
    )?;
    let store_records = records(&paths.model_evidence)?;
    let evidence = revision.as_ref().and_then(|revision| {
        store_records.iter().rev().find(|record| {
            record.provider == provider
                && record.model == defaults.model
                && &record.configuration_revision == revision
        })
    });
    let freshness = evidence.map_or(CapabilityFreshness::Missing, |record| match now_unix_secs()
        .checked_sub(record.observed_at_unix_secs)
    {
        Some(age) if age <= 60 * 60 => CapabilityFreshness::Fresh,
        _ => CapabilityFreshness::Stale,
    });
    let executable =
        ProviderDecisionSourceRegistry::with_builtins().contains(descriptor.adapter_id);
    Ok(Some(resolve_provider_readiness(
        true,
        executable,
        configured,
        freshness,
        evidence.map(|record| record.health),
    )))
}

fn build_candidates(
    catalog: &LoadedCatalog,
    options: CandidateBuildOptions<'_>,
) -> Result<Vec<RoutingCandidate>, String> {
    let CandidateBuildOptions {
        paths,
        credentials,
        evidence_records,
        anchor_provider,
        allow_cross_provider_data_transfer,
        compatible_only,
        create_revision_key,
    } = options;
    let now = now_unix_secs();
    let adapter_registry = ProviderDecisionSourceRegistry::with_builtins();
    let mut candidates = Vec::new();
    for entry in &catalog.view.models {
        let Some(descriptor) = provider_descriptors()
            .iter()
            .find(|provider| provider_catalog_matches(provider.selector, &entry.model.provider))
            .copied()
        else {
            continue;
        };
        if compatible_only && descriptor.adapter_id != AdapterId::OPENAI_COMPATIBLE {
            continue;
        }
        if !allow_cross_provider_data_transfer && descriptor.selector != anchor_provider {
            continue;
        }
        let (revision, configured) = if create_revision_key {
            let (revision, configured) = configuration_revision_for_provider(
                paths,
                descriptor,
                &entry.model.id,
                credentials,
            )?;
            (Some(revision), configured)
        } else {
            existing_provider_configuration_revision(
                paths,
                descriptor,
                &entry.model.id,
                credentials,
            )?
        };
        let Some(revision) = revision else {
            continue;
        };
        let Some(evidence) = evidence_records.iter().rev().find(|record| {
            record.provider == descriptor.selector
                && record.model == entry.model.id
                && record.configuration_revision == revision
        }) else {
            continue;
        };
        let freshness = match now.checked_sub(evidence.observed_at_unix_secs) {
            Some(age) if age <= 60 * 60 => CapabilityFreshness::Fresh,
            _ => CapabilityFreshness::Stale,
        };
        let readiness = resolve_provider_readiness(
            true,
            adapter_registry.contains(descriptor.adapter_id),
            configured,
            freshness,
            Some(evidence.health),
        );
        let decision_request = evidence
            .capabilities
            .iter()
            .rev()
            .find(|fact| fact.name == CapabilityName::DecisionRequest)
            .map_or(
                CapabilityAssertion::new(
                    CapabilityName::DecisionRequest,
                    CapabilitySupport::Unknown,
                    CapabilityProvenance::Observed,
                ),
                |fact| {
                    CapabilityAssertion::new(
                        CapabilityName::DecisionRequest,
                        fact.support,
                        fact.provenance,
                    )
                },
            );
        let tool_calling = CapabilityAssertion::new(
            CapabilityName::ToolCalling,
            match entry.model.tool_calling {
                Some(true) => CapabilitySupport::Supported,
                Some(false) => CapabilitySupport::Unsupported,
                None => CapabilitySupport::Unknown,
            },
            CapabilityProvenance::Declared,
        );
        candidates.push(RoutingCandidate {
            provider: descriptor.selector.to_owned(),
            model: entry.model.clone(),
            readiness,
            capabilities: vec![decision_request, tool_calling],
            capability_evidence_freshness: if catalog.stale {
                CapabilityEvidenceFreshness::Stale
            } else {
                CapabilityEvidenceFreshness::Fresh
            },
            estimated_cost_micro_usd: conservative_request_cost(entry),
            observed_latency_ms: (freshness == CapabilityFreshness::Fresh)
                .then_some(evidence.latency_ms),
            quality_score_basis_points: None,
        });
    }
    candidates.sort_by(|left, right| {
        (&left.provider, &left.model.id).cmp(&(&right.provider, &right.model.id))
    });
    Ok(candidates)
}

fn conservative_request_cost(entry: &crate::catalog_sources::CatalogModel) -> Option<u64> {
    let pricing = entry.pricing.as_ref()?;
    let input = entry.model.context_window?;
    let output = entry.model.max_output_tokens?;
    let input_rate = pricing.input_usd_per_million_tokens.as_ref()?.as_f64()?;
    let output_rate = pricing.output_usd_per_million_tokens.as_ref()?.as_f64()?;
    let micro_usd = input_rate * input as f64 + output_rate * output as f64;
    (micro_usd.is_finite() && micro_usd >= 0.0 && micro_usd <= u64::MAX as f64)
        .then(|| micro_usd.ceil() as u64)
}

fn format_no_candidate(reason: crate::routing::NoCandidate, catalog: &LoadedCatalog) -> String {
    let details = if reason.rejected.is_empty() {
        "no model has a current successful provider probe for this configuration".to_owned()
    } else {
        reason
            .rejected
            .iter()
            .take(5)
            .map(|candidate| {
                format!(
                    "{}:{} ({:?})",
                    candidate.provider, candidate.model, candidate.reasons
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "automatic routing found no eligible model: {details}; catalog {}{}; run `krypper setup` and accept its connection-test prompt for the model you want Krypper to verify",
        match catalog.origin {
            super::model_catalog::CatalogOrigin::Embedded => "embedded",
            super::model_catalog::CatalogOrigin::RefreshedCache => "refreshed",
        },
        if catalog.stale { " (stale)" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::capability_evidence::{
        now_unix_secs, EvidenceHealth, ProviderEvidenceObservation, ProviderEvidenceRecord,
    };
    use crate::routing::{
        CapabilitySupport, CapabilityVerification, NoCandidate, NoCandidateReasonCode,
        RoutingDecision, RoutingRequest,
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

    fn openai_entry(catalog: &LoadedCatalog) -> &crate::catalog_sources::CatalogModel {
        catalog
            .view
            .models
            .iter()
            .find(|entry| {
                entry.model.provider == "openai" && entry.model.tool_calling == Some(true)
            })
            .expect("embedded catalog has an OpenAI tool-calling model")
    }

    fn test_credentials() -> CredentialStore {
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", "fixture-token-not-sent".to_owned());
        credentials
    }

    fn probe_record(
        paths: &UserSettingsPaths,
        model: &str,
        health: EvidenceHealth,
        credentials: &CredentialStore,
    ) -> ProviderEvidenceRecord {
        let provider = crate::brain::provider_descriptor_by_selector("openai")
            .copied()
            .expect("OpenAI provider descriptor");
        let (revision, configured) =
            configuration_revision_for_provider(paths, provider, model, credentials).unwrap();
        assert!(configured);
        let mut record = ProviderEvidenceRecord::new(
            "openai",
            model,
            &revision,
            ProviderEvidenceObservation {
                observed_at_unix_secs: now_unix_secs(),
                health,
                latency_ms: 12,
                rate_limit_status: None,
                retry_after_secs: None,
                failure_code: None,
                usage: None,
            },
        )
        .unwrap();
        record
            .add_capability_fact(
                CapabilityName::DecisionRequest,
                Some(health == EvidenceHealth::Healthy),
                if health == EvidenceHealth::Healthy {
                    CapabilityProvenance::Verified
                } else {
                    CapabilityProvenance::Observed
                },
            )
            .unwrap();
        record
    }

    fn candidate_options<'a>(
        paths: &'a UserSettingsPaths,
        credentials: &'a CredentialStore,
        evidence_records: &'a [ProviderEvidenceRecord],
    ) -> CandidateBuildOptions<'a> {
        CandidateBuildOptions {
            paths,
            credentials,
            evidence_records,
            anchor_provider: "openai",
            allow_cross_provider_data_transfer: false,
            compatible_only: false,
            create_revision_key: false,
        }
    }

    #[test]
    fn automatic_route_error_points_to_the_interactive_probe_flow() {
        let catalog = load_catalog(None).unwrap();
        let message = format_no_candidate(
            NoCandidate {
                reason: NoCandidateReasonCode::AllCandidatesRejected,
                rejected: Vec::new(),
            },
            &catalog,
        );
        assert!(message.contains("run `krypper setup` and accept its connection-test prompt"));
        assert!(!message.contains("setup --test"));
    }

    #[test]
    fn automatic_route_requires_fresh_successful_probe_for_the_exact_model() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        let mut catalog = load_catalog(None).unwrap();
        catalog.stale = false;
        let entry = openai_entry(&catalog);
        let model = entry.model.id.clone();
        let credentials = test_credentials();
        let successful_probe = probe_record(&paths, &model, EvidenceHealth::Healthy, &credentials);
        let request = AutomaticRoutingRequest {
            required_capabilities: vec![
                CapabilityName::DecisionRequest,
                CapabilityName::ToolCalling,
            ],
            minimum_context_window_tokens: Some(1),
            minimum_output_tokens: Some(1),
            maximum_estimated_cost_micro_usd: None,
            preferences: vec![SelectionPreference::LowestLatency],
        };

        let missing_probe =
            build_candidates(&catalog, candidate_options(&paths, &credentials, &[])).unwrap();
        assert!(missing_probe.is_empty());

        let candidates = build_candidates(
            &catalog,
            candidate_options(&paths, &credentials, &[successful_probe]),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].model.id, model);
        assert_eq!(
            candidates[0].capabilities[0].support,
            CapabilitySupport::Supported
        );
        assert_eq!(
            candidates[0].capabilities[0].provenance,
            CapabilityProvenance::Verified
        );
        assert!(matches!(
            DeterministicModelRouter.route(&RoutingRequest::Automatic(request), &candidates),
            RoutingDecision::Selected(_)
        ));
    }

    #[test]
    fn failed_probe_is_not_an_eligible_automatic_provider() {
        let directory = TempDir::new();
        let paths = paths(&directory);
        let mut catalog = load_catalog(None).unwrap();
        catalog.stale = false;
        let model = openai_entry(&catalog).model.id.clone();
        let credentials = test_credentials();
        let failed_probe = probe_record(&paths, &model, EvidenceHealth::Unhealthy, &credentials);
        let candidates = build_candidates(
            &catalog,
            candidate_options(&paths, &credentials, &[failed_probe]),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].readiness.verification,
            CapabilityVerification::Failed
        );
        assert!(matches!(
            DeterministicModelRouter.route(
                &RoutingRequest::Automatic(AutomaticRoutingRequest {
                    required_capabilities: vec![CapabilityName::DecisionRequest],
                    ..AutomaticRoutingRequest::default()
                }),
                &candidates,
            ),
            RoutingDecision::NoCandidate(_)
        ));
    }
}
