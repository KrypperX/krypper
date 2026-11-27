//! Provider capability facts and bounded, secret-free runtime evidence.
//!
//! Catalog and descriptor metadata is declarative. A successful explicit probe
//! verifies only that a decision request completed for the selected account,
//! model, and configuration revision; it does not prove unrelated model features.

use crate::brain::providers::ModelTokenUsage;
use crate::contracts::goal_turn::DecisionFailureCode;
use crate::routing::{
    CapabilityAssertion, CapabilityName, CapabilityProvenance, CapabilitySupport,
    CapabilityVerification, ProviderReadiness,
};
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(crate) const MAX_EVIDENCE_RECORDS: usize = 256;
pub(crate) const MAX_EVIDENCE_PER_REVISION: usize = 8;
pub(crate) const EVIDENCE_FRESHNESS_SECS: u64 = 60 * 60;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_LATENCY_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_TOKEN_COUNT: u64 = 1_000_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CapabilityFact {
    pub(crate) support: CapabilitySupport,
    pub(crate) provenance: CapabilityProvenance,
}

impl CapabilityFact {
    fn verified_decision_request(healthy: bool) -> Self {
        capability_fact(
            healthy.then_some(true),
            if healthy {
                CapabilityProvenance::Verified
            } else {
                CapabilityProvenance::Observed
            },
        )
    }
}

pub(crate) fn resolve_provider_readiness(
    discoverable: bool,
    executable: bool,
    configured: bool,
    freshness: CapabilityFreshness,
    health: Option<EvidenceHealth>,
) -> ProviderReadiness {
    let verification = match (freshness, health) {
        (CapabilityFreshness::Fresh, Some(EvidenceHealth::Healthy)) => {
            CapabilityVerification::Verified
        }
        (CapabilityFreshness::Fresh, Some(EvidenceHealth::Unhealthy)) => {
            CapabilityVerification::Failed
        }
        (CapabilityFreshness::Stale, _) => CapabilityVerification::Stale,
        _ => CapabilityVerification::Unknown,
    };
    ProviderReadiness {
        discoverable,
        executable,
        configured,
        verification,
    }
}

impl From<(CapabilityName, CapabilityFact)> for CapabilityAssertion {
    fn from((name, fact): (CapabilityName, CapabilityFact)) -> Self {
        Self {
            name,
            support: fact.support,
            provenance: fact.provenance,
        }
    }
}

pub(crate) fn capability_fact(
    value: Option<bool>,
    provenance: CapabilityProvenance,
) -> CapabilityFact {
    let support = match value {
        Some(true) => CapabilitySupport::Supported,
        Some(false) => CapabilitySupport::Unsupported,
        None => CapabilitySupport::Unknown,
    };
    CapabilityFact {
        support,
        provenance,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequiredCapabilityResult {
    Supported,
    Unsupported,
    Unknown,
}

pub(crate) fn assess_required_capability(
    declared_protocol_matches: bool,
    feature: CapabilityFact,
) -> RequiredCapabilityResult {
    if !declared_protocol_matches || feature.support == CapabilitySupport::Unsupported {
        return RequiredCapabilityResult::Unsupported;
    }
    match feature.support {
        CapabilitySupport::Supported => RequiredCapabilityResult::Supported,
        CapabilitySupport::Unsupported => RequiredCapabilityResult::Unsupported,
        CapabilitySupport::Unknown => RequiredCapabilityResult::Unknown,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceHealth {
    Healthy,
    Unhealthy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityFreshness {
    Missing,
    Fresh,
    Stale,
}

/// Token counts contain no model content and preserve missing values as unknown.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenUsageEvidence {
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) total_tokens: Option<u64>,
}

impl From<ModelTokenUsage> for TokenUsageEvidence {
    fn from(usage: ModelTokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

/// An allowlisted runtime observation. Prompt, response, headers, and credentials
/// have no representation in this type and therefore cannot enter the evidence file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderEvidenceRecord {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Opaque keyed fingerprint of the effective configuration, including credentials.
    pub(crate) configuration_revision: String,
    pub(crate) observed_at_unix_secs: u64,
    pub(crate) health: EvidenceHealth,
    pub(crate) latency_ms: u64,
    /// HTTP 429 only; no response text or headers are retained.
    pub(crate) rate_limit_status: Option<u16>,
    pub(crate) retry_after_secs: Option<u64>,
    /// Stable, secret-free classification from Krypper's existing typed error vocabulary.
    pub(crate) failure_code: Option<String>,
    pub(crate) usage: Option<TokenUsageEvidence>,
    #[serde(default)]
    pub(crate) capabilities: Vec<CapabilityAssertion>,
}

#[derive(Clone, Copy)]
pub(crate) struct ProviderEvidenceObservation {
    pub(crate) observed_at_unix_secs: u64,
    pub(crate) health: EvidenceHealth,
    pub(crate) latency_ms: u64,
    pub(crate) rate_limit_status: Option<u16>,
    pub(crate) retry_after_secs: Option<u64>,
    pub(crate) failure_code: Option<DecisionFailureCode>,
    pub(crate) usage: Option<ModelTokenUsage>,
}

impl ProviderEvidenceRecord {
    pub(crate) fn decision_request_fact(&self) -> CapabilityFact {
        self.capabilities
            .iter()
            .rev()
            .find(|fact| fact.name == CapabilityName::DecisionRequest)
            .map_or_else(
                || CapabilityFact::verified_decision_request(false),
                |fact| CapabilityFact {
                    support: fact.support,
                    provenance: fact.provenance,
                },
            )
    }

    pub(crate) fn add_capability_fact(
        &mut self,
        name: CapabilityName,
        value: Option<bool>,
        provenance: CapabilityProvenance,
    ) -> Result<(), &'static str> {
        if self.capabilities.len() >= 16 {
            return Err("too many provider capability facts");
        }
        if self.capabilities.iter().any(|fact| fact.name == name) {
            return Err("duplicate provider capability fact");
        }
        self.capabilities
            .push((name, capability_fact(value, provenance)).into());
        Ok(())
    }

    pub(crate) fn new(
        provider: &str,
        model: &str,
        configuration_revision: &str,
        observation: ProviderEvidenceObservation,
    ) -> Result<Self, &'static str> {
        let ProviderEvidenceObservation {
            observed_at_unix_secs,
            health,
            latency_ms,
            rate_limit_status,
            retry_after_secs,
            failure_code,
            usage,
        } = observation;
        let record = Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            configuration_revision: configuration_revision.to_owned(),
            observed_at_unix_secs,
            health,
            latency_ms: latency_ms.min(MAX_LATENCY_MS),
            rate_limit_status: (rate_limit_status == Some(429)).then_some(429),
            retry_after_secs: retry_after_secs.filter(|seconds| *seconds <= 86_400),
            failure_code: failure_code
                .map(DecisionFailureCode::as_code)
                .map(str::to_owned),
            usage: usage.map(TokenUsageEvidence::from),
            capabilities: Vec::new(),
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if !valid_identifier(&self.provider)
            || !valid_identifier(&self.model)
            || !valid_revision(&self.configuration_revision)
            || self.latency_ms > MAX_LATENCY_MS
            || self.rate_limit_status.is_some_and(|status| status != 429)
            || self
                .retry_after_secs
                .is_some_and(|seconds| seconds > 86_400)
            || self.capabilities.len() > 16
        {
            return Err("invalid provider evidence record");
        }
        if self
            .failure_code
            .as_deref()
            .is_some_and(|code| DecisionFailureCode::from_code(code).is_none())
        {
            return Err("invalid provider evidence failure code");
        }
        if self.usage.is_some_and(|usage| {
            [
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
            ]
            .into_iter()
            .flatten()
            .any(|count| count > MAX_TOKEN_COUNT)
        }) {
            return Err("invalid provider evidence token usage");
        }
        if self.capabilities.iter().enumerate().any(|(index, fact)| {
            self.capabilities[..index]
                .iter()
                .any(|previous| previous.name == fact.name)
        }) {
            return Err("duplicate provider evidence capability fact");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderEvidenceStore {
    version: u32,
    records: Vec<ProviderEvidenceRecord>,
}

impl ProviderEvidenceStore {
    pub(crate) fn new() -> Self {
        Self {
            version: 1,
            records: Vec::new(),
        }
    }

    pub(crate) fn records(&self) -> &[ProviderEvidenceRecord] {
        &self.records
    }

    pub(crate) fn record(&mut self, record: ProviderEvidenceRecord) -> Result<(), &'static str> {
        record.validate()?;
        if self.version != 1 {
            return Err("unsupported provider evidence version");
        }
        self.records.push(record);
        let latest = self
            .records
            .last()
            .expect("record was just appended")
            .clone();
        let matching = self
            .records
            .iter()
            .filter(|candidate| same_revision_key(candidate, &latest))
            .count();
        if matching > MAX_EVIDENCE_PER_REVISION {
            let excess = matching - MAX_EVIDENCE_PER_REVISION;
            let mut removed = 0;
            self.records.retain(|candidate| {
                if removed < excess && same_revision_key(candidate, &latest) {
                    removed += 1;
                    false
                } else {
                    true
                }
            });
        }
        if self.records.len() > MAX_EVIDENCE_RECORDS {
            let excess = self.records.len() - MAX_EVIDENCE_RECORDS;
            self.records.drain(..excess);
        }
        Ok(())
    }

    pub(crate) fn latest(
        &self,
        provider: &str,
        model: &str,
        configuration_revision: &str,
    ) -> Option<&ProviderEvidenceRecord> {
        self.records.iter().rev().find(|record| {
            record.provider == provider
                && record.model == model
                && record.configuration_revision == configuration_revision
        })
    }

    pub(crate) fn freshness(
        &self,
        provider: &str,
        model: &str,
        configuration_revision: &str,
        now_unix_secs: u64,
    ) -> CapabilityFreshness {
        let Some(record) = self.latest(provider, model, configuration_revision) else {
            return CapabilityFreshness::Missing;
        };
        match now_unix_secs.checked_sub(record.observed_at_unix_secs) {
            Some(age) if age <= EVIDENCE_FRESHNESS_SECS => CapabilityFreshness::Fresh,
            _ => CapabilityFreshness::Stale,
        }
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let store: Self = serde_json::from_slice(bytes)?;
        if store.version != 1
            || store.records.len() > MAX_EVIDENCE_RECORDS
            || store
                .records
                .iter()
                .any(|record| record.validate().is_err())
            || store.records.iter().any(|record| {
                store
                    .records
                    .iter()
                    .filter(|candidate| same_revision_key(record, candidate))
                    .count()
                    > MAX_EVIDENCE_PER_REVISION
            })
        {
            return Err(serde::de::Error::custom("invalid provider evidence store"));
        }
        Ok(store)
    }
}

/// Produces a stable, keyed configuration revision without storing a credential
/// or a plain hash of one. The HMAC key is generated by the per-user settings layer.
pub(crate) fn configuration_revision(
    key: &[u8; 32],
    provider: &str,
    model: &str,
    config_values: &[(&str, &str)],
) -> String {
    let mut input = Vec::new();
    append_component(&mut input, provider.as_bytes());
    append_component(&mut input, model.as_bytes());
    for (name, value) in config_values {
        append_component(&mut input, name.as_bytes());
        append_component(&mut input, value.as_bytes());
    }
    let signature = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &input);
    signature
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn new_configuration_revision_key() -> [u8; 32] {
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

pub(crate) fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn append_component(output: &mut Vec<u8>, component: &[u8]) {
    output.extend_from_slice(&(component.len() as u64).to_be_bytes());
    output.extend_from_slice(component);
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES && !value.chars().any(char::is_control)
}

fn valid_revision(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn same_revision_key(left: &ProviderEvidenceRecord, right: &ProviderEvidenceRecord) -> bool {
    left.provider == right.provider
        && left.model == right.model
        && left.configuration_revision == right.configuration_revision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(revision: &str, at: u64) -> ProviderEvidenceRecord {
        ProviderEvidenceRecord::new(
            "cloudflare",
            "@cf/test-model",
            revision,
            ProviderEvidenceObservation {
                observed_at_unix_secs: at,
                health: EvidenceHealth::Healthy,
                latency_ms: 125,
                rate_limit_status: None,
                retry_after_secs: None,
                failure_code: None,
                usage: Some(ModelTokenUsage {
                    prompt_tokens: Some(11),
                    completion_tokens: Some(7),
                    total_tokens: Some(18),
                }),
            },
        )
        .unwrap()
    }

    #[test]
    fn unknown_capabilities_remain_unknown_and_protocol_mismatch_is_unsupported() {
        let unknown = capability_fact(None, CapabilityProvenance::Declared);
        assert_eq!(unknown.support, CapabilitySupport::Unknown);
        assert_eq!(
            assess_required_capability(true, unknown),
            RequiredCapabilityResult::Unknown
        );
        assert_eq!(
            assess_required_capability(
                false,
                capability_fact(Some(true), CapabilityProvenance::Declared)
            ),
            RequiredCapabilityResult::Unsupported
        );
        assert_eq!(
            assess_required_capability(
                true,
                capability_fact(Some(false), CapabilityProvenance::Declared)
            ),
            RequiredCapabilityResult::Unsupported
        );
    }

    #[test]
    fn readiness_keeps_discovery_execution_configuration_and_verification_separate() {
        let missing_credentials =
            resolve_provider_readiness(true, true, false, CapabilityFreshness::Missing, None);
        assert!(missing_credentials.discoverable);
        assert!(missing_credentials.executable);
        assert!(!missing_credentials.configured);
        assert_eq!(
            missing_credentials.verification,
            CapabilityVerification::Unknown
        );

        let verified = resolve_provider_readiness(
            true,
            true,
            true,
            CapabilityFreshness::Fresh,
            Some(EvidenceHealth::Healthy),
        );
        assert_eq!(verified.verification, CapabilityVerification::Verified);

        let stale = resolve_provider_readiness(
            true,
            true,
            true,
            CapabilityFreshness::Stale,
            Some(EvidenceHealth::Healthy),
        );
        assert_eq!(stale.verification, CapabilityVerification::Stale);
    }

    #[test]
    fn evidence_is_scoped_to_provider_model_and_configuration_revision() {
        let mut store = ProviderEvidenceStore::new();
        store.record(record(&"a".repeat(64), 10_000)).unwrap();
        assert_eq!(
            store.freshness("cloudflare", "@cf/test-model", &"a".repeat(64), 10_100),
            CapabilityFreshness::Fresh
        );
        assert_eq!(
            store.freshness("cloudflare", "@cf/test-model", &"b".repeat(64), 10_100),
            CapabilityFreshness::Missing
        );
        assert_eq!(
            store.freshness("openrouter", "@cf/test-model", &"a".repeat(64), 10_100),
            CapabilityFreshness::Missing
        );
        assert_eq!(
            store.freshness("cloudflare", "different-model", &"a".repeat(64), 10_100),
            CapabilityFreshness::Missing
        );
    }

    #[test]
    fn evidence_expires_and_future_timestamps_are_stale() {
        let mut store = ProviderEvidenceStore::new();
        store.record(record(&"a".repeat(64), 10_000)).unwrap();
        assert_eq!(
            store.freshness(
                "cloudflare",
                "@cf/test-model",
                &"a".repeat(64),
                10_000 + EVIDENCE_FRESHNESS_SECS
            ),
            CapabilityFreshness::Fresh
        );
        assert_eq!(
            store.freshness(
                "cloudflare",
                "@cf/test-model",
                &"a".repeat(64),
                10_001 + EVIDENCE_FRESHNESS_SECS
            ),
            CapabilityFreshness::Stale
        );
        assert_eq!(
            store.freshness("cloudflare", "@cf/test-model", &"a".repeat(64), 9_999),
            CapabilityFreshness::Stale
        );
    }

    #[test]
    fn serialized_store_rejects_more_than_the_per_revision_limit() {
        let mut store = ProviderEvidenceStore::new();
        for at in 0..=MAX_EVIDENCE_PER_REVISION {
            store.records.push(record(&"a".repeat(64), at as u64));
        }
        let bytes = serde_json::to_vec(&store).unwrap();
        assert!(ProviderEvidenceStore::from_json(&bytes).is_err());
    }

    #[test]
    fn evidence_serializes_only_bounded_redacted_observations() {
        let mut record = ProviderEvidenceRecord::new(
            "cloudflare",
            "@cf/test-model",
            &"a".repeat(64),
            ProviderEvidenceObservation {
                observed_at_unix_secs: 10,
                health: EvidenceHealth::Unhealthy,
                latency_ms: u64::MAX,
                rate_limit_status: Some(429),
                retry_after_secs: Some(u64::MAX),
                failure_code: Some(DecisionFailureCode::ProviderRateLimited),
                usage: None,
            },
        )
        .unwrap();
        record
            .add_capability_fact(
                CapabilityName::ToolCalling,
                None,
                CapabilityProvenance::Declared,
            )
            .unwrap();
        record
            .add_capability_fact(
                CapabilityName::DecisionRequest,
                Some(true),
                CapabilityProvenance::Verified,
            )
            .unwrap();
        assert_eq!(
            record.decision_request_fact().provenance,
            CapabilityProvenance::Verified
        );
        assert_eq!(
            record.decision_request_fact().support,
            CapabilitySupport::Supported
        );
        let mut store = ProviderEvidenceStore::new();
        store.record(record).unwrap();
        let bytes = serde_json::to_vec(&store).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("provider_rate_limited"));
        assert!(text.contains("\"rate_limit_status\":429"));
        assert!(!text.contains("api_key"));
        assert!(!text.contains("prompt"));
        assert!(!text.contains("response"));
        assert_eq!(ProviderEvidenceStore::from_json(&bytes).unwrap(), store);
        assert_eq!(store.records()[0].latency_ms, MAX_LATENCY_MS);
        assert_eq!(store.records()[0].retry_after_secs, None);
    }

    #[test]
    fn per_revision_history_and_total_store_are_bounded() {
        let mut store = ProviderEvidenceStore::new();
        for at in 0..(MAX_EVIDENCE_PER_REVISION + 4) {
            store.record(record(&"a".repeat(64), at as u64)).unwrap();
        }
        assert_eq!(store.records().len(), MAX_EVIDENCE_PER_REVISION);
        assert_eq!(store.records()[0].observed_at_unix_secs, 4);
        for index in 0..MAX_EVIDENCE_RECORDS {
            let revision = format!("{:064x}", index + 1);
            store.record(record(&revision, index as u64)).unwrap();
        }
        assert_eq!(store.records().len(), MAX_EVIDENCE_RECORDS);
    }

    #[test]
    fn configuration_revision_is_opaque_and_changes_with_account_configuration() {
        let key = [7; 32];
        let first = configuration_revision(
            &key,
            "cloudflare",
            "@cf/model",
            &[("account", "acct-a"), ("token", "secret-a")],
        );
        let same = configuration_revision(
            &key,
            "cloudflare",
            "@cf/model",
            &[("account", "acct-a"), ("token", "secret-a")],
        );
        let other_account = configuration_revision(
            &key,
            "cloudflare",
            "@cf/model",
            &[("account", "acct-b"), ("token", "secret-a")],
        );
        let other_token = configuration_revision(
            &key,
            "cloudflare",
            "@cf/model",
            &[("account", "acct-a"), ("token", "secret-b")],
        );
        assert_eq!(first, same);
        assert_ne!(first, other_account);
        assert_ne!(first, other_token);
        let unset = configuration_revision(&key, "openai", "model", &[("api-key", "unset:")]);
        let empty = configuration_revision(&key, "openai", "model", &[("api-key", "set:")]);
        assert_ne!(unset, empty);
        assert!(!first.contains("secret"));
    }
}
