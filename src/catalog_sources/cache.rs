use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogCacheFile {
    pub(super) schema_version: u32,
    pub(super) payload_sha256: String,
    pub(super) snapshot: CatalogSnapshot,
}

/// Read and verify an optional last-known-good catalog cache.
pub fn read_catalog_cache(path: &Path) -> Result<Option<CatalogSnapshot>, CatalogError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CatalogError::CacheIo(error.kind())),
    };
    if !metadata.file_type().is_file() {
        return Err(CatalogError::InvalidCache);
    }
    let file = open_cache_file(path).map_err(|error| CatalogError::CacheIo(error.kind()))?;
    let mut bytes = Vec::new();
    file.take((MAX_CATALOG_CACHE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| CatalogError::CacheIo(error.kind()))?;
    if bytes.len() > MAX_CATALOG_CACHE_BYTES {
        return Err(CatalogError::PayloadTooLarge);
    }
    let cache: CatalogCacheFile =
        serde_json::from_slice(&bytes).map_err(|_| CatalogError::InvalidCache)?;
    if cache.schema_version != CATALOG_CACHE_SCHEMA_VERSION
        || cache.snapshot.schema_version != CATALOG_CACHE_SCHEMA_VERSION
    {
        return Err(CatalogError::UnsupportedCacheVersion);
    }
    let canonical = cache_file_bytes(&cache)?;
    if canonical != bytes {
        return Err(CatalogError::InvalidCache);
    }
    let payload = serde_json::to_vec(&cache.snapshot).map_err(|_| CatalogError::InvalidCache)?;
    if !valid_digest(&cache.payload_sha256) || sha256(&payload) != cache.payload_sha256 {
        return Err(CatalogError::CacheHashMismatch);
    }
    validate_snapshot(&cache.snapshot)?;
    Ok(Some(cache.snapshot))
}

/// Import bytes through one fixed source adapter. Invalid input leaves an
/// existing cache untouched.
pub fn import_catalog_payload(
    cache_path: &Path,
    source: &dyn CatalogSource,
    bytes: &[u8],
    now_unix: u64,
) -> Result<CatalogRefreshResult, CatalogError> {
    let descriptor = source.descriptor();
    validate_descriptor(&descriptor)?;
    if bytes.len() > descriptor.max_bytes {
        return Err(CatalogError::PayloadTooLarge);
    }
    let mut snapshot = match read_catalog_cache(cache_path) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => empty_snapshot(now_unix),
        Err(error @ CatalogError::CacheIo(_)) => return Err(error),
        Err(_) => empty_snapshot(now_unix),
    };
    let contribution = source.parse(bytes, now_unix)?;
    validate_contribution_for_descriptor(&contribution, &descriptor)?;
    snapshot
        .contributions
        .retain(|existing| existing.provenance.source != contribution.provenance.source);
    snapshot.contributions.push(contribution);
    snapshot.refreshed_at_unix = now_unix;
    snapshot = sort_snapshot(snapshot)?;
    write_catalog_cache(cache_path, &snapshot)?;
    Ok(CatalogRefreshResult {
        snapshot,
        updated: true,
        used_last_known_good: false,
        cache_is_stale: false,
        failed_source: None,
        source_error: None,
    })
}

fn empty_snapshot(now_unix: u64) -> CatalogSnapshot {
    CatalogSnapshot {
        schema_version: CATALOG_CACHE_SCHEMA_VERSION,
        refreshed_at_unix: now_unix,
        contributions: Vec::new(),
    }
}

/// Refresh only the supplied source adapters. If any source fails validation
/// or retrieval, the complete last-known-good snapshot is returned unchanged.
pub fn refresh_catalog(
    cache_path: &Path,
    sources: &[&dyn CatalogSource],
    transport: &dyn CatalogTransport,
    now_unix: u64,
) -> Result<CatalogRefreshResult, CatalogError> {
    if sources.is_empty() {
        return Err(CatalogError::NoSources);
    }
    let descriptors: Vec<_> = sources.iter().map(|source| source.descriptor()).collect();
    let mut source_ids = BTreeSet::new();
    for descriptor in &descriptors {
        validate_descriptor(descriptor)?;
        if !source_ids.insert(descriptor.id.clone()) {
            return Err(CatalogError::DuplicateSource);
        }
    }

    let (previous, cache_error) = match read_catalog_cache(cache_path) {
        Ok(snapshot) => (snapshot, None),
        Err(error) => (None, Some(error)),
    };
    let mut fresh = Vec::with_capacity(sources.len());
    for (source, descriptor) in sources.iter().zip(descriptors.iter()) {
        let contribution = transport
            .get(descriptor.url, descriptor.max_bytes, SOURCE_TIMEOUT)
            .and_then(|bytes| {
                if bytes.len() > descriptor.max_bytes {
                    return Err(CatalogError::PayloadTooLarge);
                }
                source.parse(&bytes, now_unix)
            })
            .and_then(|contribution| {
                validate_contribution_for_descriptor(&contribution, descriptor)?;
                Ok(contribution)
            });
        match contribution {
            Ok(contribution) => fresh.push(contribution),
            Err(error) => {
                if let Some(snapshot) = previous.as_ref().cloned() {
                    let stale = snapshot.is_stale_at(now_unix);
                    return Ok(CatalogRefreshResult {
                        snapshot,
                        updated: false,
                        used_last_known_good: true,
                        cache_is_stale: stale,
                        failed_source: Some(descriptor.id.clone()),
                        source_error: Some(error),
                    });
                }
                return Err(cache_error.unwrap_or(error));
            }
        }
    }

    let mut snapshot = previous.unwrap_or(CatalogSnapshot {
        schema_version: CATALOG_CACHE_SCHEMA_VERSION,
        refreshed_at_unix: now_unix,
        contributions: Vec::new(),
    });
    for contribution in fresh {
        snapshot
            .contributions
            .retain(|existing| existing.provenance.source != contribution.provenance.source);
        snapshot.contributions.push(contribution);
    }
    snapshot.refreshed_at_unix = now_unix;
    snapshot = sort_snapshot(snapshot)?;
    write_catalog_cache(cache_path, &snapshot)?;
    let cache_is_stale = snapshot.is_stale_at(now_unix);
    Ok(CatalogRefreshResult {
        snapshot,
        updated: true,
        used_last_known_good: false,
        cache_is_stale,
        failed_source: None,
        source_error: None,
    })
}

fn write_catalog_cache(path: &Path, snapshot: &CatalogSnapshot) -> Result<(), CatalogError> {
    let snapshot = sort_snapshot(snapshot.clone())?;
    let payload = serde_json::to_vec(&snapshot).map_err(|_| CatalogError::InvalidCache)?;
    let cache = CatalogCacheFile {
        schema_version: CATALOG_CACHE_SCHEMA_VERSION,
        payload_sha256: sha256(&payload),
        snapshot,
    };
    let bytes = cache_file_bytes(&cache)?;
    if bytes.len() > MAX_CATALOG_CACHE_BYTES {
        return Err(CatalogError::PayloadTooLarge);
    }
    atomic_replace(path, &bytes)
}

pub(super) fn cache_file_bytes(cache: &CatalogCacheFile) -> Result<Vec<u8>, CatalogError> {
    let mut bytes = serde_json::to_vec_pretty(cache).map_err(|_| CatalogError::InvalidCache)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), CatalogError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| CatalogError::CacheIo(error.kind()))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(CatalogError::InvalidCache);
        }
    }
    let temporary = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| CatalogError::CacheIo(error.kind()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(CatalogError::CacheIo(error.kind()));
    }
    drop(file);

    #[cfg(unix)]
    let replace_result = fs::rename(&temporary, path);
    #[cfg(not(unix))]
    let replace_result = replace_with_rollback(&temporary, path);

    if let Err(error) = replace_result {
        let _ = fs::remove_file(&temporary);
        return Err(CatalogError::CacheIo(error.kind()));
    }
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CatalogError::CacheIo(error.kind()))?;
    Ok(())
}

#[cfg(not(unix))]
fn replace_with_rollback(temporary: &Path, destination: &Path) -> io::Result<()> {
    if !destination.exists() {
        return fs::rename(temporary, destination);
    }
    let backup = temporary_path(destination);
    fs::rename(destination, &backup)?;
    if let Err(error) = fs::rename(temporary, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(error);
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("catalog-cache");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

#[cfg(unix)]
fn open_cache_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_cache_file(path: &Path) -> io::Result<File> {
    File::open(path)
}
