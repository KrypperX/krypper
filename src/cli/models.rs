//! `krypper models`: shows what the model catalog knows.
//!
//! The command only reads the compiled-in catalog and the optional user catalog
//! file. It never contacts a model provider, so running it sends nothing over
//! the network.

use super::config::config_path;
use super::usage_error;
use crate::brain::ModelProvider;
use crate::catalog::{load_user_catalog, Catalog, ModelEntry};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

const USER_CATALOG_FILE: &str = "models.toml";
const COMMAND: Option<&str> = Some("models");

pub(super) fn run_models(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    run_models_with(arguments, output, error, &user_catalog_path, &|name| {
        ModelProvider::parse(name).is_some()
    })
}

/// Chooses the user catalog file.
///
/// `KRYPPER_MODELS` (an absolute path) wins; otherwise the file is
/// `models.toml` beside the configuration file. Like the configuration file, it
/// is never read from a task workspace.
fn user_catalog_path() -> Result<Option<PathBuf>, String> {
    let config = config_path(
        std::env::var_os("KRYPPER_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )?;
    resolve_catalog_path(std::env::var_os("KRYPPER_MODELS"), config)
}

fn resolve_catalog_path(
    explicit: Option<OsString>,
    config: Option<PathBuf>,
) -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(explicit);
        if !path.is_absolute() {
            return Err("KRYPPER_MODELS must be an absolute path".to_owned());
        }
        return Ok(Some(path));
    }
    Ok(config.map(|path| path.with_file_name(USER_CATALOG_FILE)))
}

fn run_models_with(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
    user_catalog: &dyn Fn() -> Result<Option<PathBuf>, String>,
    known_provider: &dyn Fn(&str) -> bool,
) -> io::Result<i32> {
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        write_models_help(output)?;
        return Ok(0);
    }

    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut no_config = false;
    let mut position = 0;
    while position < arguments.len() {
        let Some(argument) = arguments[position].to_str() else {
            return usage_error(error, COMMAND, "arguments must be valid UTF-8");
        };
        position += 1;
        match argument {
            "--provider" | "--model" => {
                let slot = if argument == "--provider" {
                    &mut provider
                } else {
                    &mut model
                };
                if slot.is_some() {
                    return usage_error(
                        error,
                        COMMAND,
                        &format!("{argument} may be provided only once"),
                    );
                }
                let Some(value) = arguments.get(position) else {
                    return usage_error(error, COMMAND, &format!("{argument} requires a value"));
                };
                let Some(value) = value.to_str() else {
                    return usage_error(
                        error,
                        COMMAND,
                        &format!("{argument} value must be valid UTF-8"),
                    );
                };
                *slot = Some(value.to_owned());
                position += 1;
            }
            "--no-config" => {
                if no_config {
                    return usage_error(error, COMMAND, "--no-config may be provided only once");
                }
                no_config = true;
            }
            _ => {
                return usage_error(
                    error,
                    COMMAND,
                    &format!("unrecognized arguments: {argument}"),
                );
            }
        }
    }

    if model.is_some() && provider.is_none() {
        return usage_error(error, COMMAND, "--model requires --provider");
    }
    if let Some(name) = &provider {
        if !known_provider(name) {
            return usage_error(error, COMMAND, "unsupported provider");
        }
    }
    if let Some(name) = &model {
        if name.is_empty() || name.chars().any(char::is_control) {
            return usage_error(
                error,
                COMMAND,
                "model name must be non-empty and must not contain control characters",
            );
        }
    }

    let mut catalog = Catalog::built_in();
    if !no_config {
        let path = match user_catalog() {
            Ok(path) => path,
            Err(message) => return usage_error(error, COMMAND, &message),
        };
        if let Some(path) = path {
            match load_user_catalog(&path, known_provider) {
                Ok(Some(entries)) => {
                    if !entries.is_empty() {
                        let _ = writeln!(
                            error,
                            "krypper: including {} user model entries from {}",
                            entries.len(),
                            path.display()
                        );
                    }
                    catalog = catalog.with_user_entries(entries);
                }
                Ok(None) => {}
                Err(message) => return usage_error(error, COMMAND, &message),
            }
        }
    }

    if let (Some(provider), Some(model)) = (&provider, &model) {
        match catalog.lookup(provider, model) {
            Some(entry) => write_entry(output, entry)?,
            None => writeln!(
                output,
                "{provider}/{model} is not in the catalog; it can still be used with --model."
            )?,
        }
        return Ok(0);
    }

    let entries: Vec<&ModelEntry> = catalog
        .entries()
        .iter()
        .filter(|entry| provider.as_ref().is_none_or(|name| &entry.provider == name))
        .collect();
    if entries.is_empty() {
        if let Some(name) = &provider {
            writeln!(
                output,
                "no catalog entries for provider '{name}'; models are still accepted by name."
            )?;
        }
        return Ok(0);
    }
    write_table(output, &entries)?;
    Ok(0)
}

fn count(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |count| count.to_string())
}

fn write_entry(output: &mut dyn Write, entry: &ModelEntry) -> io::Result<()> {
    writeln!(output, "provider: {}", entry.provider)?;
    writeln!(output, "model: {}", entry.id)?;
    if let Some(label) = &entry.label {
        writeln!(output, "label: {label}")?;
    }
    writeln!(output, "context window: {}", count(entry.context_window))?;
    writeln!(
        output,
        "max output tokens: {}",
        count(entry.max_output_tokens)
    )?;
    writeln!(output, "tool calling: {}", entry.tool_calling.as_str())?;
    writeln!(output, "reasoning: {}", entry.reasoning.as_str())?;
    writeln!(output, "source: {}", entry.origin.as_str())
}

fn write_table(output: &mut dyn Write, entries: &[&ModelEntry]) -> io::Result<()> {
    let mut rows: Vec<[String; 7]> = vec![[
        "PROVIDER".to_owned(),
        "MODEL".to_owned(),
        "CONTEXT".to_owned(),
        "MAX OUTPUT".to_owned(),
        "TOOLS".to_owned(),
        "REASONING".to_owned(),
        "SOURCE".to_owned(),
    ]];
    for entry in entries {
        rows.push([
            entry.provider.clone(),
            entry.id.clone(),
            count(entry.context_window),
            count(entry.max_output_tokens),
            entry.tool_calling.as_str().to_owned(),
            entry.reasoning.as_str().to_owned(),
            entry.origin.as_str().to_owned(),
        ]);
    }
    let mut widths = [0_usize; 7];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in &rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if index + 1 < row.len() {
                for _ in cell.chars().count()..widths[index] {
                    line.push(' ');
                }
            }
        }
        writeln!(output, "{line}")?;
    }
    Ok(())
}

pub(super) fn write_models_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        concat!(
            "usage: krypper models [-h] [--provider NAME] [--model NAME] [--no-config]\n\n",
            "show what Krypper knows about models\n\n",
            "options:\n",
            "  -h, --help       show this help message and exit\n",
            "  --provider NAME  only show models of this provider\n",
            "  --model NAME     show one model; requires --provider\n",
            "  --no-config      ignore the user model catalog file for this run\n\n",
            "Models that are not listed are still accepted by 'krypper task --model'.\n",
            "The user catalog is $KRYPPER_MODELS, or models.toml beside the configuration\n",
            "file (see README). This command never contacts a model provider.\n",
        )
        .as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Support;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("krypper-models-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, text: &str) -> PathBuf {
            let path = self.0.join("models.toml");
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(
        args: &[&str],
        user: Result<Option<PathBuf>, String>,
    ) -> (i32, String, String) {
        let arguments: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_models_with(&arguments, &mut output, &mut error, &|| user.clone(), &|name| {
            ModelProvider::parse(name).is_some()
        })
        .unwrap();
        (
            code,
            String::from_utf8(output).unwrap(),
            String::from_utf8(error).unwrap(),
        )
    }

    fn user_file(directory: &TempDir, text: &str) -> Result<Option<PathBuf>, String> {
        Ok(Some(directory.write(text)))
    }

    #[test]
    fn every_built_in_entry_names_a_supported_provider() {
        for entry in Catalog::built_in().entries() {
            assert!(
                ModelProvider::parse(&entry.provider).is_some(),
                "unsupported provider {}",
                entry.provider
            );
        }
    }

    #[test]
    fn lists_built_in_entries_as_an_aligned_table() {
        let (code, output, error) = run(&[], Ok(None));
        assert_eq!(code, 0, "{error}");
        let mut lines = output.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with("PROVIDER"), "{header}");
        assert!(header.ends_with("SOURCE"), "{header}");
        let opus = lines
            .find(|line| line.contains("claude-opus-5-5"))
            .expect("opus row");
        assert!(opus.starts_with("anthropic"), "{opus}");
        assert!(opus.contains("built-in"), "{opus}");
        let model_column = header.find("MODEL").unwrap();
        assert_eq!(opus.find("claude-opus-5-5"), Some(model_column));
        assert!(output.lines().all(|line| line == line.trim_end()));
        assert!(error.is_empty());
    }

    #[test]
    fn provider_filter_limits_rows_and_reports_providers_without_entries() {
        let (code, output, _) = run(&["--provider", "anthropic"], Ok(None));
        assert_eq!(code, 0);
        assert!(output.lines().skip(1).all(|line| line.starts_with("anthropic")));

        let (code, output, _) = run(&["--provider", "ollama"], Ok(None));
        assert_eq!(code, 0);
        assert_eq!(
            output,
            "no catalog entries for provider 'ollama'; models are still accepted by name.\n"
        );
    }

    #[test]
    fn model_view_shows_every_field_and_unknown_models_are_not_errors() {
        let (code, output, _) = run(
            &["--provider", "anthropic", "--model", "claude-opus-5-5"],
            Ok(None),
        );
        assert_eq!(code, 0);
        assert!(output.contains("model: claude-opus-5-5\n"), "{output}");
        assert!(output.contains("label: Claude Opus 5.5\n"), "{output}");
        assert!(output.contains("context window: unknown\n"), "{output}");
        assert!(output.contains("tool calling: yes\n"), "{output}");
        assert!(output.contains("source: built-in\n"), "{output}");

        let (code, output, error) = run(
            &["--provider", "anthropic", "--model", "not-a-listed-model"],
            Ok(None),
        );
        assert_eq!(code, 0, "{error}");
        assert_eq!(
            output,
            "anthropic/not-a-listed-model is not in the catalog; it can still be used with --model.\n"
        );
    }

    #[test]
    fn rejects_bad_arguments_with_exit_status_two() {
        for (args, fragment) in [
            (vec!["--model", "m"], "--model requires --provider"),
            (vec!["--provider", "nope"], "unsupported provider"),
            (vec!["--provider"], "--provider requires a value"),
            (
                vec!["--provider", "ollama", "--provider", "gemini"],
                "--provider may be provided only once",
            ),
            (
                vec!["--provider", "ollama", "--model", "a", "--model", "b"],
                "--model may be provided only once",
            ),
            (vec!["--no-config", "--no-config"], "--no-config may be provided only once"),
            (vec!["extra"], "unrecognized arguments: extra"),
            (
                vec!["--provider", "ollama", "--model", "bad\nname"],
                "control characters",
            ),
            (vec!["--provider", "ollama", "--model", ""], "non-empty"),
        ] {
            let (code, output, error) = run(&args, Ok(None));
            assert_eq!(code, 2, "{args:?}");
            assert!(output.is_empty(), "{args:?}");
            assert!(error.contains(fragment), "{args:?} -> {error}");
            assert!(error.contains("usage: krypper models"), "{error}");
        }
    }

    #[test]
    fn help_succeeds_without_reading_any_file() {
        let (code, output, error) = run(&["--help"], Err("must not be called".to_owned()));
        assert_eq!(code, 0);
        assert!(output.starts_with("usage: krypper models"), "{output}");
        assert!(output.contains("never contacts a model provider"));
        assert!(error.is_empty());
    }

    #[test]
    fn user_catalog_adds_entries_and_announces_its_source() {
        let directory = TempDir::new();
        let user = user_file(
            &directory,
            "version = 1\n[[model]]\nprovider = \"ollama\"\nid = \"qwen3:8b\"\ncontext_window = 32768\ntool_calling = \"yes\"\n",
        );
        let (code, output, error) = run(&[], user.clone());
        assert_eq!(code, 0, "{error}");
        assert!(output.contains("qwen3:8b"), "{output}");
        assert!(output.contains("32768"), "{output}");
        assert!(output.contains("user"), "{output}");
        assert!(
            error.contains("including 1 user model entries from"),
            "{error}"
        );

        let (code, output, _) = run(&["--provider", "ollama", "--model", "qwen3:8b"], user);
        assert_eq!(code, 0);
        assert!(output.contains("context window: 32768\n"), "{output}");
        assert!(output.contains("source: user\n"), "{output}");
    }

    #[test]
    fn user_catalog_can_override_a_built_in_entry() {
        let directory = TempDir::new();
        let user = user_file(
            &directory,
            "version = 1\n[[model]]\nprovider = \"anthropic\"\nid = \"claude-opus-5-5\"\ncontext_window = 1234\ntool_calling = \"no\"\n",
        );
        let (code, output, _) = run(
            &["--provider", "anthropic", "--model", "claude-opus-5-5"],
            user,
        );
        assert_eq!(code, 0);
        assert!(output.contains("context window: 1234\n"), "{output}");
        assert!(output.contains("tool calling: no\n"), "{output}");
        assert!(output.contains("source: user\n"), "{output}");
    }

    #[test]
    fn a_missing_user_catalog_is_not_an_error() {
        let directory = TempDir::new();
        let (code, output, error) = run(&[], Ok(Some(directory.0.join("absent.toml"))));
        assert_eq!(code, 0, "{error}");
        assert!(output.contains("claude-opus-5-5"));
        assert!(error.is_empty());
    }

    #[test]
    fn a_broken_user_catalog_fails_before_printing_anything() {
        let directory = TempDir::new();
        let user = user_file(&directory, "version = 1\n[[model]]\nprovider = \"nope\"\nid = \"m\"\n");
        let (code, output, error) = run(&[], user);
        assert_eq!(code, 2);
        assert!(output.is_empty());
        assert!(error.contains("invalid model catalog"), "{error}");
        assert!(error.contains("unsupported provider"), "{error}");
    }

    #[test]
    fn no_config_ignores_even_a_broken_user_catalog() {
        let directory = TempDir::new();
        let user = user_file(&directory, "this is not valid\n");
        let (code, output, error) = run(&["--no-config"], user);
        assert_eq!(code, 0, "{error}");
        assert!(output.contains("claude-opus-5-5"));
        assert!(error.is_empty());

        let (code, _, error) = run(&["--no-config"], Err("must not be called".to_owned()));
        assert_eq!(code, 0, "{error}");
    }

    #[test]
    fn a_bad_catalog_location_is_a_usage_error() {
        let (code, _, error) = run(&[], Err("KRYPPER_MODELS must be an absolute path".to_owned()));
        assert_eq!(code, 2);
        assert!(error.contains("KRYPPER_MODELS"), "{error}");
    }

    #[test]
    fn catalog_path_prefers_the_override_then_sits_beside_the_config_file() {
        let config = Some(PathBuf::from("/home/u/.config/krypper/config.toml"));
        assert_eq!(
            resolve_catalog_path(Some(OsString::from("/etc/models.toml")), config.clone()),
            Ok(Some(PathBuf::from("/etc/models.toml")))
        );
        assert_eq!(
            resolve_catalog_path(Some(OsString::new()), config.clone()),
            Ok(Some(PathBuf::from("/home/u/.config/krypper/models.toml")))
        );
        assert_eq!(
            resolve_catalog_path(None, config),
            Ok(Some(PathBuf::from("/home/u/.config/krypper/models.toml")))
        );
        assert_eq!(resolve_catalog_path(None, None), Ok(None));
        assert!(resolve_catalog_path(Some(OsString::from("models.toml")), None).is_err());
    }

    #[test]
    fn support_values_render_as_documented() {
        assert_eq!(Support::Yes.as_str(), "yes");
        assert_eq!(Support::No.as_str(), "no");
        assert_eq!(Support::Unknown.as_str(), "unknown");
    }
}
