//! Deterministic offline normalizer for the pinned models.dev snapshot.
//! Run with `cargo run --example generate_model_catalog -- [--check]`.
use ring::digest::{digest, SHA256};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize)]
struct ModelRecord<'a> {
    provider: &'a str,
    id: &'a str,
    name: &'a str,
    context_window: Option<&'a Value>,
    max_output_tokens: Option<&'a Value>,
    tool_calling: Option<&'a Value>,
    reasoning: Option<&'a Value>,
    modalities: Option<&'a Value>,
    status: Option<&'a Value>,
}

/// Normalize without network access or changes to the runtime model router.
fn normalize(raw: &Value) -> Result<Vec<Value>, String> {
    let providers = raw.as_object().ok_or("snapshot root must be an object")?;
    let mut records = Vec::new();
    for (provider_id, provider) in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, model) in models {
            let Some(model) = model.as_object() else {
                continue;
            };
            let limits = model.get("limit").and_then(Value::as_object);
            let record = ModelRecord {
                provider: provider_id,
                id: model_id,
                name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(model_id),
                context_window: limits.and_then(|limits| limits.get("context")),
                max_output_tokens: limits.and_then(|limits| limits.get("output")),
                tool_calling: model.get("tool_call"),
                reasoning: model.get("reasoning"),
                modalities: model.get("modalities"),
                status: model.get("status"),
            };
            records.push(serde_json::to_value(record).map_err(|error| error.to_string())?);
        }
    }
    if records.is_empty() {
        return Err("No models found; verify upstream schema".into());
    }
    Ok(records)
}

fn generate(source: &[u8], metadata: &Value) -> Result<Vec<u8>, String> {
    let expected_hash = metadata
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or("missing snapshot sha256")?;
    let actual_hash = digest(&SHA256, source);
    let actual_hash = actual_hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual_hash != expected_hash {
        return Err("Snapshot SHA-256 mismatch".into());
    }
    let raw: Value = serde_json::from_slice(source).map_err(|error| error.to_string())?;
    let provider_count = metadata
        .get("provider_count")
        .and_then(Value::as_u64)
        .ok_or("missing provider_count")?;
    if raw.as_object().map(|providers| providers.len() as u64) != Some(provider_count) {
        return Err("Snapshot provider count mismatch".into());
    }
    let records = normalize(&raw)?;
    // Serialize structs rather than JSON maps to preserve the Python generator's
    // documented key order. The output is always UTF-8 with a trailing newline.
    let mut output = String::from("[\n");
    for (index, record) in records.iter().enumerate() {
        let provider = record["provider"].as_str().ok_or("invalid provider")?;
        let id = record["id"].as_str().ok_or("invalid model id")?;
        let model = raw
            .get(provider)
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(id))
            .ok_or("missing model")?;
        let limits = model.get("limit").and_then(Value::as_object);
        let row = ModelRecord {
            provider,
            id,
            name: model
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id),
            context_window: limits.and_then(|limits| limits.get("context")),
            max_output_tokens: limits.and_then(|limits| limits.get("output")),
            tool_calling: model.get("tool_call"),
            reasoning: model.get("reasoning"),
            modalities: model.get("modalities"),
            status: model.get("status"),
        };
        let pretty = serde_json::to_string_pretty(&row).map_err(|error| error.to_string())?;
        for line in pretty.lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
        if index + 1 != records.len() {
            // Move comma onto the closing brace line, matching Python json.dumps.
            output.truncate(output.len() - 1);
            output.push_str(",\n");
        }
    }
    output.push_str("]\n");
    Ok(output.into_bytes())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let check = match args.as_slice() {
        [] => false,
        [arg] if arg == "--check" => true,
        _ => return Err("usage: cargo run --example generate_model_catalog -- [--check]".into()),
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source =
        fs::read(root.join("snapshots/models-dev/api.json")).map_err(|error| error.to_string())?;
    let metadata: Value = serde_json::from_slice(
        &fs::read(root.join("snapshots/models-dev/metadata.json"))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let output = generate(&source, &metadata)?;
    let target = root.join("src/catalog_data.json");
    if check {
        if fs::read(&target).map_err(|error| error.to_string())? != output {
            return Err(
                "Catalog out of date: rerun cargo run --example generate_model_catalog".into(),
            );
        }
        println!("Catalog verified");
    } else {
        fs::write(target, output).map_err(|error| error.to_string())?;
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
    #[test]
    fn rejects_invalid_snapshot_hash() {
        let metadata = serde_json::json!({"sha256": "bad", "provider_count": 1});
        assert_eq!(
            generate(b"{}", &metadata).unwrap_err(),
            "Snapshot SHA-256 mismatch"
        );
    }
}
