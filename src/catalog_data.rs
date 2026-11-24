//! Offline, vendored models.dev catalog. Informational only: not a provider allowlist.
//! User configuration and provider routing remain authoritative.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Match a built-in provider selector to catalog IDs from its trusted
/// descriptor. Catalog records remain descriptive only.
pub fn provider_matches(selector: &str, catalog_provider: &str) -> bool {
    crate::brain::provider_catalog_matches(selector, catalog_provider)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Parse the checked-in slim snapshot; no network access occurs.
pub fn models() -> Result<Vec<ModelSpec>, serde_json::Error> {
    let records: Vec<Value> =
        serde_json::from_str(include_str!("../snapshots/models-dev/api.json"))?;
    parse_records(records)
}

fn parse_records(records: Vec<Value>) -> Result<Vec<ModelSpec>, serde_json::Error> {
    let mut result: Vec<ModelSpec> = records
        .into_iter()
        .map(|record| {
            let provider = record
                .get("provider")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    <serde_json::Error as serde::de::Error>::custom(
                        "catalog record requires a non-empty string `provider`",
                    )
                })?
                .to_owned();
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    <serde_json::Error as serde::de::Error>::custom(
                        "catalog record requires a non-empty string `id`",
                    )
                })?
                .to_owned();
            Ok(ModelSpec {
                provider,
                name: record
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                context_window: record.get("context_window").and_then(Value::as_u64),
                max_output_tokens: record.get("max_output_tokens").and_then(Value::as_u64),
                tool_calling: record.get("tool_calling").and_then(Value::as_bool),
                reasoning: record.get("reasoning").and_then(Value::as_bool),
                status: record
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                modalities: record.get("modalities").cloned(),
                id,
            })
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    result.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    Ok(result)
}

/// Unknown models are not rejected; the catalog is descriptive, not restrictive.
pub fn lookup(provider: &str, model_id: &str) -> Result<Option<ModelSpec>, serde_json::Error> {
    Ok(models()?
        .into_iter()
        .find(|m| provider_matches(provider, &m.provider) && m.id == model_id))
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

    #[test]
    fn malformed_catalog_identity_is_rejected() {
        let record = serde_json::json!({ "provider": "openai" });
        assert!(parse_records(vec![record]).is_err());
    }

    #[test]
    fn cloudflare_selectors_match_workers_ai_and_gateway_catalogs() {
        assert!(provider_matches("cloudflare", "cloudflare-workers-ai"));
        assert!(!provider_matches("cloudflare", "cloudflare-ai-gateway"));
        assert!(provider_matches(
            "cloudflare-ai-gateway",
            "cloudflare-ai-gateway"
        ));
        assert!(provider_matches(
            "cloudflare-ai-gateway",
            "cloudflare-workers-ai"
        ));
        assert!(!provider_matches("cloudflare-ai-gateway", "openai"));
    }

    #[test]
    fn gemini_selector_matches_google_catalog_records() {
        assert!(provider_matches("gemini", "google"));
        assert!(!provider_matches("gemini", "google-vertex"));
    }
}
