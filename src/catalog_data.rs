//! Offline, vendored models.dev catalog. Informational only: not a provider allowlist.
//! User configuration and provider routing remain authoritative.
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub tool_calling: Option<bool>,
    pub reasoning: Option<bool>,
    pub status: Option<String>,
    pub modalities: Option<Value>,
}

/// Parse the checked-in snapshot; no network access occurs.
pub fn models() -> Result<Vec<ModelSpec>, serde_json::Error> {
    let root: Value = serde_json::from_str(include_str!("../snapshots/models-dev/api.json"))?;
    let mut result = Vec::new();
    if let Some(providers) = root.as_object() {
        for (provider_id, provider) in providers {
            if let Some(entries) = provider.get("models").and_then(Value::as_object) {
                for (model_id, model) in entries {
                    let limits = model.get("limit");
                    result.push(ModelSpec {
                        provider: provider_id.clone(),
                        id: model_id.clone(),
                        name: model
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(model_id)
                            .to_owned(),
                        context_window: limits
                            .and_then(|v| v.get("context"))
                            .and_then(Value::as_u64),
                        max_output_tokens: limits
                            .and_then(|v| v.get("output"))
                            .and_then(Value::as_u64),
                        tool_calling: model.get("tool_call").and_then(Value::as_bool),
                        reasoning: model.get("reasoning").and_then(Value::as_bool),
                        status: model
                            .get("status")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        modalities: model.get("modalities").cloned(),
                    });
                }
            }
        }
    }
    result.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    Ok(result)
}

/// Unknown models are not rejected; the catalog is descriptive, not restrictive.
pub fn lookup(provider: &str, model_id: &str) -> Result<Option<ModelSpec>, serde_json::Error> {
    Ok(models()?
        .into_iter()
        .find(|m| m.provider == provider && m.id == model_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_has_models() {
        let all = models().expect("valid vendored JSON");
        assert!(!all.is_empty());
        assert!(all
            .windows(2)
            .all(|w| (&w[0].provider, &w[0].id) <= (&w[1].provider, &w[1].id)));
    }
    #[test]
    fn unknown_model_is_not_an_error() {
        assert!(lookup("unknown-provider", "custom-model")
            .unwrap()
            .is_none());
    }
}
