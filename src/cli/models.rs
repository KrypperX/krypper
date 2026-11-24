//! Read-only CLI for the offline catalog and its explicitly refreshed cache.

use super::model_catalog::{
    cache_path_for_user_settings, catalog_from_snapshot, load_catalog, refresh_user_catalog,
    CatalogOrigin,
};
use super::settings::user_settings_paths;
use std::ffi::OsString;
use std::io::{self, Write};

pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> io::Result<i32> {
    let mut provider: Option<&str> = None;
    let mut id: Option<&str> = None;
    let mut json = false;
    let mut provenance = false;
    let mut refresh = false;
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].to_str() else {
            return usage(err, "arguments must be UTF-8");
        };
        match arg {
            "-h" | "--help" => {
                writeln!(
                    out,
                    "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json] [--provenance] [--refresh]"
                )?;
                writeln!(
                    out,
                    "Browse offline model metadata; --refresh explicitly downloads the public catalogs."
                )?;
                writeln!(
                    out,
                    "Use --provenance to include catalog age and source metadata."
                )?;
                writeln!(
                    out,
                    "'cloudflare' selects Workers AI; 'cloudflare-ai-gateway' selects the AI Gateway and Workers AI catalogs."
                )?;
                return Ok(0);
            }
            "--json" if !json => json = true,
            "--provenance" if !provenance => provenance = true,
            "--refresh" if !refresh => refresh = true,
            "--provider" if provider.is_none() => {
                index += 1;
                provider = args.get(index).and_then(|s| s.to_str());
                if provider.is_none_or(str::is_empty) {
                    return usage(err, "--provider needs a value");
                }
            }
            "--id" if id.is_none() => {
                index += 1;
                id = args.get(index).and_then(|s| s.to_str());
                if id.is_none_or(str::is_empty) {
                    return usage(err, "--id needs a value");
                }
            }
            _ => return usage(err, "unrecognized or duplicate option"),
        }
        index += 1;
    }

    let settings = user_settings_paths().map_err(io::Error::other)?;
    let cache_path = settings
        .as_ref()
        .map(|paths| cache_path_for_user_settings(&paths.model));
    let loaded = if refresh {
        let Some(cache_path) = cache_path.as_deref() else {
            return usage(
                err,
                "cannot locate a per-user config directory; set XDG_CONFIG_HOME or HOME to refresh the catalog",
            );
        };
        let result = refresh_user_catalog(cache_path).map_err(io::Error::other)?;
        if result.updated {
            writeln!(err, "krypper: refreshed the public model catalog cache")?;
        } else if let (Some(source), Some(cause)) =
            (result.failed_source.as_ref(), result.source_error.as_ref())
        {
            writeln!(
                err,
                "krypper: refresh failed for {}; using last-known-good catalog{} ({cause})",
                source.as_str(),
                if result.cache_is_stale {
                    " (stale)"
                } else {
                    ""
                }
            )?;
        }
        catalog_from_snapshot(result.snapshot).map_err(io::Error::other)?
    } else {
        load_catalog(cache_path.as_deref()).map_err(io::Error::other)?
    };

    let selected: Vec<_> = loaded
        .view
        .models
        .iter()
        .filter(|entry| {
            provider.is_none_or(|p| crate::catalog_data::provider_matches(p, &entry.model.provider))
                && id.is_none_or(|value| entry.model.id == value)
        })
        .collect();

    if json {
        let values: Vec<_> = selected.iter().map(|entry| model_json(entry)).collect();
        if provenance {
            let catalog = serde_json::json!({
                "origin": origin_name(loaded.origin),
                "stale": loaded.stale,
                "provenance": loaded.view.provenance,
            });
            serde_json::to_writer_pretty(
                &mut *out,
                &serde_json::json!({
                    "catalog": catalog,
                    "models": values,
                }),
            )?;
        } else {
            serde_json::to_writer_pretty(&mut *out, &values)?;
        }
        writeln!(out)?;
    } else {
        if provenance {
            writeln!(
                out,
                "Catalog: {}{}",
                origin_name(loaded.origin),
                if loaded.stale { " (stale)" } else { "" }
            )?;
            for source in &loaded.view.provenance {
                writeln!(
                    out,
                    "Catalog source: {} retrieved_at_unix={} license={} url={}",
                    source.source.as_str(),
                    source.retrieved_at_unix,
                    source.license,
                    source.url
                )?;
            }
        }
        for entry in &selected {
            let model = &entry.model;
            writeln!(
                out,
                "{}\t{}\t{}\tcontext={}\toutput={}\ttools={}\treasoning={}",
                model.provider,
                model.id,
                model.name,
                model
                    .context_window
                    .map_or("?".to_owned(), |v| v.to_string()),
                model
                    .max_output_tokens
                    .map_or("?".to_owned(), |v| v.to_string()),
                model.tool_calling.map_or("?".to_owned(), |v| v.to_string()),
                model.reasoning.map_or("?".to_owned(), |v| v.to_string())
            )?;
        }
    }
    Ok(0)
}

fn model_json(entry: &crate::catalog_sources::CatalogModel) -> serde_json::Value {
    let model = &entry.model;
    serde_json::json!({
        "provider": model.provider, "id": model.id, "name": model.name,
        "context_window": model.context_window, "max_output_tokens": model.max_output_tokens,
        "tool_calling": model.tool_calling, "reasoning": model.reasoning,
        "modalities": model.modalities, "status": model.status
    })
}

fn origin_name(origin: CatalogOrigin) -> &'static str {
    match origin {
        CatalogOrigin::Embedded => "embedded",
        CatalogOrigin::RefreshedCache => "refreshed_cache",
    }
}

fn usage(err: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(
        err,
        "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json] [--provenance] [--refresh]"
    )?;
    writeln!(err, "krypper models: error: {message}")?;
    Ok(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_invalid_args() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run(&[OsString::from("--help")], &mut out, &mut err).unwrap(),
            0
        );
        assert!(String::from_utf8(out).unwrap().contains("offline"));
        assert_eq!(
            run(&[OsString::from("--bogus")], &mut Vec::new(), &mut err).unwrap(),
            2
        );
    }

    #[test]
    fn unknown_model_is_empty_not_an_error() {
        let mut out = Vec::new();
        assert_eq!(
            run(
                &[
                    OsString::from("--id"),
                    OsString::from("__unknown__"),
                    OsString::from("--json")
                ],
                &mut out,
                &mut Vec::new()
            )
            .unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap().trim(), "[]");
    }

    #[test]
    fn cloudflare_selectors_list_runnable_catalog_models() {
        fn models_for(provider: &str) -> Vec<serde_json::Value> {
            let mut out = Vec::new();
            assert_eq!(
                run(
                    &[
                        OsString::from("--provider"),
                        OsString::from(provider),
                        OsString::from("--json"),
                    ],
                    &mut out,
                    &mut Vec::new(),
                )
                .unwrap(),
                0
            );
            serde_json::from_slice(&out).expect("model list JSON")
        }

        let workers_ai = models_for("cloudflare");
        assert!(!workers_ai.is_empty());
        assert!(workers_ai.iter().all(|model| {
            model["provider"] == "cloudflare-workers-ai"
                && model["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("@cf/"))
        }));

        let gateway = models_for("cloudflare-ai-gateway");
        let providers: std::collections::BTreeSet<_> = gateway
            .iter()
            .filter_map(|model| model["provider"].as_str())
            .collect();
        assert!(providers.contains("cloudflare-ai-gateway"));
        assert!(providers.contains("cloudflare-workers-ai"));
    }

    #[test]
    fn provenance_output_is_opt_in_and_keeps_plain_json_array_contract() {
        let mut output = Vec::new();
        assert_eq!(
            run(&[OsString::from("--json")], &mut output, &mut Vec::new()).unwrap(),
            0
        );
        assert!(serde_json::from_slice::<Vec<serde_json::Value>>(&output).is_ok());

        output.clear();
        assert_eq!(
            run(
                &[OsString::from("--json"), OsString::from("--provenance")],
                &mut output,
                &mut Vec::new()
            )
            .unwrap(),
            0
        );
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert!(value["catalog"]["provenance"].is_array());
        assert!(value["models"].is_array());
    }
}
