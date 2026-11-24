//! Deterministic offline normalizer for the pinned models.dev snapshot.
//! Default mode writes the optional normalized JSON artifact.
//! Use --check to validate it, or --from-upstream <path> to refresh the slim snapshot.
use krypper::catalog_sources::{CatalogSource, ModelsDevSource};
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
#[cfg(not(unix))]
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// Increment when the normalized record schema or field mapping changes.
const GENERATOR_VERSION: &str = "2";
static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Deserialize, Serialize)]
struct ModelRecord {
    provider: String,
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    // Preserve a present JSON null as Some(Value::Null), distinct from a missing field.
    #[serde(
        default,
        deserialize_with = "deserialize_present_value",
        skip_serializing_if = "Option::is_none"
    )]
    modalities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

fn deserialize_present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn has_terminal_controls(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_upstream_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut output = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    output.push(b'\n');
    Ok(output)
}

fn temporary_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("catalog");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), id))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
    }

    let temporary = temporary_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    drop(file);

    #[cfg(unix)]
    let replace_result = fs::rename(&temporary, path);
    #[cfg(not(unix))]
    let replace_result = replace_with_rollback(&temporary, path);

    if let Err(error) = replace_result {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }

    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
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

fn rollback_catalog_files(
    snapshot_path: &Path,
    metadata_path: &Path,
    old_snapshot: &[u8],
    old_metadata: &[u8],
) -> Result<(), String> {
    atomic_write(snapshot_path, old_snapshot)?;
    atomic_write(metadata_path, old_metadata)
}

fn write_catalog_files_atomically(
    snapshot_path: &Path,
    snapshot: &[u8],
    metadata_path: &Path,
    metadata: &[u8],
) -> Result<(), String> {
    let metadata_value: Value =
        serde_json::from_slice(metadata).map_err(|error| error.to_string())?;
    validate_slim_snapshot(snapshot, &metadata_value)?;
    let old_snapshot = fs::read(snapshot_path).map_err(|error| error.to_string())?;
    let old_metadata = fs::read(metadata_path).map_err(|error| error.to_string())?;

    if let Err(error) = atomic_write(snapshot_path, snapshot) {
        return match rollback_catalog_files(
            snapshot_path,
            metadata_path,
            &old_snapshot,
            &old_metadata,
        ) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; rollback failed: {rollback}")),
        };
    }
    if let Err(error) = atomic_write(metadata_path, metadata) {
        return match rollback_catalog_files(
            snapshot_path,
            metadata_path,
            &old_snapshot,
            &old_metadata,
        ) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; rollback failed: {rollback}")),
        };
    }

    let post_write_check = (|| {
        let written_snapshot = fs::read(snapshot_path).map_err(|error| error.to_string())?;
        let written_metadata = fs::read(metadata_path).map_err(|error| error.to_string())?;
        let written_metadata_value: Value =
            serde_json::from_slice(&written_metadata).map_err(|error| error.to_string())?;
        if written_snapshot != snapshot || written_metadata != metadata {
            return Err("catalog files differ from the staged contents".to_owned());
        }
        validate_slim_snapshot(&written_snapshot, &written_metadata_value)?;
        Ok(())
    })();
    if let Err(check_error) = post_write_check {
        return match rollback_catalog_files(snapshot_path, metadata_path, &old_snapshot, &old_metadata) {
            Ok(()) => Err(format!("catalog files failed post-write consistency checks: {check_error}")),
            Err(error) => Err(format!("catalog files failed post-write consistency checks: {check_error}; rollback failed: {error}")),
        };
    }
    Ok(())
}

/// Keep the generated slim snapshot aligned with the runtime source adapter.
fn records_from_contribution(
    contribution: krypper::catalog_sources::CatalogContribution,
) -> Vec<ModelRecord> {
    let mut records: Vec<_> = contribution
        .models
        .into_iter()
        .map(|entry| {
            let model = entry.model;
            ModelRecord {
                provider: model.provider,
                id: model.id,
                name: model.name,
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                tool_calling: model.tool_calling,
                reasoning: model.reasoning,
                modalities: model.modalities,
                status: model.status,
            }
        })
        .collect();
    records.sort_by(|left, right| (&left.provider, &left.id).cmp(&(&right.provider, &right.id)));
    records
}

#[cfg(test)]
fn normalize(raw: &Value) -> Result<Vec<ModelRecord>, String> {
    let bytes = serde_json::to_vec(raw).map_err(|error| error.to_string())?;
    let contribution = ModelsDevSource
        .parse(&bytes, 0)
        .map_err(|error| error.to_string())?;
    Ok(records_from_contribution(contribution))
}

fn generate_from_upstream(source: &[u8], metadata: &mut Value) -> Result<Vec<u8>, String> {
    let contribution = ModelsDevSource
        .parse(source, 0)
        .map_err(|error| error.to_string())?;
    let records = records_from_contribution(contribution);
    let snapshot = pretty_json(&records)?;
    let providers = records
        .iter()
        .map(|record| record.provider.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;

    let fields = metadata
        .as_object_mut()
        .ok_or("metadata root must be an object")?;
    let input_hash = sha256(source);
    if fields.get("trusted_input_sha256").and_then(Value::as_str) != Some(input_hash.as_str()) {
        fields.remove("upstream_revision");
    }
    fields.insert("trusted_input_sha256".into(), Value::String(input_hash));
    fields.insert("snapshot_sha256".into(), Value::String(sha256(&snapshot)));
    fields.insert(
        "generator_version".into(),
        Value::String(GENERATOR_VERSION.into()),
    );
    fields.insert("provider_count".into(), Value::from(providers));
    fields.insert("model_count".into(), Value::from(records.len() as u64));
    Ok(snapshot)
}

fn validate_slim_snapshot(source: &[u8], metadata: &Value) -> Result<Vec<ModelRecord>, String> {
    let upstream_hash = metadata
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or("missing upstream sha256")?;
    if !valid_sha256(upstream_hash) {
        return Err("Invalid upstream SHA-256".into());
    }
    let trusted_input_hash = metadata
        .get("trusted_input_sha256")
        .and_then(Value::as_str)
        .ok_or("missing trusted upstream input sha256")?;
    if !valid_sha256(trusted_input_hash) {
        return Err("Invalid trusted upstream input SHA-256".into());
    }
    let expected_hash = metadata
        .get("snapshot_sha256")
        .and_then(Value::as_str)
        .ok_or("missing snapshot_sha256")?;
    if sha256(source) != expected_hash {
        return Err("Slim snapshot SHA-256 mismatch".into());
    }
    if metadata.get("generator_version").and_then(Value::as_str) != Some(GENERATOR_VERSION) {
        return Err("Snapshot generator version mismatch".into());
    }

    let records: Vec<ModelRecord> =
        serde_json::from_slice(source).map_err(|error| error.to_string())?;
    if records.iter().any(|record| {
        has_terminal_controls(&record.provider)
            || has_terminal_controls(&record.id)
            || has_terminal_controls(&record.name)
            || record.status.as_deref().is_some_and(has_terminal_controls)
    }) {
        return Err("Snapshot contains terminal control characters".into());
    }
    if pretty_json(&records)? != source {
        return Err("Snapshot is not a canonical slim artifact".into());
    }
    let providers = records
        .iter()
        .map(|record| record.provider.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    if metadata.get("provider_count").and_then(Value::as_u64) != Some(providers) {
        return Err("Snapshot provider count mismatch".into());
    }
    if metadata.get("model_count").and_then(Value::as_u64) != Some(records.len() as u64) {
        return Err("Snapshot model count mismatch".into());
    }
    if records
        .windows(2)
        .any(|pair| (&pair[0].provider, &pair[0].id) > (&pair[1].provider, &pair[1].id))
    {
        return Err("Snapshot models are not sorted".into());
    }
    Ok(records)
}

fn verify_runtime(records: &[ModelRecord]) -> Result<(), String> {
    let runtime = krypper::catalog_data::models().map_err(|error| error.to_string())?;
    if records.len() != runtime.len() {
        return Err("Catalog record count differs from the Rust runtime".into());
    }
    for (record, model) in records.iter().zip(runtime.iter()) {
        if record.provider != model.provider
            || record.id != model.id
            || record.name != model.name
            || record.context_window != model.context_window
            || record.max_output_tokens != model.max_output_tokens
            || record.tool_calling != model.tool_calling
            || record.reasoning != model.reasoning
            || record.modalities != model.modalities
            || record.status != model.status
        {
            return Err("Catalog differs from the Rust runtime".into());
        }
    }
    Ok(())
}

fn verify_trusted_upstream(
    source: &[u8],
    snapshot: &[u8],
    metadata: &Value,
) -> Result<Vec<ModelRecord>, String> {
    let upstream_revision = metadata
        .get("upstream_revision")
        .and_then(Value::as_str)
        .ok_or("missing upstream revision")?;
    if !valid_upstream_revision(upstream_revision) {
        return Err("Invalid upstream revision".into());
    }
    let expected_source_hash = metadata
        .get("trusted_input_sha256")
        .and_then(Value::as_str)
        .ok_or("missing upstream sha256")?;
    if sha256(source) != expected_source_hash {
        return Err("Trusted upstream input SHA-256 mismatch".into());
    }
    let mut regenerated_metadata = metadata.clone();
    let regenerated_snapshot = generate_from_upstream(source, &mut regenerated_metadata)?;
    if regenerated_metadata != *metadata {
        return Err("Catalog metadata differs from trusted upstream input".into());
    }
    if regenerated_snapshot != snapshot {
        return Err("Runtime snapshot differs from trusted upstream input".into());
    }
    validate_slim_snapshot(snapshot, metadata)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = match args.as_slice() {
        [] => ("generate", None),
        [arg] if arg == "--check" => ("check", None),
        [flag, path] if flag == "--from-upstream" => {
            ("from-upstream", Some(PathBuf::from(path)))
        }
        [flag, path] if flag == "--check-upstream" => {
            ("check-upstream", Some(PathBuf::from(path)))
        }
        _ => {
            return Err("usage: cargo run --example generate_model_catalog -- [--check | --check-upstream <raw-upstream-json> | --from-upstream <raw-upstream-json>]".into())
        }
    };

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let snapshot_path = root.join("snapshots/models-dev/api.json");
    let metadata_path = root.join("snapshots/models-dev/metadata.json");
    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&metadata_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;

    if let Some(upstream_path) = mode.1 {
        let source = fs::read(upstream_path).map_err(|error| error.to_string())?;
        if mode.0 == "from-upstream" {
            let source_hash = sha256(&source);
            if metadata.get("snapshot_sha256").and_then(Value::as_str) == Some(source_hash.as_str())
            {
                return Err("input is the slim snapshot; provide original upstream bytes".into());
            }
            metadata["sha256"] = Value::String(source_hash);
            let snapshot = generate_from_upstream(&source, &mut metadata)?;
            let metadata_bytes = pretty_json(&metadata)?;
            write_catalog_files_atomically(
                &snapshot_path,
                &snapshot,
                &metadata_path,
                &metadata_bytes,
            )?;
            println!("Runtime model snapshot and metadata generated atomically");
            return Ok(());
        }

        let checked_snapshot = fs::read(&snapshot_path).map_err(|error| error.to_string())?;
        let records = verify_trusted_upstream(&source, &checked_snapshot, &metadata)?;
        verify_runtime(&records)?;
        println!("Verified trusted upstream input and runtime catalog parity");
        return Ok(());
    }

    let source = fs::read(&snapshot_path).map_err(|error| error.to_string())?;
    let records = validate_slim_snapshot(&source, &metadata)?;
    if mode.0 == "check" {
        verify_runtime(&records)?;
        println!("Verified {} catalog records", records.len());
    } else {
        let runtime_snapshot = pretty_json(&records)?;
        if runtime_snapshot != source {
            return Err("Runtime snapshot is not canonical".into());
        }
        atomic_write(&snapshot_path, &runtime_snapshot)?;
        println!("Runtime model catalog generated");
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slim_snapshot_keeps_runtime_fields_and_tracks_upstream_provenance() {
        let source = br#"{"provider":{"name":"Provider metadata","models":{"model":{"name":"Model","limit":{"context":64,"input":999,"output":16},"tool_call":true,"reasoning":false,"modalities":{"input":["text"]},"status":"active","pricing":{"input":1}},"null_modalities":{"modalities":null}}}}"#;
        let mut metadata = json!({
            "source":"https://models.dev/api.json",
            "license":"MIT",
            "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "trusted_input_sha256": sha256(source),
            "upstream_revision":"cca439751cc0559abd8b39bf8a266d41f49e03f2"
        });

        let snapshot = generate_from_upstream(source, &mut metadata).unwrap();
        let records: Vec<ModelRecord> = serde_json::from_slice(&snapshot).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].provider, "provider");
        assert_eq!(records[0].id, "model");
        assert_eq!(records[0].context_window, Some(64));
        assert_eq!(records[0].max_output_tokens, Some(16));
        assert_eq!(records[0].tool_calling, Some(true));
        assert_eq!(records[0].reasoning, Some(false));
        assert_eq!(records[0].status.as_deref(), Some("active"));
        assert_eq!(records[0].modalities, Some(json!({"input": ["text"]})));
        assert_eq!(records[1].name, "null_modalities");
        assert_eq!(records[1].modalities, Some(Value::Null));
        assert_eq!(
            metadata["sha256"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(metadata["trusted_input_sha256"], sha256(source));
        assert_eq!(metadata["snapshot_sha256"], sha256(&snapshot));
        assert_eq!(metadata["generator_version"], GENERATOR_VERSION);
        assert_eq!(metadata["provider_count"], 1);
        assert_eq!(metadata["model_count"], 2);
        assert!(validate_slim_snapshot(&snapshot, &metadata).is_ok());
        assert_eq!(
            verify_trusted_upstream(source, &snapshot, &metadata)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            verify_trusted_upstream(
                source,
                &snapshot,
                &json!({
                    "trusted_input_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                    "upstream_revision": "cca439751cc0559abd8b39bf8a266d41f49e03f2"
                })
            )
            .unwrap_err(),
            "Trusted upstream input SHA-256 mismatch"
        );
    }

    #[test]
    fn slim_snapshot_rejects_terminal_control_characters() {
        let records = vec![ModelRecord {
            provider: "provider".into(),
            id: "model".into(),
            name: "Bad\u{1b}[2JName".into(),
            context_window: None,
            max_output_tokens: None,
            tool_calling: None,
            reasoning: None,
            modalities: None,
            status: None,
        }];
        let bytes = pretty_json(&records).unwrap();
        let metadata = json!({
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "trusted_input_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "snapshot_sha256": sha256(&bytes),
            "generator_version": GENERATOR_VERSION,
            "provider_count": 1,
            "model_count": 1
        });
        assert_eq!(
            validate_slim_snapshot(&bytes, &metadata).unwrap_err(),
            "Snapshot contains terminal control characters"
        );
    }

    #[test]
    fn changed_upstream_input_clears_the_previous_revision_pin() {
        let mut metadata = json!({
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "trusted_input_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "upstream_revision": "cca439751cc0559abd8b39bf8a266d41f49e03f2"
        });
        generate_from_upstream(br#"{"provider":{"models":{"model":{}}}}"#, &mut metadata).unwrap();
        assert!(metadata.get("upstream_revision").is_none());
    }

    #[test]
    fn catalog_pair_replaces_runtime_files_and_checks_consistency() {
        let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let directory = env::temp_dir().join(format!(
            "krypper-catalog-generator-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let snapshot_path = directory.join("api.json");
        let metadata_path = directory.join("metadata.json");
        fs::write(&snapshot_path, b"old snapshot").unwrap();
        fs::write(&metadata_path, b"old metadata").unwrap();

        let source = br#"{"provider":{"models":{"model":{"name":"Model"}}}}"#;
        let mut metadata = json!({
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });
        let snapshot = generate_from_upstream(source, &mut metadata).unwrap();
        let metadata_bytes = pretty_json(&metadata).unwrap();
        let mut invalid_metadata = metadata.clone();
        invalid_metadata["snapshot_sha256"] = Value::String("wrong".into());
        assert!(write_catalog_files_atomically(
            &snapshot_path,
            &snapshot,
            &metadata_path,
            &pretty_json(&invalid_metadata).unwrap(),
        )
        .is_err());
        assert_eq!(fs::read(&snapshot_path).unwrap(), b"old snapshot");
        assert_eq!(fs::read(&metadata_path).unwrap(), b"old metadata");

        write_catalog_files_atomically(&snapshot_path, &snapshot, &metadata_path, &metadata_bytes)
            .unwrap();
        assert_eq!(fs::read(&snapshot_path).unwrap(), snapshot);
        assert_eq!(fs::read(&metadata_path).unwrap(), metadata_bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn check_rejects_a_changed_slim_snapshot() {
        let metadata = json!({
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "trusted_input_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "snapshot_sha256": "wrong",
            "generator_version": GENERATOR_VERSION,
            "provider_count": 1,
            "model_count": 1
        });
        assert_eq!(
            validate_slim_snapshot(b"[]", &metadata).unwrap_err(),
            "Slim snapshot SHA-256 mismatch"
        );
    }

    #[test]
    fn normalization_rejects_empty_provider_and_model_ids() {
        let empty_provider = json!({ "": { "models": { "model": {} } } });
        assert!(normalize(&empty_provider).is_err());

        let empty_model = json!({ "openai": { "models": { "": {} } } });
        assert!(normalize(&empty_model).is_err());
    }
}
