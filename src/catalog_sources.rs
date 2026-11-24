//! Explicit catalog source adapters and a bounded last-known-good cache.
//!
//! The embedded catalog in catalog_data remains the ordinary offline default.
//! This module performs network or cache work only when a caller invokes one of
//! its refresh/import functions.

use crate::catalog_data::ModelSpec;
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Number;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CATALOG_CACHE_SCHEMA_VERSION: u32 = 1;
pub const MAX_CATALOG_CACHE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CATALOG_SOURCE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CATALOG_CACHE_AGE_SECONDS: u64 = 30 * 24 * 60 * 60;
const MODELS_DEV_MAX_BYTES: usize = MAX_CATALOG_SOURCE_BYTES;
const CLOUDFLARE_MAX_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CatalogSourceId(String);

impl CatalogSourceId {
    pub fn new(id: impl Into<String>) -> Result<Self, CatalogError> {
        let id = id.into();
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || id.starts_with('-')
            || id.ends_with('-')
        {
            return Err(CatalogError::InvalidSourceId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn models_dev() -> Self {
        Self("models-dev".to_owned())
    }

    fn cloudflare_workers_ai() -> Self {
        Self("cloudflare-workers-ai".to_owned())
    }

    fn builtin_descriptor(&self) -> Option<CatalogSourceDescriptor> {
        match self.0.as_str() {
            "models-dev" => Some(CatalogSourceDescriptor {
                id: self.clone(),
                url: "https://models.dev/api.json",
                license: "MIT",
                license_url: "https://github.com/anomalyco/models.dev/blob/dev/LICENSE",
                schema_version: 1,
                max_bytes: MODELS_DEV_MAX_BYTES,
            }),
            "cloudflare-workers-ai" => Some(CatalogSourceDescriptor {
                id: self.clone(),
                url: "https://ai-cloudflare-com.pages.dev/api/models",
                license: "CC-BY-4.0",
                license_url:
                    "https://github.com/cloudflare/cloudflare-docs/blob/production/LICENSE",
                schema_version: 1,
                max_bytes: CLOUDFLARE_MAX_BYTES,
            }),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for CatalogSourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?;
        Self::new(id).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSourceDescriptor {
    pub id: CatalogSourceId,
    pub url: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    /// Version of this adapter's normalized contribution schema.
    pub schema_version: u32,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProvenance {
    pub source: CatalogSourceId,
    pub url: String,
    pub license: String,
    pub license_url: String,
    pub source_sha256: String,
    pub retrieved_at_unix: u64,
    pub schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderMetadata {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    /// Reference USD rate per one million input tokens. This metadata supports
    /// estimates; it does not represent an invoice rate. Missing prices remain
    /// unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_usd_per_million_tokens: Option<Number>,
    /// Reference USD rate per one million output tokens. This metadata supports
    /// estimates; it does not represent an invoice rate. Missing prices remain
    /// unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_usd_per_million_tokens: Option<Number>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFeatures {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_attachments: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_workers_paid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_deprecation_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    pub model: ModelSpec,
    /// Optional reference rates used for route estimates. Missing or stale
    /// prices must not be treated as zero or as proof of actual billed cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Optional descriptive catalog facts; runtime capability evidence is
    /// tracked separately by the provider-evidence subsystem.
    #[serde(default)]
    pub features: ModelFeatures,
    #[serde(default)]
    pub sources: Vec<CatalogSourceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogContribution {
    pub provenance: CatalogProvenance,
    #[serde(default)]
    pub providers: Vec<CatalogProviderMetadata>,
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub schema_version: u32,
    pub refreshed_at_unix: u64,
    pub contributions: Vec<CatalogContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogView {
    pub provenance: Vec<CatalogProvenance>,
    pub providers: Vec<CatalogProviderMetadata>,
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    NoSources,
    InvalidSourceUrl,
    InvalidSourceId,
    SourceUnavailable,
    PayloadTooLarge,
    InvalidSourcePayload(CatalogSourceId),
    DuplicateSource,
    DuplicateProvider,
    DuplicateModel,
    InvalidProvenance(CatalogSourceId),
    InvalidCache,
    UnsupportedCacheVersion,
    CacheHashMismatch,
    CacheIo(io::ErrorKind),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSources => formatter.write_str("at least one catalog source is required"),
            Self::InvalidSourceUrl => formatter.write_str("catalog source URL must use HTTPS"),
            Self::InvalidSourceId => formatter.write_str("catalog source ID is invalid"),
            Self::SourceUnavailable => formatter.write_str("catalog source is unavailable"),
            Self::PayloadTooLarge => formatter.write_str("catalog source exceeds its size limit"),
            Self::InvalidSourcePayload(source) => {
                write!(formatter, "invalid payload from catalog source {source:?}")
            }
            Self::DuplicateSource => formatter.write_str("catalog contains a duplicate source"),
            Self::DuplicateProvider => {
                formatter.write_str("catalog source contains duplicate provider records")
            }
            Self::DuplicateModel => {
                formatter.write_str("catalog source contains duplicate model records")
            }
            Self::InvalidProvenance(source) => {
                write!(
                    formatter,
                    "invalid provenance for catalog source {source:?}"
                )
            }
            Self::InvalidCache => formatter.write_str("catalog cache is invalid"),
            Self::UnsupportedCacheVersion => {
                formatter.write_str("catalog cache schema version is unsupported")
            }
            Self::CacheHashMismatch => formatter.write_str("catalog cache checksum does not match"),
            Self::CacheIo(kind) => write!(formatter, "catalog cache I/O failed: {kind}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// A source adapter declares a descriptor and parses source bytes. Source
/// payloads cannot choose the descriptor URL or add request headers.
pub trait CatalogSource {
    fn descriptor(&self) -> CatalogSourceDescriptor;
    fn parse(
        &self,
        bytes: &[u8],
        retrieved_at_unix: u64,
    ) -> Result<CatalogContribution, CatalogError>;
}

/// Built-in source requests are unauthenticated and have bounded size and time.
pub trait CatalogTransport {
    fn get(&self, url: &str, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>, CatalogError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UreqCatalogTransport;

impl CatalogTransport for UreqCatalogTransport {
    fn get(&self, url: &str, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>, CatalogError> {
        if !safe_https_url(url) {
            return Err(CatalogError::InvalidSourceUrl);
        }
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .timeout_global(Some(timeout))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut response = agent
            .get(url)
            .call()
            .map_err(|_| CatalogError::SourceUnavailable)?;
        if response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > max_bytes)
        {
            return Err(CatalogError::PayloadTooLarge);
        }
        let read_limit = max_bytes as u64 + 1;
        let reader = response.body_mut().with_config().limit(read_limit).reader();
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        reader
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|_| CatalogError::SourceUnavailable)?;
        if bytes.len() > max_bytes {
            return Err(CatalogError::PayloadTooLarge);
        }
        Ok(bytes)
    }
}

mod adapters;
pub use adapters::{CloudflareWorkersAiSource, ModelsDevSource};

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_price(value: Option<&Number>) -> bool {
    value.is_none_or(|number| {
        number
            .as_f64()
            .is_some_and(|price| price.is_finite() && price >= 0.0)
    })
}

fn safe_https_url(url: &str) -> bool {
    let Some(authority_and_path) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !authority
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b'\\')
}

fn validate_contribution(contribution: &CatalogContribution) -> Result<(), CatalogError> {
    let provenance = &contribution.provenance;
    CatalogSourceId::new(provenance.source.0.clone())
        .map_err(|_| CatalogError::InvalidProvenance(provenance.source.clone()))?;
    if !safe_https_url(&provenance.url)
        || provenance.license.is_empty()
        || !safe_https_url(&provenance.license_url)
        || provenance.schema_version == 0
        || !valid_digest(&provenance.source_sha256)
    {
        return Err(CatalogError::InvalidProvenance(provenance.source.clone()));
    }
    if let Some(descriptor) = provenance.source.builtin_descriptor() {
        if provenance.url != descriptor.url
            || provenance.license != descriptor.license
            || provenance.license_url != descriptor.license_url
            || provenance.schema_version != descriptor.schema_version
        {
            return Err(CatalogError::InvalidProvenance(provenance.source.clone()));
        }
    }

    let mut provider_ids = BTreeSet::new();
    for provider in &contribution.providers {
        if provider.id.is_empty() || provider.name.is_empty() {
            return Err(CatalogError::InvalidSourcePayload(
                provenance.source.clone(),
            ));
        }
        if provider
            .documentation_url
            .as_deref()
            .is_some_and(|url| !safe_https_url(url))
        {
            return Err(CatalogError::InvalidSourcePayload(
                provenance.source.clone(),
            ));
        }
        if !provider_ids.insert(&provider.id) {
            return Err(CatalogError::DuplicateProvider);
        }
    }

    let mut model_ids = BTreeSet::new();
    for model in &contribution.models {
        if model.model.provider.is_empty()
            || model.model.id.is_empty()
            || model.sources.len() != 1
            || model.sources.first() != Some(&provenance.source)
            || model.pricing.as_ref().is_some_and(|pricing| {
                !valid_price(pricing.input_usd_per_million_tokens.as_ref())
                    || !valid_price(pricing.output_usd_per_million_tokens.as_ref())
            })
        {
            return Err(CatalogError::InvalidSourcePayload(
                provenance.source.clone(),
            ));
        }
        if !model_ids.insert((&model.model.provider, &model.model.id)) {
            return Err(CatalogError::DuplicateModel);
        }
        if provenance.source == CatalogSourceId::cloudflare_workers_ai()
            && (model.model.provider != "cloudflare-workers-ai"
                || !model.model.id.starts_with("@cf/"))
        {
            return Err(CatalogError::InvalidSourcePayload(
                provenance.source.clone(),
            ));
        }
    }
    Ok(())
}

fn validate_descriptor(descriptor: &CatalogSourceDescriptor) -> Result<(), CatalogError> {
    CatalogSourceId::new(descriptor.id.0.clone())
        .map_err(|_| CatalogError::InvalidProvenance(descriptor.id.clone()))?;
    if !safe_https_url(descriptor.url)
        || descriptor.license.is_empty()
        || !safe_https_url(descriptor.license_url)
        || descriptor.schema_version == 0
        || descriptor.max_bytes == 0
        || descriptor.max_bytes > MAX_CATALOG_SOURCE_BYTES
    {
        return Err(CatalogError::InvalidProvenance(descriptor.id.clone()));
    }
    Ok(())
}

fn validate_contribution_for_descriptor(
    contribution: &CatalogContribution,
    descriptor: &CatalogSourceDescriptor,
) -> Result<(), CatalogError> {
    validate_descriptor(descriptor)?;
    validate_contribution(contribution)?;
    let provenance = &contribution.provenance;
    if provenance.source != descriptor.id
        || provenance.url != descriptor.url
        || provenance.license != descriptor.license
        || provenance.license_url != descriptor.license_url
        || provenance.schema_version != descriptor.schema_version
    {
        return Err(CatalogError::InvalidProvenance(descriptor.id.clone()));
    }
    Ok(())
}

fn sort_contribution(
    mut contribution: CatalogContribution,
) -> Result<CatalogContribution, CatalogError> {
    contribution
        .providers
        .sort_by(|left, right| left.id.cmp(&right.id));
    contribution.models.sort_by(|left, right| {
        (&left.model.provider, &left.model.id).cmp(&(&right.model.provider, &right.model.id))
    });
    for model in &mut contribution.models {
        model.sources.sort();
        model.sources.dedup();
    }
    validate_contribution(&contribution)?;
    Ok(contribution)
}

fn sort_snapshot(mut snapshot: CatalogSnapshot) -> Result<CatalogSnapshot, CatalogError> {
    snapshot
        .contributions
        .sort_by_key(|item| item.provenance.source.clone());
    snapshot.contributions = snapshot
        .contributions
        .into_iter()
        .map(sort_contribution)
        .collect::<Result<Vec<_>, _>>()?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn validate_snapshot(snapshot: &CatalogSnapshot) -> Result<(), CatalogError> {
    if snapshot.schema_version != CATALOG_CACHE_SCHEMA_VERSION {
        return Err(CatalogError::UnsupportedCacheVersion);
    }
    if snapshot.contributions.is_empty() {
        return Err(CatalogError::NoSources);
    }
    if snapshot
        .contributions
        .windows(2)
        .any(|pair| pair[0].provenance.source >= pair[1].provenance.source)
    {
        return Err(CatalogError::InvalidCache);
    }
    let mut sources = BTreeSet::new();
    for contribution in &snapshot.contributions {
        if contribution
            .providers
            .windows(2)
            .any(|pair| pair[0].id >= pair[1].id)
            || contribution.models.windows(2).any(|pair| {
                (&pair[0].model.provider, &pair[0].model.id)
                    >= (&pair[1].model.provider, &pair[1].model.id)
            })
            || contribution.models.iter().any(|model| {
                model.sources.len() != 1 || model.sources[0] != contribution.provenance.source
            })
        {
            return Err(CatalogError::InvalidCache);
        }
        if !sources.insert(contribution.provenance.source.clone()) {
            return Err(CatalogError::DuplicateSource);
        }
        validate_contribution(contribution)?;
    }
    merge_contributions(&snapshot.contributions)?;
    Ok(())
}

/// Merge source records in stable source order. models.dev supplies broad
/// metadata; Cloudflare's own Workers AI feed has precedence for its records.
pub fn merge_contributions(
    contributions: &[CatalogContribution],
) -> Result<CatalogView, CatalogError> {
    let mut ordered = contributions
        .iter()
        .cloned()
        .map(sort_contribution)
        .collect::<Result<Vec<_>, _>>()?;
    ordered.sort_by_key(|item| {
        (
            merge_rank(&item.provenance.source),
            item.provenance.source.clone(),
        )
    });
    let mut seen_sources = BTreeSet::new();
    let mut provenance = Vec::with_capacity(ordered.len());
    let mut providers: BTreeMap<String, CatalogProviderMetadata> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), CatalogModel> = BTreeMap::new();

    for contribution in &ordered {
        validate_contribution(contribution)?;
        if !seen_sources.insert(contribution.provenance.source.clone()) {
            return Err(CatalogError::DuplicateSource);
        }
        provenance.push(contribution.provenance.clone());
        for provider in &contribution.providers {
            providers
                .entry(provider.id.clone())
                .and_modify(|existing| {
                    if existing.documentation_url.is_none() {
                        existing.documentation_url = provider.documentation_url.clone();
                    }
                })
                .or_insert_with(|| provider.clone());
        }
        for model in &contribution.models {
            let key = (model.model.provider.clone(), model.model.id.clone());
            match models.get_mut(&key) {
                Some(existing) => merge_model(
                    existing,
                    model,
                    contribution.provenance.source == CatalogSourceId::cloudflare_workers_ai(),
                ),
                None => {
                    models.insert(key, model.clone());
                }
            }
        }
    }

    Ok(CatalogView {
        provenance,
        providers: providers.into_values().collect(),
        models: models.into_values().collect(),
    })
}

fn merge_rank(source: &CatalogSourceId) -> u8 {
    if source == &CatalogSourceId::models_dev() {
        0
    } else if source == &CatalogSourceId::cloudflare_workers_ai() {
        2
    } else {
        1
    }
}

fn merge_model(
    existing: &mut CatalogModel,
    incoming: &CatalogModel,
    source_is_authoritative: bool,
) {
    let current = &mut existing.model;
    let next = &incoming.model;
    if (source_is_authoritative || current.name == current.id) && next.name != next.id {
        current.name = next.name.clone();
    }
    merge_option(
        &mut current.context_window,
        next.context_window,
        source_is_authoritative,
    );
    merge_option(
        &mut current.max_output_tokens,
        next.max_output_tokens,
        source_is_authoritative,
    );
    merge_option(
        &mut current.tool_calling,
        next.tool_calling,
        source_is_authoritative,
    );
    merge_option(
        &mut current.reasoning,
        next.reasoning,
        source_is_authoritative,
    );
    if next.status.is_some() && (source_is_authoritative || current.status.is_none()) {
        current.status = next.status.clone();
    }
    if next.modalities.is_some() && (source_is_authoritative || current.modalities.is_none()) {
        current.modalities = next.modalities.clone();
    }
    if existing.pricing.is_none() {
        existing.pricing = incoming.pricing.clone();
    }
    merge_features(
        &mut existing.features,
        &incoming.features,
        source_is_authoritative,
    );
    existing.sources.extend(incoming.sources.iter().cloned());
    existing.sources.sort();
    existing.sources.dedup();
}

fn merge_option<T: Clone>(current: &mut Option<T>, next: Option<T>, authoritative: bool) {
    if next.is_some() && (authoritative || current.is_none()) {
        *current = next;
    }
}

fn merge_features(current: &mut ModelFeatures, next: &ModelFeatures, authoritative: bool) {
    merge_option(
        &mut current.supports_attachments,
        next.supports_attachments,
        authoritative,
    );
    merge_option(
        &mut current.structured_output,
        next.structured_output,
        authoritative,
    );
    merge_option(&mut current.vision, next.vision, authoritative);
    merge_option(
        &mut current.requires_workers_paid,
        next.requires_workers_paid,
        authoritative,
    );
    merge_option(&mut current.task, next.task.clone(), authoritative);
    merge_option(
        &mut current.planned_deprecation_date,
        next.planned_deprecation_date.clone(),
        authoritative,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRefreshResult {
    pub snapshot: CatalogSnapshot,
    pub updated: bool,
    pub used_last_known_good: bool,
    pub cache_is_stale: bool,
    pub failed_source: Option<CatalogSourceId>,
    pub source_error: Option<CatalogError>,
}

impl CatalogSnapshot {
    pub fn merged(&self) -> Result<CatalogView, CatalogError> {
        validate_snapshot(self)?;
        merge_contributions(&self.contributions)
    }

    pub fn is_stale_at(&self, now_unix: u64) -> bool {
        self.contributions.is_empty()
            || self.contributions.iter().any(|contribution| {
                now_unix.saturating_sub(contribution.provenance.retrieved_at_unix)
                    > MAX_CATALOG_CACHE_AGE_SECONDS
            })
    }
}

pub fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

mod cache;
pub use cache::{import_catalog_payload, read_catalog_cache, refresh_catalog};
#[cfg(test)]
mod tests;
