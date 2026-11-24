//! Optional user-level defaults for `krypper task`.
//!
//! The file accepts top-level string assignments and TOML comments. Credentials
//! are kept in a separate per-user credential store; environment variables
//! override the stored values.
//!
//! Krypper reads the file only from the user's own configuration location, never
//! from a task workspace, so repository contents cannot change which service
//! receives task data.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const CONFIG_DIRECTORY: &str = "krypper";
const CONFIG_FILE: &str = "config.toml";

/// Model selection defaults loaded from a configuration file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModelDefaults {
    pub(crate) model: String,
    pub(crate) provider: Option<String>,
    pub(crate) routing_mode: RoutingMode,
    pub(crate) allow_cross_provider_data_transfer: bool,
    /// A configured ceiling enables bounded fallback; `None` keeps it disabled.
    pub(crate) fallback_budget_micro_usd: Option<u64>,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RoutingMode {
    #[default]
    Manual,
    Automatic,
}

impl RoutingMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Automatic => "automatic",
        }
    }
}

pub(super) fn validate_route_settings(
    routing_mode: RoutingMode,
    allow_cross_provider_data_transfer: bool,
    fallback_budget_micro_usd: Option<u64>,
) -> Result<(), &'static str> {
    if routing_mode == RoutingMode::Manual
        && (allow_cross_provider_data_transfer || fallback_budget_micro_usd.is_some())
    {
        Err("cross-provider routing and fallback require automatic routing")
    } else {
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedConfig {
    provider: Option<String>,
    model: Option<String>,
    routing_mode: Option<RoutingMode>,
    allow_cross_provider_data_transfer: Option<bool>,
    fallback_budget_micro_usd: Option<u64>,
}

/// Chooses the configuration file location.
///
/// `KRYPPER_CONFIG` wins, then `XDG_CONFIG_HOME`, then `HOME/.config`. Every
/// location must be an absolute path; empty values are treated as unset, and a
/// relative `XDG_CONFIG_HOME` or `HOME` is ignored as the XDG specification
/// requires. A relative `KRYPPER_CONFIG` is rejected so a typo cannot silently
/// select a different file.
pub(super) fn config_path(
    explicit: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(explicit);
        if !path.is_absolute() {
            return Err("KRYPPER_CONFIG must be an absolute path".to_owned());
        }
        return Ok(Some(path));
    }
    if let Some(base) = absolute_directory(xdg_config_home) {
        return Ok(Some(base.join(CONFIG_DIRECTORY).join(CONFIG_FILE)));
    }
    if let Some(base) = absolute_directory(home) {
        return Ok(Some(
            base.join(".config")
                .join(CONFIG_DIRECTORY)
                .join(CONFIG_FILE),
        ));
    }
    Ok(None)
}

fn absolute_directory(value: Option<OsString>) -> Option<PathBuf> {
    value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Loads model defaults from `path`.
///
/// Returns `Ok(None)` when the file does not exist or sets neither key. Any
/// other problem is reported so a broken file never silently changes where task
/// data goes.
pub(super) fn load_model_defaults(path: &Path) -> Result<Option<ModelDefaults>, String> {
    let Some(text) = read_config_text(path)? else {
        return Ok(None);
    };
    let parsed = parse_config(&text)
        .map_err(|cause| format!("invalid config file '{}': {cause}", path.display()))?;
    let has_routing_settings = parsed.routing_mode.is_some()
        || parsed.allow_cross_provider_data_transfer.is_some()
        || parsed.fallback_budget_micro_usd.is_some();
    let Some(model) = parsed.model else {
        if parsed.provider.is_some() {
            return Err(format!(
                "invalid config file '{}': provider requires model",
                path.display()
            ));
        }
        if has_routing_settings {
            return Err(format!(
                "invalid config file '{}': routing settings require a model default",
                path.display()
            ));
        }
        return Ok(None);
    };
    let routing_mode = parsed.routing_mode.unwrap_or_default();
    let allow_cross_provider_data_transfer =
        parsed.allow_cross_provider_data_transfer.unwrap_or(false);
    let fallback_budget_micro_usd = parsed.fallback_budget_micro_usd;
    validate_route_settings(
        routing_mode,
        allow_cross_provider_data_transfer,
        fallback_budget_micro_usd,
    )
    .map_err(|message| format!("invalid config file '{}': {message}", path.display()))?;
    Ok(Some(ModelDefaults {
        model,
        provider: parsed.provider,
        routing_mode,
        allow_cross_provider_data_transfer,
        fallback_budget_micro_usd,
        path: path.to_path_buf(),
    }))
}

fn read_config_text(path: &Path) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => {
            return Err(format!(
                "cannot read config file '{}': {}",
                path.display(),
                cause.kind()
            ));
        }
    };
    let metadata = file.metadata().map_err(|cause| {
        format!(
            "cannot read config file '{}': {}",
            path.display(),
            cause.kind()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "config file '{}' is not a regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| {
            format!(
                "cannot read config file '{}': {}",
                path.display(),
                cause.kind()
            )
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!(
            "config file '{}' is larger than {MAX_CONFIG_BYTES} bytes",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("config file '{}' is not valid UTF-8", path.display()))
}

fn parse_config(text: &str) -> Result<ParsedConfig, String> {
    let mut seen = BTreeSet::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            return Err("tables are not supported".to_owned());
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("expected key = \"value\"".to_owned());
        };
        let key = key.trim();
        if !seen.insert(key) {
            return Err(format!("duplicate key '{key}'"));
        }
        if !is_single_line_quoted_string(value) {
            return Err(
                "values must be single-line, double-quoted or single-quoted strings".to_owned(),
            );
        }
    }
    let values = toml::from_str::<BTreeMap<String, String>>(text)
        .map_err(|_| "invalid TOML syntax; values must be quoted strings".to_owned())?;
    let mut parsed = ParsedConfig::default();
    for (key, value) in values {
        if value.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        if value.chars().any(char::is_control) {
            return Err(format!("{key} must not contain control characters"));
        }
        match key.as_str() {
            "provider" => parsed.provider = Some(value),
            "model" => parsed.model = Some(value),
            "routing_mode" => {
                let mode = match value.as_str() {
                    "manual" => RoutingMode::Manual,
                    "automatic" => RoutingMode::Automatic,
                    _ => return Err("routing_mode must be manual or automatic".to_owned()),
                };
                parsed.routing_mode = Some(mode);
            }
            "allow_cross_provider_data_transfer" => {
                let enabled = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(
                            "allow_cross_provider_data_transfer must be true or false".to_owned()
                        )
                    }
                };
                parsed.allow_cross_provider_data_transfer = Some(enabled);
            }
            "fallback_budget_micro_usd" => {
                let budget = value
                    .parse::<u64>()
                    .ok()
                    .filter(|budget| *budget > 0 && *budget <= MAX_FALLBACK_BUDGET_MICRO_USD)
                    .ok_or_else(|| {
                        format!(
                            "fallback budget must be between 1 and {MAX_FALLBACK_BUDGET_MICRO_USD} micro-USD"
                        )
                    })?;
                parsed.fallback_budget_micro_usd = Some(budget);
            }
            _ => {
                return Err(
                    "unknown key (supported keys: provider, model, routing_mode, allow_cross_provider_data_transfer, fallback_budget_micro_usd)"
                        .to_owned(),
                );
            }
        }
    }
    Ok(parsed)
}

fn is_single_line_quoted_string(value: &str) -> bool {
    let value = value.trim_start();
    let Some(quote) = value
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))
    else {
        return false;
    };
    if value.starts_with(&quote.to_string().repeat(3)) {
        return false;
    }

    let mut escaped = false;
    let mut closing_quote = None;
    for (index, character) in value[1..].char_indices() {
        if quote == '"' && escaped {
            escaped = false;
        } else if quote == '"' && character == '\\' {
            escaped = true;
        } else if character == quote {
            closing_quote = Some(index);
            break;
        }
    }
    let Some(closing_quote) = closing_quote else {
        return false;
    };
    let trailing = value[closing_quote + 2..].trim();
    trailing.is_empty() || trailing.starts_with('#')
}

pub(super) const MAX_FALLBACK_BUDGET_MICRO_USD: u64 = 1_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("krypper-config-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, text: &[u8]) -> PathBuf {
            let path = self.0.join("config.toml");
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn os(value: &str) -> Option<OsString> {
        Some(OsString::from(value))
    }

    #[test]
    fn path_prefers_explicit_then_xdg_then_home() {
        assert_eq!(
            config_path(os("/etc/k.toml"), os("/xdg"), os("/home/u")).unwrap(),
            Some(PathBuf::from("/etc/k.toml"))
        );
        assert_eq!(
            config_path(None, os("/xdg"), os("/home/u")).unwrap(),
            Some(PathBuf::from("/xdg/krypper/config.toml"))
        );
        assert_eq!(
            config_path(None, None, os("/home/u")).unwrap(),
            Some(PathBuf::from("/home/u/.config/krypper/config.toml"))
        );
    }

    #[test]
    fn path_ignores_empty_and_relative_base_directories() {
        assert_eq!(
            config_path(os(""), os(""), os("/home/u")).unwrap(),
            Some(PathBuf::from("/home/u/.config/krypper/config.toml"))
        );
        assert_eq!(
            config_path(None, os("relative"), os("/home/u")).unwrap(),
            Some(PathBuf::from("/home/u/.config/krypper/config.toml"))
        );
        assert_eq!(
            config_path(None, os("relative"), os("also/relative")).unwrap(),
            None
        );
        assert_eq!(config_path(None, None, None).unwrap(), None);
    }

    #[test]
    fn path_rejects_a_relative_explicit_override() {
        assert!(config_path(os("config.toml"), None, os("/home/u")).is_err());
    }

    #[test]
    fn parses_quoted_values_comments_and_blank_lines() {
        let parsed = parse_config(
            "# defaults\n\nprovider = \"gemini\" # hosted\n  model=\"gemini-2.5-flash\"\n",
        )
        .unwrap();
        assert_eq!(parsed.provider.as_deref(), Some("gemini"));
        assert_eq!(parsed.model.as_deref(), Some("gemini-2.5-flash"));
    }

    #[test]
    fn accepts_single_quoted_toml_strings() {
        let parsed = parse_config("model = 'qwen3'\n").unwrap();

        assert_eq!(parsed.model.as_deref(), Some("qwen3"));
    }

    #[test]
    fn rejects_unsupported_or_malformed_lines() {
        for (text, fragment) in [
            ("[model]\n", "tables"),
            ("model\n", "expected key"),
            ("model = qwen3\n", "double-quoted"),
            ("model = \"\"\"qwen3\"\"\"\n", "double-quoted"),
            ("model = \"qwen3\n", "double-quoted"),
            ("model = \"qwen3\" extra\n", "double-quoted"),
            ("model = \"\"\n", "must not be empty"),
            ("model = \"a\\nb\"\n", "control characters"),
            ("api_key = \"secret\"\n", "unknown key"),
            ("model = \"a\"\nmodel = \"b\"\n", "duplicate"),
        ] {
            let error = parse_config(text).unwrap_err();
            assert!(error.contains(fragment), "{text:?} -> {error}");
        }
    }

    #[test]
    fn errors_never_echo_secret_looking_values() {
        let error = parse_config("api_key = \"sk-very-secret\"\n").unwrap_err();
        assert!(!error.contains("sk-very-secret"), "{error}");
    }

    #[test]
    fn missing_file_and_empty_file_yield_no_defaults() {
        let directory = TempDir::new();
        assert_eq!(
            load_model_defaults(&directory.0.join("absent.toml")).unwrap(),
            None
        );
        let path = directory.write(b"# nothing here\n");
        assert_eq!(load_model_defaults(&path).unwrap(), None);
    }

    #[test]
    fn loads_model_with_optional_provider() {
        let directory = TempDir::new();
        let path = directory.write(b"model = \"qwen3\"\n");
        let defaults = load_model_defaults(&path).unwrap().unwrap();
        assert_eq!(defaults.model, "qwen3");
        assert_eq!(defaults.provider, None);
        assert_eq!(defaults.path, path);

        let path = directory.write(b"provider = \"openai\"\nmodel = \"m\"\n");
        let defaults = load_model_defaults(&path).unwrap().unwrap();
        assert_eq!(defaults.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn loads_automatic_routing_preferences_without_changing_manual_defaults() {
        let directory = TempDir::new();
        let path = directory.write(
            b"provider = \"openai\"\nmodel = \"gpt-4.1\"\nrouting_mode = \"automatic\"\nallow_cross_provider_data_transfer = \"true\"\nfallback_budget_micro_usd = \"2500000\"\n",
        );
        let defaults = load_model_defaults(&path).unwrap().unwrap();
        assert_eq!(defaults.routing_mode, RoutingMode::Automatic);
        assert!(defaults.allow_cross_provider_data_transfer);
        assert_eq!(defaults.fallback_budget_micro_usd, Some(2_500_000));

        let path = directory.write(b"provider = \"openai\"\nmodel = \"gpt-4.1\"\n");
        let defaults = load_model_defaults(&path).unwrap().unwrap();
        assert_eq!(defaults.routing_mode, RoutingMode::Manual);
        assert!(!defaults.allow_cross_provider_data_transfer);
        assert_eq!(defaults.fallback_budget_micro_usd, None);
    }

    #[test]
    fn rejects_fallback_settings_without_automatic_routing() {
        let directory = TempDir::new();
        let path = directory.write(
            b"provider = \"openai\"\nmodel = \"gpt-4.1\"\nfallback_budget_micro_usd = \"1000000\"\n",
        );
        assert!(load_model_defaults(&path)
            .unwrap_err()
            .contains("require automatic routing"));
    }

    #[test]
    fn provider_without_model_is_rejected() {
        let directory = TempDir::new();
        let path = directory.write(b"provider = \"openai\"\n");
        let error = load_model_defaults(&path).unwrap_err();
        assert!(error.contains("provider requires model"), "{error}");
    }

    #[test]
    fn rejects_oversized_non_utf8_and_non_regular_files() {
        let directory = TempDir::new();
        let oversized = directory.write(&vec![b'#'; MAX_CONFIG_BYTES as usize + 1]);
        assert!(load_model_defaults(&oversized)
            .unwrap_err()
            .contains("larger than"));

        let invalid = directory.write(&[0xff, 0xfe, b'\n']);
        assert!(load_model_defaults(&invalid).unwrap_err().contains("UTF-8"));

        assert!(load_model_defaults(&directory.0)
            .unwrap_err()
            .contains("not a regular file"));
    }

    #[test]
    fn accepts_a_file_exactly_at_the_size_limit() {
        let directory = TempDir::new();
        let path = directory.write(&vec![b'#'; MAX_CONFIG_BYTES as usize]);
        assert_eq!(load_model_defaults(&path).unwrap(), None);
    }
}
