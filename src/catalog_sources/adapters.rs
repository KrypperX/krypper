use super::*;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

#[derive(Debug, Default, Clone, Copy)]
pub struct ModelsDevSource;

impl CatalogSource for ModelsDevSource {
    fn descriptor(&self) -> CatalogSourceDescriptor {
        CatalogSourceId::models_dev()
            .builtin_descriptor()
            .expect("built-in models.dev descriptor")
    }

    fn parse(
        &self,
        bytes: &[u8],
        retrieved_at_unix: u64,
    ) -> Result<CatalogContribution, CatalogError> {
        let descriptor = self.descriptor();
        validate_source_bytes(bytes, &descriptor)?;
        let raw = parse_json_without_duplicate_keys(bytes)
            .map_err(|()| CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        let raw_providers = raw
            .as_object()
            .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        let mut providers = Vec::new();
        let mut models = Vec::new();

        for (provider_id, provider) in raw_providers {
            if provider_id.is_empty() {
                return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone()));
            }
            let provider = provider
                .as_object()
                .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
            let Some(raw_models) = provider.get("models") else {
                continue;
            };
            let raw_models = raw_models
                .as_object()
                .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
            providers.push(CatalogProviderMetadata {
                id: provider_id.clone(),
                name: provider
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(provider_id)
                    .to_owned(),
                documentation_url: provider
                    .get("doc")
                    .and_then(Value::as_str)
                    .filter(|url| safe_https_url(url))
                    .map(str::to_owned),
            });
            for (model_id, raw_model) in raw_models {
                if model_id.is_empty() {
                    return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone()));
                }
                let model = raw_model
                    .as_object()
                    .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
                let limits = model.get("limit").and_then(Value::as_object);
                let cost = model.get("cost").and_then(Value::as_object);
                let pricing = cost.map(|cost| ModelPricing {
                    input_usd_per_million_tokens: cost.get("input").and_then(parse_number),
                    output_usd_per_million_tokens: cost.get("output").and_then(parse_number),
                });
                let pricing = pricing.filter(|price| {
                    price.input_usd_per_million_tokens.is_some()
                        || price.output_usd_per_million_tokens.is_some()
                });
                let features = ModelFeatures {
                    supports_attachments: model.get("attachment").and_then(Value::as_bool),
                    structured_output: model.get("structured_output").and_then(Value::as_bool),
                    ..ModelFeatures::default()
                };
                models.push(CatalogModel {
                    model: ModelSpec {
                        provider: provider_id.clone(),
                        id: model_id.clone(),
                        name: model
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .unwrap_or(model_id)
                            .to_owned(),
                        context_window: limits
                            .and_then(|limits| limits.get("context"))
                            .and_then(Value::as_u64),
                        max_output_tokens: limits
                            .and_then(|limits| limits.get("output"))
                            .and_then(Value::as_u64),
                        tool_calling: model.get("tool_call").and_then(Value::as_bool),
                        reasoning: model.get("reasoning").and_then(Value::as_bool),
                        status: model
                            .get("status")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        modalities: model.get("modalities").cloned(),
                    },
                    pricing,
                    features,
                    sources: vec![descriptor.id.clone()],
                });
            }
        }
        if models.is_empty() {
            return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone()));
        }
        let contribution = CatalogContribution {
            provenance: provenance(descriptor, bytes, retrieved_at_unix),
            providers,
            models,
        };
        validate_contribution(&contribution)?;
        sort_contribution(contribution)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CloudflareWorkersAiSource;

impl CatalogSource for CloudflareWorkersAiSource {
    fn descriptor(&self) -> CatalogSourceDescriptor {
        CatalogSourceId::cloudflare_workers_ai()
            .builtin_descriptor()
            .expect("built-in Cloudflare Workers AI descriptor")
    }

    fn parse(
        &self,
        bytes: &[u8],
        retrieved_at_unix: u64,
    ) -> Result<CatalogContribution, CatalogError> {
        let descriptor = self.descriptor();
        validate_source_bytes(bytes, &descriptor)?;
        let raw = parse_json_without_duplicate_keys(bytes)
            .map_err(|()| CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        let raw_models = raw
            .get("models")
            .and_then(Value::as_array)
            .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        let mut models = Vec::with_capacity(raw_models.len());
        for raw_model in raw_models {
            let id = raw_model
                .get("name")
                .and_then(Value::as_str)
                .filter(|id| {
                    id.strip_prefix("@cf/")
                        .is_some_and(|suffix| !suffix.is_empty())
                })
                .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
            let properties = match raw_model.get("properties") {
                Some(Value::Array(properties)) => properties.as_slice(),
                None | Some(Value::Null) => &[],
                Some(_) => return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone())),
            };
            if has_duplicate_properties(properties) {
                return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone()));
            }
            let property = |id: &str| {
                properties
                    .iter()
                    .find(|property| {
                        property.get("property_id").and_then(Value::as_str) == Some(id)
                    })
                    .and_then(|property| property.get("value"))
            };
            let context_window = property("context_window").and_then(Value::as_u64);
            let tool_calling = property("function_calling").and_then(Value::as_bool);
            let reasoning = property("reasoning").and_then(Value::as_bool);
            let vision = property("vision").and_then(Value::as_bool);
            let status = raw_model
                .get("deprecated")
                .and_then(Value::as_bool)
                .filter(|deprecated| *deprecated)
                .map(|_| "deprecated".to_owned());
            models.push(CatalogModel {
                model: ModelSpec {
                    provider: "cloudflare-workers-ai".to_owned(),
                    id: id.to_owned(),
                    name: id.to_owned(),
                    context_window,
                    max_output_tokens: None,
                    tool_calling,
                    reasoning,
                    status,
                    modalities: None,
                },
                pricing: None,
                features: ModelFeatures {
                    structured_output: None,
                    supports_attachments: None,
                    vision,
                    requires_workers_paid: property("require_workers_paid")
                        .and_then(Value::as_bool),
                    task: raw_model.get("task").and_then(task_name).map(str::to_owned),
                    planned_deprecation_date: property("planned_deprecation_date")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                sources: vec![descriptor.id.clone()],
            });
        }
        if models.is_empty() {
            return Err(CatalogError::InvalidSourcePayload(descriptor.id.clone()));
        }
        let contribution = CatalogContribution {
            provenance: provenance(descriptor, bytes, retrieved_at_unix),
            providers: vec![CatalogProviderMetadata {
                id: "cloudflare-workers-ai".to_owned(),
                name: "Cloudflare Workers AI".to_owned(),
                documentation_url: Some(
                    "https://developers.cloudflare.com/workers-ai/models/".to_owned(),
                ),
            }],
            models,
        };
        validate_contribution(&contribution)?;
        sort_contribution(contribution)
    }
}

fn task_name(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        value
            .get("name")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
    })
}

struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueJsonVisitor;

        impl<'de> Visitor<'de> for UniqueJsonVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Number::from_f64(value)
                    .map(Value::Number)
                    .map(UniqueJson)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(UniqueJson(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }

            fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = mapping.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    let UniqueJson(value) = mapping.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

pub(super) fn parse_json_without_duplicate_keys(bytes: &[u8]) -> Result<Value, ()> {
    serde_json::from_slice::<UniqueJson>(bytes)
        .map(|UniqueJson(value)| value)
        .map_err(|_| ())
}

fn has_duplicate_properties(properties: &[Value]) -> bool {
    let mut ids = BTreeSet::new();
    properties.iter().any(|property| {
        property
            .get("property_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !ids.insert(id))
    })
}

fn parse_number(value: &Value) -> Option<Number> {
    let number = value
        .as_number()
        .cloned()
        .or_else(|| value.as_str().and_then(|text| text.parse::<Number>().ok()))?;
    number
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|_| number)
}

pub(super) fn validate_source_bytes(
    bytes: &[u8],
    descriptor: &CatalogSourceDescriptor,
) -> Result<(), CatalogError> {
    if bytes.len() > descriptor.max_bytes {
        return Err(CatalogError::PayloadTooLarge);
    }
    Ok(())
}

pub(super) fn provenance(
    descriptor: CatalogSourceDescriptor,
    bytes: &[u8],
    retrieved_at_unix: u64,
) -> CatalogProvenance {
    CatalogProvenance {
        source: descriptor.id,
        url: descriptor.url.to_owned(),
        license: descriptor.license.to_owned(),
        license_url: descriptor.license_url.to_owned(),
        source_sha256: sha256(bytes),
        retrieved_at_unix,
        schema_version: descriptor.schema_version,
    }
}
