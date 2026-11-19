//! Read-only CLI for the vendored models.dev snapshot.
use std::ffi::OsString;
use std::io::{self, Write};

pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> io::Result<i32> {
    let mut provider: Option<&str> = None;
    let mut id: Option<&str> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].to_str() else {
            return usage(err, "arguments must be UTF-8");
        };
        match arg {
            "-h" | "--help" => {
                writeln!(
                    out,
                    "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json]"
                )?;
                writeln!(
                    out,
                    "Read-only, offline model metadata; does not validate provider access."
                )?;
                return Ok(0);
            }
            "--json" if !json => json = true,
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
    let all = crate::catalog_data::models().map_err(io::Error::other)?;
    let selected: Vec<_> = all
        .into_iter()
        .filter(|m| provider.is_none_or(|p| m.provider == p) && id.is_none_or(|v| m.id == v))
        .collect();
    if json {
        let values: Vec<_> = selected
            .iter()
            .map(|m| {
                serde_json::json!({
                "provider": m.provider, "id": m.id, "name": m.name,
                "context_window": m.context_window, "max_output_tokens": m.max_output_tokens,
                "tool_calling": m.tool_calling, "reasoning": m.reasoning,
                "modalities": m.modalities, "status": m.status
                })
            })
            .collect();
        serde_json::to_writer_pretty(&mut *out, &values)?;
        writeln!(out)?;
    } else {
        for m in &selected {
            writeln!(
                out,
                "{}\t{}\t{}\tcontext={}\toutput={}\ttools={}\treasoning={}",
                m.provider,
                m.id,
                m.name,
                m.context_window.map_or("?".to_owned(), |v| v.to_string()),
                m.max_output_tokens
                    .map_or("?".to_owned(), |v| v.to_string()),
                m.tool_calling.map_or("?".to_owned(), |v| v.to_string()),
                m.reasoning.map_or("?".to_owned(), |v| v.to_string())
            )?;
        }
    }
    Ok(0)
}

fn usage(err: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(
        err,
        "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json]"
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
}
