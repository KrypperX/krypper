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
                    "Browse the offline model catalog in tables grouped by provider; --refresh explicitly downloads public catalogs."
                )?;
                writeln!(
                    out,
                    "Tables show catalog limits, declared capabilities, modalities, reference prices, features, and status; missing facts are unknown."
                )?;
                writeln!(
                    out,
                    "Use --provenance for catalog sources; --json --provenance includes per-model sources, pricing, and feature metadata."
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
        let values: Vec<_> = selected
            .iter()
            .map(|entry| {
                if provenance {
                    model_json_with_provenance(entry)
                } else {
                    model_json(entry)
                }
            })
            .collect();
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
        write_provider_tables(&selected, &loaded.view.providers, out)?;
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

fn write_provider_tables(
    selected: &[&crate::catalog_sources::CatalogModel],
    providers: &[crate::catalog_sources::CatalogProviderMetadata],
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut grouped =
        std::collections::BTreeMap::<&str, Vec<&crate::catalog_sources::CatalogModel>>::new();
    for entry in selected {
        grouped
            .entry(entry.model.provider.as_str())
            .or_default()
            .push(*entry);
    }

    const HEADERS: [&str; 8] = [
        "Model",
        "Limits (context / output)",
        "Tools",
        "Reasoning",
        "Modalities (input / output)",
        "Reference price (USD / 1M, input / output)",
        "Known features",
        "Status",
    ];

    for (group_index, (provider_id, models)) in grouped.into_iter().enumerate() {
        if group_index > 0 {
            writeln!(out)?;
        }
        let display_provider_id = table_cell(provider_id);
        let provider_name = providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(|provider| provider.name.as_str())
            .filter(|name| !name.is_empty() && *name != provider_id);
        if let Some(provider_name) = provider_name {
            writeln!(
                out,
                "Provider: {} ({display_provider_id}) - {} models",
                table_cell(provider_name),
                models.len()
            )?;
        } else {
            writeln!(
                out,
                "Provider: {display_provider_id} - {} models",
                models.len()
            )?;
        }

        let rows: Vec<Vec<String>> = models
            .iter()
            .map(|entry| {
                let model = &entry.model;
                vec![
                    format!("{} [{}]", model.name, model.id),
                    format!(
                        "{} / {}",
                        model
                            .context_window
                            .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
                        model
                            .max_output_tokens
                            .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
                    ),
                    yes_no_unknown(model.tool_calling),
                    yes_no_unknown(model.reasoning),
                    format_modalities(model.modalities.as_ref()),
                    format_reference_price(entry.pricing.as_ref()),
                    format_known_features(&entry.features),
                    model
                        .status
                        .as_deref()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("unknown")
                        .to_owned(),
                ]
                .into_iter()
                .map(|value| table_cell(&value))
                .collect()
            })
            .collect();
        write_pipe_table(out, &HEADERS, &rows)?;
    }
    Ok(())
}

fn write_pipe_table(out: &mut dyn Write, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .chain(std::iter::once(headers[column].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    write_pipe_row(
        out,
        &headers
            .iter()
            .map(|header| (*header).to_owned())
            .collect::<Vec<_>>(),
        &widths,
    )?;
    writeln!(
        out,
        "|{}|",
        widths
            .iter()
            .map(|width| format!(" {} ", "-".repeat(*width)))
            .collect::<Vec<_>>()
            .join("|")
    )?;
    for row in rows {
        write_pipe_row(out, row, &widths)?;
    }
    Ok(())
}

fn write_pipe_row(out: &mut dyn Write, cells: &[String], widths: &[usize]) -> io::Result<()> {
    let padded = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!(" {cell:<width$} "))
        .collect::<Vec<_>>()
        .join("|");
    writeln!(out, "|{padded}|")
}

fn table_cell(value: &str) -> String {
    let mut cell = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '|' => cell.push_str("\\|"),
            character if character.is_control() => cell.push(' '),
            character => cell.push(character),
        }
    }
    cell
}

fn yes_no_unknown(value: Option<bool>) -> String {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
    .to_owned()
}

fn format_modalities(value: Option<&serde_json::Value>) -> String {
    fn side(value: Option<&serde_json::Value>, key: &str) -> String {
        let Some(values) = value.and_then(|value| value.get(key)) else {
            return "unknown".to_owned();
        };
        let Some(values) = values.as_array() else {
            return "unknown".to_owned();
        };
        let values: Vec<_> = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if values.is_empty() {
            "none".to_owned()
        } else {
            values.join(", ")
        }
    }
    format!(
        "input: {}; output: {}",
        side(value, "input"),
        side(value, "output")
    )
}

fn format_reference_price(price: Option<&crate::catalog_sources::ModelPricing>) -> String {
    let Some(price) = price else {
        return "unknown".to_owned();
    };
    let input = price
        .input_usd_per_million_tokens
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), ToString::to_string);
    let output = price
        .output_usd_per_million_tokens
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), ToString::to_string);
    format!("{input} / {output}")
}

fn format_known_features(features: &crate::catalog_sources::ModelFeatures) -> String {
    let mut values = Vec::new();
    for (name, value) in [
        ("attachments", features.supports_attachments),
        ("structured output", features.structured_output),
        ("vision", features.vision),
        ("paid plan required", features.requires_workers_paid),
    ] {
        if let Some(value) = value {
            values.push(format!("{name}: {}", yes_no_unknown(Some(value))));
        }
    }
    if let Some(task) = features.task.as_deref().filter(|value| !value.is_empty()) {
        values.push(format!("task: {task}"));
    }
    if let Some(date) = features
        .planned_deprecation_date
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        values.push(format!("planned deprecation: {date}"));
    }
    if values.is_empty() {
        "unknown".to_owned()
    } else {
        values.join("; ")
    }
}

fn model_json_with_provenance(entry: &crate::catalog_sources::CatalogModel) -> serde_json::Value {
    let mut value = model_json(entry);
    value["pricing"] = serde_json::to_value(&entry.pricing).expect("pricing is serializable");
    value["features"] = serde_json::to_value(&entry.features).expect("features are serializable");
    value["sources"] = serde_json::to_value(&entry.sources).expect("sources are serializable");
    value
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
    fn human_output_groups_models_by_provider_and_shows_catalog_details() {
        let mut output = Vec::new();
        assert_eq!(
            run(
                &[
                    OsString::from("--provider"),
                    OsString::from("cloudflare-ai-gateway"),
                ],
                &mut output,
                &mut Vec::new(),
            )
            .unwrap(),
            0
        );

        let output = String::from_utf8(output).unwrap();
        let groups: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with("Provider: "))
            .collect();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].contains("cloudflare-ai-gateway"));
        assert!(groups[1].contains("cloudflare-workers-ai"));
        assert!(output.contains("| Model"));
        assert!(output.contains("Limits (context / output)"));
        assert!(output.contains("Reference price"));
        assert!(output.contains("@cf/"));
        assert!(output.contains("unknown"));
    }

    #[test]
    fn human_table_shows_reference_prices_and_only_reported_features() {
        let price = crate::catalog_sources::ModelPricing {
            input_usd_per_million_tokens: Some(serde_json::Number::from(2)),
            output_usd_per_million_tokens: Some(serde_json::Number::from(8)),
        };
        assert_eq!(format_reference_price(Some(&price)), "2 / 8");
        assert_eq!(format_reference_price(None), "unknown");

        let features = crate::catalog_sources::ModelFeatures {
            supports_attachments: Some(false),
            vision: Some(true),
            task: Some("text-generation".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            format_known_features(&features),
            "attachments: no; vision: yes; task: text-generation"
        );
    }

    #[test]
    fn table_cells_escape_delimiters_and_replace_terminal_controls() {
        assert_eq!(table_cell("name | value\nnext"), "name \\| value next");
    }

    #[test]
    fn provenance_output_is_opt_in_and_keeps_plain_json_array_contract() {
        let mut output = Vec::new();
        assert_eq!(
            run(&[OsString::from("--json")], &mut output, &mut Vec::new()).unwrap(),
            0
        );
        let plain: Vec<serde_json::Value> = serde_json::from_slice(&output).unwrap();
        assert!(plain.iter().all(|model| model.get("pricing").is_none()));

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
        assert!(value["models"].as_array().unwrap().iter().all(|model| {
            model.get("pricing").is_some()
                && model.get("features").is_some()
                && model.get("sources").is_some()
        }));
    }

    #[test]
    fn model_pricing_and_features_are_only_in_provenance_json() {
        let entry = crate::catalog_sources::CatalogModel {
            model: crate::catalog_data::ModelSpec {
                provider: "test-provider".to_owned(),
                id: "test-model".to_owned(),
                name: "Test model".to_owned(),
                context_window: Some(4096),
                max_output_tokens: Some(1024),
                tool_calling: Some(true),
                reasoning: Some(true),
                status: None,
                modalities: None,
            },
            pricing: Some(crate::catalog_sources::ModelPricing {
                input_usd_per_million_tokens: Some(serde_json::Number::from(2)),
                output_usd_per_million_tokens: Some(serde_json::Number::from(8)),
            }),
            features: crate::catalog_sources::ModelFeatures {
                structured_output: Some(true),
                ..Default::default()
            },
            sources: vec![crate::catalog_sources::CatalogSourceId::new("test-source")
                .expect("valid source id")],
        };

        let plain = model_json(&entry);
        assert!(plain.get("pricing").is_none());
        assert!(plain.get("features").is_none());
        assert!(plain.get("sources").is_none());

        let detailed = model_json_with_provenance(&entry);
        assert_eq!(detailed["pricing"]["input_usd_per_million_tokens"], 2);
        assert_eq!(detailed["pricing"]["output_usd_per_million_tokens"], 8);
        assert_eq!(detailed["features"]["structured_output"], true);
        assert_eq!(detailed["sources"][0], "test-source");
    }
}
