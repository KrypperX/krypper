use super::*;
use serde_json::Value;
use std::cell::Cell;
use std::fs;

const MODELS_DEV_FIXTURE: &[u8] = br#"{
      "cloudflare-workers-ai": {
        "name": "Cloudflare Workers AI",
        "doc": "https://developers.cloudflare.com/workers-ai/",
        "future_provider_field": {"preserved_upstream": true},
        "models": {
          "@cf/test/model": {
            "name": "Workers Test Model",
            "limit": {"context": 4096, "output": 1024},
            "tool_call": true,
            "reasoning": false,
            "attachment": true,
            "structured_output": true,
            "cost": {"input": 0.2, "output": "0.4"},
            "future_model_field": "ignored"
          }
        }
      }
    }"#;

const CLOUDFLARE_FIXTURE: &[u8] = br#"{
      "models": [{
        "name": "@cf/test/model",
        "task": {"name": "Text Generation", "description": "ignored"},
        "properties": [
          {"property_id": "context_window", "value": 8192},
          {"property_id": "function_calling", "value": false},
          {"property_id": "reasoning", "value": true},
          {"property_id": "vision", "value": true},
          {"property_id": "require_workers_paid", "value": true},
          {"property_id": "planned_deprecation_date", "value": "2027-01-01"},
          {"property_id": "future_property", "value": "ignored"}
        ],
        "deprecated": true,
        "future_record_field": {"ignored": true}
      }]
    }"#;

struct TestDirectory(crate::test_support::TempDir);

impl TestDirectory {
    fn new() -> Self {
        Self(crate::test_support::TempDir::with_prefix(
            "krypper-catalog-source-test-",
        ))
    }
}

struct FixtureTransport {
    response: Result<Vec<u8>, CatalogError>,
    calls: Cell<usize>,
}

struct OversizedTransport;

impl CatalogTransport for OversizedTransport {
    fn get(
        &self,
        _url: &str,
        max_bytes: usize,
        _timeout: Duration,
    ) -> Result<Vec<u8>, CatalogError> {
        Ok(vec![0; max_bytes + 1])
    }
}

impl FixtureTransport {
    fn response(bytes: &[u8]) -> Self {
        Self {
            response: Ok(bytes.to_vec()),
            calls: Cell::new(0),
        }
    }

    fn unavailable() -> Self {
        Self {
            response: Err(CatalogError::SourceUnavailable),
            calls: Cell::new(0),
        }
    }
}

impl CatalogTransport for FixtureTransport {
    fn get(
        &self,
        _url: &str,
        max_bytes: usize,
        _timeout: Duration,
    ) -> Result<Vec<u8>, CatalogError> {
        self.calls.set(self.calls.get() + 1);
        let response = self.response.clone()?;
        if response.len() > max_bytes {
            return Err(CatalogError::PayloadTooLarge);
        }
        Ok(response)
    }
}

struct TestSource;

impl CatalogSource for TestSource {
    fn descriptor(&self) -> CatalogSourceDescriptor {
        CatalogSourceDescriptor {
            id: CatalogSourceId::new("fixture-source").unwrap(),
            url: "https://catalog.example/models",
            license: "MIT",
            license_url: "https://catalog.example/license",
            schema_version: 1,
            max_bytes: 1024,
        }
    }

    fn parse(
        &self,
        bytes: &[u8],
        retrieved_at_unix: u64,
    ) -> Result<CatalogContribution, CatalogError> {
        let descriptor = self.descriptor();
        super::adapters::validate_source_bytes(bytes, &descriptor)?;
        let raw = super::adapters::parse_json_without_duplicate_keys(bytes)
            .map_err(|()| CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(CatalogError::InvalidSourcePayload(descriptor.id.clone()))?;
        sort_contribution(CatalogContribution {
            provenance: super::adapters::provenance(descriptor.clone(), bytes, retrieved_at_unix),
            providers: vec![],
            models: vec![CatalogModel {
                model: ModelSpec {
                    provider: "fixture-provider".to_owned(),
                    id: id.to_owned(),
                    name: id.to_owned(),
                    context_window: None,
                    max_output_tokens: None,
                    tool_calling: None,
                    reasoning: None,
                    status: None,
                    modalities: None,
                },
                pricing: None,
                features: ModelFeatures::default(),
                sources: vec![descriptor.id],
            }],
        })
    }
}

#[test]
fn models_dev_source_maps_provider_metadata_pricing_and_features() {
    let contribution = ModelsDevSource
        .parse(MODELS_DEV_FIXTURE, 1_800_000_000)
        .unwrap();
    let model = &contribution.models[0];
    assert_eq!(
        contribution.provenance.source,
        CatalogSourceId::models_dev()
    );
    assert_eq!(contribution.provenance.license, "MIT");
    assert_eq!(contribution.providers[0].name, "Cloudflare Workers AI");
    assert_eq!(
        contribution.providers[0].documentation_url.as_deref(),
        Some("https://developers.cloudflare.com/workers-ai/")
    );
    assert_eq!(model.model.id, "@cf/test/model");
    assert_eq!(model.model.context_window, Some(4096));
    assert_eq!(
        model
            .pricing
            .as_ref()
            .unwrap()
            .input_usd_per_million_tokens
            .as_ref()
            .unwrap()
            .to_string(),
        "0.2"
    );
    assert_eq!(
        model
            .pricing
            .as_ref()
            .unwrap()
            .output_usd_per_million_tokens
            .as_ref()
            .unwrap()
            .to_string(),
        "0.4"
    );
    assert_eq!(model.features.supports_attachments, Some(true));
    assert_eq!(model.features.structured_output, Some(true));
}

#[test]
fn catalog_display_text_strips_terminal_controls_and_ids_reject_them() {
    let bytes = br#"{"provider":{"name":"Provider\u001b[2J","models":{"model":{"name":"Model\u001b[31m name","status":"deprecated\u001b[0m"}}}}"#;
    let contribution = ModelsDevSource.parse(bytes, 10).unwrap();
    assert_eq!(contribution.providers[0].name, "Provider[2J");
    assert_eq!(contribution.models[0].model.name, "Model[31m name");
    assert_eq!(
        contribution.models[0].model.status.as_deref(),
        Some("deprecated[0m")
    );

    let controlled_provider_id = br#"{"provider\u001b":{"models":{"model":{}}}}"#;
    let controlled_model_id = br#"{"provider":{"models":{"model\u001b":{}}}}"#;
    assert!(ModelsDevSource.parse(controlled_provider_id, 10).is_err());
    assert!(ModelsDevSource.parse(controlled_model_id, 10).is_err());
}

#[test]
fn cloudflare_catalog_display_text_strips_terminal_controls() {
    let contribution = CloudflareWorkersAiSource
        .parse(
            br#"{"models":[{"name":"@cf/text","task":"Text\u001b[2J Generation","properties":[{"property_id":"planned_deprecation_date","value":"2027-01-01\u001b[0m"}]}]}"#,
            10,
        )
        .unwrap();
    assert_eq!(
        contribution.models[0].features.task.as_deref(),
        Some("Text[2J Generation")
    );
    assert_eq!(
        contribution.models[0]
            .features
            .planned_deprecation_date
            .as_deref(),
        Some("2027-01-01[0m")
    );
}

#[test]
fn cloudflare_source_preserves_exact_workers_id_and_ignores_unknown_fields() {
    let contribution = CloudflareWorkersAiSource
        .parse(CLOUDFLARE_FIXTURE, 1_800_000_001)
        .unwrap();
    assert_eq!(
        contribution.provenance.source,
        CatalogSourceId::cloudflare_workers_ai()
    );
    assert_eq!(contribution.provenance.license, "CC-BY-4.0");
    let model = &contribution.models[0];
    assert_eq!(model.model.provider, "cloudflare-workers-ai");
    assert_eq!(model.model.id, "@cf/test/model");
    assert_eq!(model.model.context_window, Some(8192));
    assert_eq!(model.model.tool_calling, Some(false));
    assert_eq!(model.model.status.as_deref(), Some("deprecated"));
    assert_eq!(model.features.task.as_deref(), Some("Text Generation"));
    assert_eq!(model.features.vision, Some(true));
    assert_eq!(model.features.supports_attachments, None);
    assert_eq!(model.model.modalities, None);
    assert_eq!(model.features.requires_workers_paid, Some(true));
    assert_eq!(
        model.features.planned_deprecation_date.as_deref(),
        Some("2027-01-01")
    );
}

#[test]
fn cloudflare_vision_evidence_does_not_invent_modalities_or_attachments() {
    let contribution = CloudflareWorkersAiSource
        .parse(
            br#"{"models":[{"name":"@cf/text","properties":[{"property_id":"vision","value":false}]}]}"#,
            10,
        )
        .unwrap();
    assert_eq!(contribution.models[0].features.vision, Some(false));
    assert_eq!(contribution.models[0].model.modalities, None);
    assert_eq!(contribution.models[0].features.supports_attachments, None);
}

#[test]
fn models_dev_rejects_malformed_provider_and_model_records() {
    for bytes in [
        br#"{"provider":null}"#.as_slice(),
        br#"{"provider":{"models":[]}}"#.as_slice(),
        br#"{"provider":{"models":{"model":null}}}"#.as_slice(),
    ] {
        assert_eq!(
            ModelsDevSource.parse(bytes, 10).unwrap_err(),
            CatalogError::InvalidSourcePayload(CatalogSourceId::models_dev())
        );
    }
}

#[test]
fn source_ids_reject_invalid_serialized_values() {
    for id in ["", "Upper", "has space", "-prefix", "suffix-"] {
        let value = serde_json::to_string(id).unwrap();
        assert!(serde_json::from_str::<CatalogSourceId>(&value).is_err());
    }
}

#[test]
fn merge_is_order_independent_and_cloudflare_overrides_its_model_metadata() {
    let models_dev = ModelsDevSource
        .parse(MODELS_DEV_FIXTURE, 1_800_000_000)
        .unwrap();
    let cloudflare = CloudflareWorkersAiSource
        .parse(CLOUDFLARE_FIXTURE, 1_800_000_001)
        .unwrap();
    let forward = merge_contributions(&[models_dev.clone(), cloudflare.clone()]).unwrap();
    let reverse = merge_contributions(&[cloudflare, models_dev]).unwrap();
    assert_eq!(forward, reverse);
    let model = &forward.models[0];
    assert_eq!(model.model.context_window, Some(8192));
    assert_eq!(model.model.tool_calling, Some(false));
    assert_eq!(
        model
            .pricing
            .as_ref()
            .unwrap()
            .input_usd_per_million_tokens
            .as_ref()
            .unwrap()
            .to_string(),
        "0.2"
    );
    assert_eq!(
        model.sources,
        vec![
            CatalogSourceId::cloudflare_workers_ai(),
            CatalogSourceId::models_dev()
        ]
    );
}

#[test]
fn duplicate_model_records_fail_closed() {
    let mut contribution = ModelsDevSource.parse(MODELS_DEV_FIXTURE, 10).unwrap();
    contribution.models.push(contribution.models[0].clone());
    assert_eq!(
        merge_contributions(&[contribution]).unwrap_err(),
        CatalogError::DuplicateModel
    );

    let duplicate_json =
        br#"{"provider":{"models":{"same":{"name":"first"},"same":{"name":"second"}}}"#;
    assert_eq!(
        ModelsDevSource.parse(duplicate_json, 10).unwrap_err(),
        CatalogError::InvalidSourcePayload(CatalogSourceId::models_dev())
    );
}

#[test]
fn duplicate_source_contributions_fail_closed() {
    let first = ModelsDevSource.parse(MODELS_DEV_FIXTURE, 10).unwrap();
    let second = ModelsDevSource.parse(MODELS_DEV_FIXTURE, 11).unwrap();
    assert_eq!(
        merge_contributions(&[first, second]).unwrap_err(),
        CatalogError::DuplicateSource
    );
}

#[test]
fn invalid_prices_fail_closed() {
    let mut contribution = ModelsDevSource.parse(MODELS_DEV_FIXTURE, 10).unwrap();
    contribution.models[0].pricing = Some(ModelPricing {
        input_usd_per_million_tokens: Some(serde_json::json!(-1).as_number().unwrap().clone()),
        output_usd_per_million_tokens: None,
    });
    assert_eq!(
        merge_contributions(&[contribution]).unwrap_err(),
        CatalogError::InvalidSourcePayload(CatalogSourceId::models_dev())
    );
}

#[test]
fn explicit_refresh_writes_a_verified_cache() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    let source = ModelsDevSource;
    let transport = FixtureTransport::response(MODELS_DEV_FIXTURE);
    let result = refresh_catalog(&path, &[&source], &transport, 1_800_000_000).unwrap();
    assert!(result.updated);
    assert!(!result.used_last_known_good);
    assert_eq!(transport.calls.get(), 1);
    assert_eq!(
        read_catalog_cache(&path)
            .unwrap()
            .unwrap()
            .merged()
            .unwrap()
            .models
            .len(),
        1
    );
}

#[test]
fn source_trait_accepts_a_new_typed_source_id_without_changing_the_registry() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    let result =
        import_catalog_payload(&path, &TestSource, br#"{"id":"custom-model"}"#, 55).unwrap();
    assert_eq!(
        result.snapshot.contributions[0].provenance.source.as_str(),
        "fixture-source"
    );
    assert_eq!(
        result.snapshot.merged().unwrap().models[0].model.id,
        "custom-model"
    );
}

#[test]
fn core_enforces_source_size_limits_even_if_transport_or_adapter_does_not() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    let source = TestSource;
    let oversized = vec![b' '; source.descriptor().max_bytes + 1];
    assert_eq!(
        import_catalog_payload(&path, &source, &oversized, 55).unwrap_err(),
        CatalogError::PayloadTooLarge
    );
    assert_eq!(
        refresh_catalog(&path, &[&source], &OversizedTransport, 55).unwrap_err(),
        CatalogError::PayloadTooLarge
    );
    assert!(!path.exists());
}

#[test]
fn unavailable_source_returns_stale_last_known_good_without_replacing_it() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    import_catalog_payload(&path, &ModelsDevSource, MODELS_DEV_FIXTURE, 90).unwrap();
    let source = CloudflareWorkersAiSource;
    import_catalog_payload(&path, &source, CLOUDFLARE_FIXTURE, 100).unwrap();
    let original = fs::read(&path).unwrap();
    let transport = FixtureTransport::unavailable();
    let result = refresh_catalog(
        &path,
        &[&source],
        &transport,
        100 + MAX_CATALOG_CACHE_AGE_SECONDS + 1,
    )
    .unwrap();
    assert!(!result.updated);
    assert!(result.used_last_known_good);
    assert!(result.cache_is_stale);
    assert_eq!(
        result.failed_source,
        Some(CatalogSourceId::cloudflare_workers_ai())
    );
    assert_eq!(result.source_error, Some(CatalogError::SourceUnavailable));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn corrupt_cache_and_invalid_import_do_not_replace_last_known_good() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    fs::write(&path, b"{invalid\n").unwrap();
    assert_eq!(
        read_catalog_cache(&path).unwrap_err(),
        CatalogError::InvalidCache
    );

    let source = ModelsDevSource;
    import_catalog_payload(&path, &source, MODELS_DEV_FIXTURE, 10).unwrap();
    let original = fs::read(&path).unwrap();
    assert!(import_catalog_payload(&path, &source, b"{invalid", 11).is_err());
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn cache_rejects_changed_payload_hashes_and_unknown_schema_versions() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    import_catalog_payload(&path, &ModelsDevSource, MODELS_DEV_FIXTURE, 1_800_000_000).unwrap();

    let mut cache: super::cache::CatalogCacheFile =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cache.snapshot.contributions[0].models[0].model.name = "tampered".to_owned();
    fs::write(&path, super::cache::cache_file_bytes(&cache).unwrap()).unwrap();
    assert_eq!(
        read_catalog_cache(&path).unwrap_err(),
        CatalogError::CacheHashMismatch
    );

    cache.schema_version = CATALOG_CACHE_SCHEMA_VERSION + 1;
    cache.snapshot.schema_version = CATALOG_CACHE_SCHEMA_VERSION + 1;
    fs::write(&path, super::cache::cache_file_bytes(&cache).unwrap()).unwrap();
    assert_eq!(
        read_catalog_cache(&path).unwrap_err(),
        CatalogError::UnsupportedCacheVersion
    );
}

#[test]
fn cache_rejects_unknown_fields() {
    let directory = TestDirectory::new();
    let path = directory.0.path().join("catalog.json");
    import_catalog_payload(&path, &ModelsDevSource, MODELS_DEV_FIXTURE, 10).unwrap();
    let mut cache: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cache
        .as_object_mut()
        .unwrap()
        .insert("future_cache_field".to_owned(), serde_json::json!(true));
    fs::write(&path, serde_json::to_vec_pretty(&cache).unwrap()).unwrap();
    assert_eq!(
        read_catalog_cache(&path).unwrap_err(),
        CatalogError::InvalidCache
    );
}

#[test]
fn ordinary_catalog_lookup_does_not_use_a_transport() {
    let transport = FixtureTransport::unavailable();
    assert!(!crate::catalog_data::models().unwrap().is_empty());
    assert_eq!(transport.calls.get(), 0);
}
