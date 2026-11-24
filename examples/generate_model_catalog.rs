//! Deterministic offline normalizer for the pinned models.dev snapshot.
//! Default mode writes the optional normalized JSON artifact.
//! Use --check to validate it, or --from-upstream <path> to refresh the slim snapshot.
use krypper::catalog_sources::{CatalogSource, ModelsDevSource};
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

// Increment when the normalized record schema or field mapping changes.
const GENERATOR_VERSION: &str = "1";

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

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut output = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    output.push(b'\n');
    Ok(output)
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
    fields.insert("sha256".into(), Value::String(sha256(source)));
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
    metadata
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or("missing upstream sha256")?;
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

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (check, upstream_path) = match args.as_slice() {
        [] => (false, None),
        [arg] if arg == "--check" => (true, None),
        [flag, path] if flag == "--from-upstream" => (false, Some(PathBuf::from(path))),
        _ => {
            return Err(
                "usage: cargo run --example generate_model_catalog -- [--check | --from-upstream <path>]"
                    .into(),
            )
        }
    };

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let snapshot_path = root.join("snapshots/models-dev/api.json");
    let metadata_path = root.join("snapshots/models-dev/metadata.json");
    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&metadata_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;

    if let Some(upstream_path) = upstream_path {
        let source = fs::read(upstream_path).map_err(|error| error.to_string())?;
        let source_hash = sha256(&source);
        if metadata.get("snapshot_sha256").and_then(Value::as_str) == Some(source_hash.as_str()) {
            return Err("input is the slim snapshot; provide original upstream bytes".into());
        }
        let snapshot = generate_from_upstream(&source, &mut metadata)?;
        let metadata = pretty_json(&metadata)?;
        fs::write(&snapshot_path, snapshot).map_err(|error| error.to_string())?;
        fs::write(&metadata_path, metadata).map_err(|error| error.to_string())?;
        println!("Slim model snapshot generated");
        return Ok(());
    }

    let source = fs::read(&snapshot_path).map_err(|error| error.to_string())?;
    let records = validate_slim_snapshot(&source, &metadata)?;
    let target = root.join("src/catalog_data.json");
    if check {
        verify_runtime(&records)?;
        println!("Verified {} catalog records", records.len());
    } else {
        fs::write(target, pretty_json(&records)?).map_err(|error| error.to_string())?;
        println!("Catalog generated");
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
        let mut metadata = json!({"source":"https://models.dev/api.json","license":"MIT"});

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
        assert_eq!(metadata["sha256"], sha256(source));
        assert_eq!(metadata["snapshot_sha256"], sha256(&snapshot));
        assert_eq!(metadata["generator_version"], GENERATOR_VERSION);
        assert_eq!(metadata["provider_count"], 1);
        assert_eq!(metadata["model_count"], 2);
        assert!(validate_slim_snapshot(&snapshot, &metadata).is_ok());
    }

    #[test]
    fn check_rejects_a_changed_slim_snapshot() {
        let metadata = json!({
            "sha256": "recorded-upstream-hash",
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
