//! Per-user view over the embedded catalog and an explicitly refreshed cache.

use crate::catalog_data::ModelSpec;
use crate::catalog_sources::{
    read_catalog_cache, refresh_catalog, CatalogError, CatalogModel, CatalogProvenance,
    CatalogRefreshResult, CatalogSnapshot, CatalogSource, CatalogView, CloudflareWorkersAiSource,
    ModelsDevSource, UreqCatalogTransport, MAX_CATALOG_CACHE_AGE_SECONDS,
};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

const CACHE_FILE: &str = "model-catalog-cache.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CatalogOrigin {
    Embedded,
    RefreshedCache,
}

#[derive(Clone, Debug)]
pub(super) struct LoadedCatalog {
    pub(super) view: CatalogView,
    pub(super) origin: CatalogOrigin,
    pub(super) stale: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ModelCatalogError {
    CacheRead(CatalogError),
    CacheMerge(CatalogError),
    Refresh(CatalogError),
    EmbeddedMetadataInvalid,
    EmbeddedSourceMismatch,
    EmbeddedMetadataFieldsInvalid,
    EmbeddedSnapshotChecksumMismatch,
    EmbeddedRetrievalTimeInvalid,
    EmbeddedCatalogInvalid,
    EmbeddedCatalogCountsMismatch,
}

impl fmt::Display for ModelCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheRead(error) => write!(formatter, "model catalog cache is invalid: {error}"),
            Self::CacheMerge(error) => {
                write!(formatter, "model catalog cache cannot be merged: {error}")
            }
            Self::Refresh(error) => write!(formatter, "catalog refresh failed: {error}"),
            Self::EmbeddedMetadataInvalid => {
                formatter.write_str("embedded model catalog provenance is invalid")
            }
            Self::EmbeddedSourceMismatch => formatter.write_str(
                "embedded model catalog provenance does not match its source descriptor",
            ),
            Self::EmbeddedMetadataFieldsInvalid => {
                formatter.write_str("embedded model catalog hashes or counts are invalid")
            }
            Self::EmbeddedSnapshotChecksumMismatch => {
                formatter.write_str("embedded model catalog checksum does not match its provenance")
            }
            Self::EmbeddedRetrievalTimeInvalid => {
                formatter.write_str("embedded model catalog retrieval time is invalid")
            }
            Self::EmbeddedCatalogInvalid => {
                formatter.write_str("embedded model catalog is invalid")
            }
            Self::EmbeddedCatalogCountsMismatch => {
                formatter.write_str("embedded model catalog counts do not match its provenance")
            }
        }
    }
}

impl std::error::Error for ModelCatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CacheRead(error) | Self::CacheMerge(error) | Self::Refresh(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddedMetadata {
    generator_version: String,
    license: String,
    model_count: usize,
    provider_count: usize,
    repository: String,
    retrieved_at: String,
    sha256: String,
    snapshot_sha256: String,
    source: String,
    #[serde(default)]
    trusted_input_sha256: Option<String>,
    #[serde(default)]
    upstream_revision: Option<String>,
}

pub(super) fn load_catalog(cache_path: Option<&Path>) -> Result<LoadedCatalog, ModelCatalogError> {
    if let Some(path) = cache_path {
        match read_catalog_cache(path) {
            Ok(Some(snapshot)) => return loaded_cache(snapshot),
            Ok(None) => {}
            Err(error) => return Err(ModelCatalogError::CacheRead(error)),
        }
    }
    embedded_catalog()
}

pub(super) fn refresh_user_catalog(
    cache_path: &Path,
) -> Result<CatalogRefreshResult, ModelCatalogError> {
    let models_dev = ModelsDevSource;
    let cloudflare = CloudflareWorkersAiSource;
    let sources: [&dyn CatalogSource; 2] = [&models_dev, &cloudflare];
    refresh_catalog(
        cache_path,
        &sources,
        &UreqCatalogTransport,
        crate::catalog_sources::unix_timestamp_now(),
    )
    .map_err(ModelCatalogError::Refresh)
}

pub(super) fn catalog_from_snapshot(
    snapshot: CatalogSnapshot,
) -> Result<LoadedCatalog, ModelCatalogError> {
    let stale = snapshot.is_stale_at(crate::catalog_sources::unix_timestamp_now());
    let view = snapshot.merged().map_err(ModelCatalogError::CacheMerge)?;
    Ok(LoadedCatalog {
        view,
        origin: CatalogOrigin::RefreshedCache,
        stale,
    })
}

pub(super) fn cache_path_for_user_settings(model_path: &Path) -> PathBuf {
    model_path.with_file_name(CACHE_FILE)
}

fn loaded_cache(snapshot: CatalogSnapshot) -> Result<LoadedCatalog, ModelCatalogError> {
    catalog_from_snapshot(snapshot)
}

fn embedded_catalog() -> Result<LoadedCatalog, ModelCatalogError> {
    let metadata: EmbeddedMetadata =
        serde_json::from_str(include_str!("../../snapshots/models-dev/metadata.json"))
            .map_err(|_| ModelCatalogError::EmbeddedMetadataInvalid)?;
    let source = ModelsDevSource;
    let descriptor = source.descriptor();
    if metadata.source != descriptor.url || metadata.license != descriptor.license {
        return Err(ModelCatalogError::EmbeddedSourceMismatch);
    }
    if metadata.sha256.len() != 64
        || !metadata.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || metadata.snapshot_sha256.len() != 64
        || !metadata
            .snapshot_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || metadata
            .trusted_input_sha256
            .as_deref()
            .is_some_and(|hash| !is_hex_digest(hash, 64))
        || metadata
            .upstream_revision
            .as_deref()
            .is_some_and(|revision| !is_hex_digest(revision, 40))
        || metadata.generator_version.is_empty()
        || metadata.model_count == 0
        || metadata.provider_count == 0
    {
        return Err(ModelCatalogError::EmbeddedMetadataFieldsInvalid);
    }
    let snapshot_bytes = include_bytes!("../../snapshots/models-dev/api.json");
    let snapshot_hash = ring::digest::digest(&ring::digest::SHA256, snapshot_bytes);
    let snapshot_hash = snapshot_hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if snapshot_hash != metadata.snapshot_sha256.to_ascii_lowercase() {
        return Err(ModelCatalogError::EmbeddedSnapshotChecksumMismatch);
    }
    let retrieved_at_unix = parse_rfc3339_utc(&metadata.retrieved_at)
        .ok_or(ModelCatalogError::EmbeddedRetrievalTimeInvalid)?;
    let provenance = CatalogProvenance {
        source: descriptor.id,
        url: metadata.source,
        license: descriptor.license.to_owned(),
        license_url: descriptor.license_url.to_owned(),
        source_sha256: metadata.sha256,
        retrieved_at_unix,
        schema_version: descriptor.schema_version,
    };
    let embedded_models =
        crate::catalog_data::models().map_err(|_| ModelCatalogError::EmbeddedCatalogInvalid)?;
    let provider_count = embedded_models
        .iter()
        .map(|model| model.provider.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if embedded_models.len() != metadata.model_count || provider_count != metadata.provider_count {
        return Err(ModelCatalogError::EmbeddedCatalogCountsMismatch);
    }
    let models = embedded_models
        .into_iter()
        .map(|model| embedded_model(model, &provenance))
        .collect::<Vec<_>>();
    let view = CatalogView {
        provenance: vec![provenance.clone()],
        providers: Vec::new(),
        models,
    };
    let _ = (
        metadata.repository,
        metadata.trusted_input_sha256,
        metadata.upstream_revision,
    );
    let stale = crate::catalog_sources::unix_timestamp_now().saturating_sub(retrieved_at_unix)
        > MAX_CATALOG_CACHE_AGE_SECONDS;
    Ok(LoadedCatalog {
        view,
        origin: CatalogOrigin::Embedded,
        stale,
    })
}

fn embedded_model(model: ModelSpec, provenance: &CatalogProvenance) -> CatalogModel {
    CatalogModel {
        model,
        pricing: None,
        features: Default::default(),
        sources: vec![provenance.source.clone()],
    }
}

fn is_hex_digest(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_rfc3339_utc(value: &str) -> Option<u64> {
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-');
    let year = date.next()?.parse::<i64>().ok()?;
    let month = date.next()?.parse::<i64>().ok()?;
    let day = date.next()?.parse::<i64>().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time_end = time.find(['Z', '+', '-']).unwrap_or(time.len());
    let clock = &time[..time_end];
    let mut clock_parts = clock.split(':');
    let hour = clock_parts.next()?.parse::<i64>().ok()?;
    let minute = clock_parts.next()?.parse::<i64>().ok()?;
    let second_text = clock_parts.next()?;
    if clock_parts.next().is_some() || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    let second = second_text.split('.').next()?.parse::<i64>().ok()?;
    if !(0..=59).contains(&second) {
        return None;
    }
    let suffix = &time[time_end..];
    let offset_seconds = match suffix {
        "Z" | "+00:00" => 0,
        _ if suffix.len() == 6 && matches!(suffix.as_bytes()[0], b'+' | b'-') => {
            let offset_hour = suffix[1..3].parse::<i64>().ok()?;
            let offset_minute = suffix[4..6].parse::<i64>().ok()?;
            if suffix.as_bytes()[3] != b':' || offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let sign = if suffix.starts_with('+') { 1 } else { -1 };
            sign * (offset_hour * 3600 + offset_minute * 60)
        }
        _ => return None,
    };

    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    let seconds = days_since_epoch
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?
        .checked_sub(offset_seconds)?;
    u64::try_from(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::{catalog_from_snapshot, load_catalog, parse_rfc3339_utc, ModelCatalogError};
    use crate::catalog_sources::{CatalogError, CatalogSnapshot, CATALOG_CACHE_SCHEMA_VERSION};
    use crate::test_support::TempDir;

    #[test]
    fn parses_rfc3339_timestamps_and_offsets() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_rfc3339_utc("not-a-time"), None);
    }

    #[test]
    fn cache_merge_error_keeps_catalog_cause_and_existing_display() {
        let error = catalog_from_snapshot(CatalogSnapshot {
            schema_version: CATALOG_CACHE_SCHEMA_VERSION,
            refreshed_at_unix: 0,
            contributions: Vec::new(),
        })
        .unwrap_err();

        assert_eq!(
            error,
            ModelCatalogError::CacheMerge(CatalogError::NoSources)
        );
        assert_eq!(
            error.to_string(),
            "model catalog cache cannot be merged: at least one catalog source is required"
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn invalid_cache_keeps_typed_catalog_cause_and_existing_display() {
        let directory = TempDir::new();
        let cache_path = directory.write_file("catalog.json", b"{");

        let error = load_catalog(Some(&cache_path)).unwrap_err();

        assert_eq!(
            error,
            ModelCatalogError::CacheRead(CatalogError::InvalidCache)
        );
        assert_eq!(
            error.to_string(),
            "model catalog cache is invalid: catalog cache is invalid"
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn catalog_failure_variants_keep_existing_displays() {
        let cases = [
            (
                ModelCatalogError::Refresh(CatalogError::SourceUnavailable),
                "catalog refresh failed: catalog source is unavailable",
            ),
            (
                ModelCatalogError::EmbeddedMetadataInvalid,
                "embedded model catalog provenance is invalid",
            ),
            (
                ModelCatalogError::EmbeddedSourceMismatch,
                "embedded model catalog provenance does not match its source descriptor",
            ),
            (
                ModelCatalogError::EmbeddedMetadataFieldsInvalid,
                "embedded model catalog hashes or counts are invalid",
            ),
            (
                ModelCatalogError::EmbeddedSnapshotChecksumMismatch,
                "embedded model catalog checksum does not match its provenance",
            ),
            (
                ModelCatalogError::EmbeddedRetrievalTimeInvalid,
                "embedded model catalog retrieval time is invalid",
            ),
            (
                ModelCatalogError::EmbeddedCatalogInvalid,
                "embedded model catalog is invalid",
            ),
            (
                ModelCatalogError::EmbeddedCatalogCountsMismatch,
                "embedded model catalog counts do not match its provenance",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }
}
