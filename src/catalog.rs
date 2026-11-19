//! Central model catalog.
//!
//! The catalog records what Krypper knows about a model, keyed by provider name
//! and model id: context window, output limit, and tool-calling and reasoning
//! support. Three rules shape it:
//!
//! * **A model that is not listed still works.** The catalog can only add
//!   knowledge; it never allows or denies a model, and it is never a security
//!   boundary. Every field is optional and defaults to "unknown".
//! * **No network access.** Entries are compiled in or read from one local
//!   file the user controls. Nothing is fetched at run time, so the privacy
//!   contract that task data goes only where the user selected stays intact.
//! * **Only documented facts.** A built-in value is added only when the
//!   provider's own documentation states it. Values that cannot be verified
//!   stay unknown instead of being guessed, because a wrong limit is worse
//!   than a missing one. Maintainers record the documentation page in the
//!   commit message that adds or changes an entry.
//!
//! Users extend or override the built-in entries with a `models.toml` file
//! written in the same small TOML subset as the configuration file:
//!
//! ```toml
//! version = 1
//!
//! [[model]]
//! provider = "ollama"
//! id = "qwen3:8b"
//! label = "Qwen3 8B (local)"
//! context_window = 32768
//! max_output_tokens = 4096
//! tool_calling = "yes"   # "yes", "no" or "unknown"
//! reasoning = "unknown"
//! ```

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const MAX_CATALOG_BYTES: u64 = 64 * 1024;
const MAX_USER_ENTRIES: usize = 512;
const MAX_ID_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_TOKEN_COUNT: u32 = 100_000_000;
const SUPPORTED_VERSION: u32 = 1;

/// Whether a model offers a capability, when that is known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Support {
    Yes,
    No,
    Unknown,
}

impl Support {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "yes" => Some(Self::Yes),
            "no" => Some(Self::No),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Where a catalog entry came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    BuiltIn,
    User,
}

impl Origin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::User => "user",
        }
    }
}

/// Everything the catalog records about one model of one provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelEntry {
    pub(crate) provider: String,
    pub(crate) id: String,
    pub(crate) label: Option<String>,
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) tool_calling: Support,
    pub(crate) reasoning: Support,
    pub(crate) origin: Origin,
}

struct Seed {
    provider: &'static str,
    id: &'static str,
    label: &'static str,
    tool_calling: Support,
}

/// Built-in entries. Limits stay unset until a provider document states them.
const BUILT_IN: &[Seed] = &[
    Seed {
        provider: "anthropic",
        id: "claude-fable-5-2",
        label: "Claude Fable 5.2",
        tool_calling: Support::Yes,
    },
    Seed {
        provider: "anthropic",
        id: "claude-opus-5-5",
        label: "Claude Opus 5.5",
        tool_calling: Support::Yes,
    },
    Seed {
        provider: "anthropic",
        id: "claude-sonnet-5-5",
        label: "Claude Sonnet 5.5",
        tool_calling: Support::Yes,
    },
    Seed {
        provider: "anthropic",
        id: "claude-haiku-5-5",
        label: "Claude Haiku 5.5",
        tool_calling: Support::Yes,
    },
];

/// The built-in entries merged with any user entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Catalog {
    entries: Vec<ModelEntry>,
}

impl Catalog {
    pub(crate) fn built_in() -> Self {
        let entries = BUILT_IN
            .iter()
            .map(|seed| ModelEntry {
                provider: seed.provider.to_owned(),
                id: seed.id.to_owned(),
                label: Some(seed.label.to_owned()),
                context_window: None,
                max_output_tokens: None,
                tool_calling: seed.tool_calling,
                reasoning: Support::Unknown,
                origin: Origin::BuiltIn,
            })
            .collect();
        Self { entries }
    }

    /// Adds user entries; a user entry replaces a built-in entry for the same
    /// provider and id.
    pub(crate) fn with_user_entries(mut self, user: Vec<ModelEntry>) -> Self {
        for entry in user {
            self.entries
                .retain(|known| !(known.provider == entry.provider && known.id == entry.id));
            self.entries.push(entry);
        }
        self.entries
            .sort_by(|left, right| (&left.provider, &left.id).cmp(&(&right.provider, &right.id)));
        self
    }

    pub(crate) fn entries(&self) -> &[ModelEntry] {
        &self.entries
    }

    /// Finds one model by exact provider name and model id.
    ///
    /// `None` means the catalog has no knowledge of the model, not that the
    /// model is unusable.
    pub(crate) fn lookup(&self, provider: &str, id: &str) -> Option<&ModelEntry> {
        self.entries
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)
    }
}

/// Reads and parses a user catalog file.
///
/// Returns `Ok(None)` when the file does not exist. Any other problem is
/// reported, so a broken file is never silently ignored.
pub(crate) fn load_user_catalog(
    path: &Path,
    known_provider: &dyn Fn(&str) -> bool,
) -> Result<Option<Vec<ModelEntry>>, String> {
    let Some(text) = read_catalog_text(path)? else {
        return Ok(None);
    };
    parse_user_catalog(&text, known_provider)
        .map(Some)
        .map_err(|cause| format!("invalid model catalog '{}': {cause}", path.display()))
}

fn read_catalog_text(path: &Path) -> Result<Option<String>, String> {
    let read_error = |cause: io::Error| {
        format!(
            "cannot read model catalog '{}': {}",
            path.display(),
            cause.kind()
        )
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(read_error(cause)),
    };
    let metadata = file.metadata().map_err(read_error)?;
    if !metadata.is_file() {
        return Err(format!(
            "model catalog '{}' is not a regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(format!(
            "model catalog '{}' is larger than {MAX_CATALOG_BYTES} bytes",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("model catalog '{}' is not valid UTF-8", path.display()))
}

#[derive(Default)]
struct Draft {
    line: usize,
    provider: Option<String>,
    id: Option<String>,
    label: Option<String>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    tool_calling: Option<Support>,
    reasoning: Option<Support>,
}

enum Value {
    Text(String),
    Number(u32),
}

/// Parses the text of a user catalog file.
pub(crate) fn parse_user_catalog(
    text: &str,
    known_provider: &dyn Fn(&str) -> bool,
) -> Result<Vec<ModelEntry>, String> {
    let mut version_seen = false;
    let mut draft: Option<Draft> = None;
    let mut entries: Vec<ModelEntry> = Vec::new();
    let mut identities: BTreeSet<(String, String)> = BTreeSet::new();

    for (index, raw_line) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let Some(rest) = line.strip_prefix("[[model]]") else {
                return Err(format!(
                    "line {number}: only [[model]] tables are supported"
                ));
            };
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(format!("line {number}: unexpected text after [[model]]"));
            }
            if !version_seen {
                return Err(format!(
                    "line {number}: 'version = {SUPPORTED_VERSION}' must come before the first [[model]]"
                ));
            }
            if let Some(finished) = draft.take() {
                finish_entry(finished, known_provider, &mut identities, &mut entries)?;
            }
            if entries.len() + 1 > MAX_USER_ENTRIES {
                return Err(format!(
                    "line {number}: more than {MAX_USER_ENTRIES} model entries"
                ));
            }
            draft = Some(Draft {
                line: number,
                ..Draft::default()
            });
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {number}: expected key = value"));
        };
        let key = key.trim();
        let value = parse_value(value.trim()).map_err(|cause| format!("line {number}: {cause}"))?;

        let Some(current) = draft.as_mut() else {
            if key != "version" {
                return Err(format!(
                    "line {number}: only 'version' is allowed before the first [[model]]"
                ));
            }
            if version_seen {
                return Err(format!("line {number}: duplicate key 'version'"));
            }
            match value {
                Value::Number(SUPPORTED_VERSION) => version_seen = true,
                _ => {
                    return Err(format!(
                        "line {number}: unsupported version (expected {SUPPORTED_VERSION})"
                    ))
                }
            }
            continue;
        };
        assign_field(current, key, value, number)?;
    }

    if let Some(finished) = draft.take() {
        finish_entry(finished, known_provider, &mut identities, &mut entries)?;
    }
    Ok(entries)
}

fn parse_value(value: &str) -> Result<Value, &'static str> {
    if let Some(rest) = value.strip_prefix('"') {
        let Some(end) = rest.find('"') else {
            return Err("string is missing its closing quote");
        };
        let (text, after) = rest.split_at(end);
        let after = after[1..].trim();
        if !after.is_empty() && !after.starts_with('#') {
            return Err("unexpected text after the string");
        }
        if text.contains('\\') {
            return Err("escape sequences are not supported");
        }
        if text.chars().any(char::is_control) {
            return Err("value must not contain control characters");
        }
        return Ok(Value::Text(text.to_owned()));
    }
    let token = value.split('#').next().unwrap_or("").trim();
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("value must be a double-quoted string or a plain whole number");
    }
    token
        .parse::<u32>()
        .map(Value::Number)
        .map_err(|_| "number is too large")
}

fn assign_field(draft: &mut Draft, key: &str, value: Value, number: usize) -> Result<(), String> {
    fn text(value: Value, number: usize, key: &str) -> Result<String, String> {
        match value {
            Value::Text(text) => Ok(text),
            Value::Number(_) => Err(format!("line {number}: '{key}' must be a quoted string")),
        }
    }
    fn count(value: Value, number: usize, key: &str) -> Result<u32, String> {
        match value {
            Value::Number(count) => Ok(count),
            Value::Text(_) => Err(format!("line {number}: '{key}' must be a whole number")),
        }
    }
    fn support(value: Value, number: usize, key: &str) -> Result<Support, String> {
        let text = text(value, number, key)?;
        Support::parse(&text)
            .ok_or_else(|| format!("line {number}: '{key}' must be \"yes\", \"no\" or \"unknown\""))
    }
    fn set<T>(slot: &mut Option<T>, value: T, number: usize, key: &str) -> Result<(), String> {
        if slot.is_some() {
            return Err(format!("line {number}: duplicate key '{key}'"));
        }
        *slot = Some(value);
        Ok(())
    }

    match key {
        "provider" => set(&mut draft.provider, text(value, number, key)?, number, key),
        "id" => set(&mut draft.id, text(value, number, key)?, number, key),
        "label" => set(&mut draft.label, text(value, number, key)?, number, key),
        "context_window" => set(
            &mut draft.context_window,
            count(value, number, key)?,
            number,
            key,
        ),
        "max_output_tokens" => set(
            &mut draft.max_output_tokens,
            count(value, number, key)?,
            number,
            key,
        ),
        "tool_calling" => set(
            &mut draft.tool_calling,
            support(value, number, key)?,
            number,
            key,
        ),
        "reasoning" => set(&mut draft.reasoning, support(value, number, key)?, number, key),
        _ => Err(format!(
            "line {number}: unknown key (supported keys: provider, id, label, context_window, max_output_tokens, tool_calling, reasoning)"
        )),
    }
}

fn finish_entry(
    draft: Draft,
    known_provider: &dyn Fn(&str) -> bool,
    identities: &mut BTreeSet<(String, String)>,
    entries: &mut Vec<ModelEntry>,
) -> Result<(), String> {
    let line = draft.line;
    let provider = draft
        .provider
        .ok_or_else(|| format!("line {line}: [[model]] is missing 'provider'"))?;
    let id = draft
        .id
        .ok_or_else(|| format!("line {line}: [[model]] is missing 'id'"))?;
    if !known_provider(&provider) {
        return Err(format!("line {line}: unsupported provider '{provider}'"));
    }
    if id.is_empty() || id.trim() != id || id.len() > MAX_ID_BYTES {
        return Err(format!(
            "line {line}: 'id' must be non-empty, have no surrounding whitespace, and be at most {MAX_ID_BYTES} bytes"
        ));
    }
    if draft
        .label
        .as_ref()
        .is_some_and(|label| label.is_empty() || label.len() > MAX_LABEL_BYTES)
    {
        return Err(format!(
            "line {line}: 'label' must be non-empty and at most {MAX_LABEL_BYTES} bytes"
        ));
    }
    for (name, value) in [
        ("context_window", draft.context_window),
        ("max_output_tokens", draft.max_output_tokens),
    ] {
        if value.is_some_and(|count| count == 0 || count > MAX_TOKEN_COUNT) {
            return Err(format!(
                "line {line}: '{name}' must be between 1 and {MAX_TOKEN_COUNT}"
            ));
        }
    }
    if let (Some(context), Some(output)) = (draft.context_window, draft.max_output_tokens) {
        if output > context {
            return Err(format!(
                "line {line}: 'max_output_tokens' must not exceed 'context_window'"
            ));
        }
    }
    if !identities.insert((provider.clone(), id.clone())) {
        return Err(format!(
            "line {line}: duplicate entry for provider '{provider}' and id '{id}'"
        ));
    }
    entries.push(ModelEntry {
        provider,
        id,
        label: draft.label,
        context_window: draft.context_window,
        max_output_tokens: draft.max_output_tokens,
        tool_calling: draft.tool_calling.unwrap_or(Support::Unknown),
        reasoning: draft.reasoning.unwrap_or(Support::Unknown),
        origin: Origin::User,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("krypper-catalog-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, bytes: &[u8]) -> PathBuf {
            let path = self.0.join("models.toml");
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn known(name: &str) -> bool {
        matches!(name, "ollama" | "anthropic" | "openai")
    }

    fn parse(text: &str) -> Result<Vec<ModelEntry>, String> {
        parse_user_catalog(text, &known)
    }

    #[test]
    fn built_in_entries_are_unique_valid_and_conservative() {
        let catalog = Catalog::built_in();
        assert!(!catalog.entries().is_empty());
        let mut seen = BTreeSet::new();
        for entry in catalog.entries() {
            assert!(
                seen.insert((entry.provider.clone(), entry.id.clone())),
                "duplicate built-in entry {}/{}",
                entry.provider,
                entry.id
            );
            assert!(
                !entry.provider.is_empty()
                    && entry
                        .provider
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
                "{}",
                entry.provider
            );
            assert!(!entry.id.is_empty() && entry.id.trim() == entry.id);
            assert!(entry.id.len() <= MAX_ID_BYTES);
            assert_eq!(entry.origin, Origin::BuiltIn);
            for count in [entry.context_window, entry.max_output_tokens]
                .into_iter()
                .flatten()
            {
                assert!(count > 0 && count <= MAX_TOKEN_COUNT);
            }
        }
    }

    #[test]
    fn lookup_is_exact_and_unlisted_models_are_simply_absent() {
        let catalog = Catalog::built_in();
        let entry = catalog.lookup("anthropic", "claude-opus-5-5").unwrap();
        assert_eq!(entry.label.as_deref(), Some("Claude Opus 5.5"));
        assert!(catalog.lookup("anthropic", "claude-opus-5").is_none());
        assert!(catalog.lookup("openai", "claude-opus-5-5").is_none());
        assert!(catalog.lookup("anthropic", "CLAUDE-OPUS-5-5").is_none());
    }

    #[test]
    fn parses_a_full_entry_and_applies_unknown_defaults() {
        let entries = parse(
            "# my models\nversion = 1\n\n[[model]] # local\nprovider = \"ollama\"\nid = \"qwen3:8b\"\nlabel = \"Qwen3 8B\"\ncontext_window = 32768 # tokens\nmax_output_tokens = 4096\ntool_calling = \"yes\"\nreasoning = \"no\"\n\n[[model]]\nprovider = \"openai\"\nid = \"x\"\n",
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].provider, "ollama");
        assert_eq!(entries[0].id, "qwen3:8b");
        assert_eq!(entries[0].label.as_deref(), Some("Qwen3 8B"));
        assert_eq!(entries[0].context_window, Some(32768));
        assert_eq!(entries[0].max_output_tokens, Some(4096));
        assert_eq!(entries[0].tool_calling, Support::Yes);
        assert_eq!(entries[0].reasoning, Support::No);
        assert_eq!(entries[0].origin, Origin::User);
        assert_eq!(entries[1].context_window, None);
        assert_eq!(entries[1].tool_calling, Support::Unknown);
        assert_eq!(entries[1].reasoning, Support::Unknown);
    }

    #[test]
    fn empty_or_version_only_files_have_no_entries() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("# nothing\n").unwrap().is_empty());
        assert!(parse("version = 1\n").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_files_with_a_line_number() {
        let model = "version = 1\n[[model]]\nprovider = \"ollama\"\nid = \"m\"\n";
        for (text, fragment) in [
            (
                "[[model]]\nprovider = \"ollama\"\nid = \"m\"\n",
                "must come before",
            ),
            ("provider = \"ollama\"\n", "only 'version'"),
            ("version = 2\n", "unsupported version"),
            ("version = \"1\"\n", "unsupported version"),
            ("version = 1\nversion = 1\n", "duplicate key 'version'"),
            ("version = 1\n[model]\n", "only [[model]]"),
            ("version = 1\n[[model]] x\n", "after [[model]]"),
            ("version = 1\nnonsense\n", "expected key"),
            ("version = 1\n[[model]]\nid = \"m\"\n", "missing 'provider'"),
            (
                "version = 1\n[[model]]\nprovider = \"ollama\"\n",
                "missing 'id'",
            ),
            (
                "version = 1\n[[model]]\nprovider = \"nope\"\nid = \"m\"\n",
                "unsupported provider",
            ),
            (
                "version = 1\n[[model]]\nprovider = \"ollama\"\nid = \" m\"\n",
                "'id' must be",
            ),
            (
                "version = 1\n[[model]]\nprovider = \"ollama\"\nid = \"\"\n",
                "'id' must be",
            ),
            (format!("{model}label = \"\"\n").as_str(), "'label' must be"),
            (format!("{model}colour = \"red\"\n").as_str(), "unknown key"),
            (
                format!("{model}id = \"again\"\n").as_str(),
                "duplicate key 'id'",
            ),
            (
                format!("{model}context_window = 0\n").as_str(),
                "between 1 and",
            ),
            (
                format!("{model}context_window = -5\n").as_str(),
                "whole number",
            ),
            (
                format!("{model}context_window = 1_000\n").as_str(),
                "whole number",
            ),
            (
                format!("{model}context_window = 99999999999\n").as_str(),
                "too large",
            ),
            (
                format!("{model}context_window = 100000001\n").as_str(),
                "between 1 and",
            ),
            (
                format!("{model}context_window = \"big\"\n").as_str(),
                "whole number",
            ),
            (format!("{model}id2 = 3\n").as_str(), "unknown key"),
            (
                format!("{model}tool_calling = \"maybe\"\n").as_str(),
                "\"yes\", \"no\" or",
            ),
            (
                format!("{model}tool_calling = 1\n").as_str(),
                "quoted string",
            ),
            (format!("{model}label = \"a\\nb\"\n").as_str(), "escape"),
            (format!("{model}label = \"open\n").as_str(), "closing quote"),
            (
                format!("{model}label = \"a\" b\n").as_str(),
                "unexpected text",
            ),
            (
                &format!("{model}context_window = 100\nmax_output_tokens = 101\n"),
                "must not exceed",
            ),
            (
                format!("{model}[[model]]\nprovider = \"ollama\"\nid = \"m\"\n").as_str(),
                "duplicate entry",
            ),
        ] {
            let error = parse(text).unwrap_err();
            assert!(error.contains(fragment), "{text:?} -> {error}");
            assert!(error.starts_with("line "), "{error}");
        }
    }

    #[test]
    fn same_id_under_different_providers_is_allowed() {
        let entries = parse(
            "version = 1\n[[model]]\nprovider = \"ollama\"\nid = \"m\"\n[[model]]\nprovider = \"openai\"\nid = \"m\"\n",
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn rejects_more_entries_than_the_limit() {
        let mut text = String::from("version = 1\n");
        for index in 0..=MAX_USER_ENTRIES {
            text.push_str(&format!(
                "[[model]]\nprovider = \"ollama\"\nid = \"m{index}\"\n"
            ));
        }
        assert!(parse(&text).unwrap_err().contains("more than"));
    }

    #[test]
    fn user_entries_override_built_in_ones_and_the_result_is_sorted() {
        let user = parse(
            "version = 1\n[[model]]\nprovider = \"anthropic\"\nid = \"claude-opus-5-5\"\ncontext_window = 1000\n[[model]]\nprovider = \"ollama\"\nid = \"a\"\n",
        )
        .unwrap();
        let catalog = Catalog::built_in().with_user_entries(user);
        let opus = catalog.lookup("anthropic", "claude-opus-5-5").unwrap();
        assert_eq!(opus.origin, Origin::User);
        assert_eq!(opus.context_window, Some(1000));
        assert_eq!(opus.label, None);
        assert_eq!(
            catalog
                .entries()
                .iter()
                .filter(|entry| entry.id == "claude-opus-5-5")
                .count(),
            1
        );
        assert!(catalog.lookup("ollama", "a").is_some());
        let keys: Vec<_> = catalog
            .entries()
            .iter()
            .map(|entry| (entry.provider.clone(), entry.id.clone()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn missing_file_yields_no_user_entries() {
        let directory = TempDir::new();
        assert_eq!(
            load_user_catalog(&directory.0.join("absent.toml"), &known).unwrap(),
            None
        );
    }

    #[test]
    fn load_reports_the_path_and_never_the_file_contents() {
        let directory = TempDir::new();
        let path = directory.write(
            b"version = 1\n[[model]]\nprovider = \"ollama\"\nid = \"m\"\nsecret = \"sk-hidden\"\n",
        );
        let error = load_user_catalog(&path, &known).unwrap_err();
        assert!(error.contains("invalid model catalog"), "{error}");
        assert!(error.contains("line 5"), "{error}");
        assert!(!error.contains("sk-hidden"), "{error}");
    }

    #[test]
    fn load_rejects_oversized_non_utf8_and_non_regular_files() {
        let directory = TempDir::new();
        let oversized = directory.write(&vec![b'#'; MAX_CATALOG_BYTES as usize + 1]);
        assert!(load_user_catalog(&oversized, &known)
            .unwrap_err()
            .contains("larger than"));
        let invalid = directory.write(&[0xff, 0xfe, b'\n']);
        assert!(load_user_catalog(&invalid, &known)
            .unwrap_err()
            .contains("UTF-8"));
        assert!(load_user_catalog(&directory.0, &known)
            .unwrap_err()
            .contains("not a regular file"));
    }

    #[test]
    fn load_accepts_a_file_exactly_at_the_size_limit() {
        let directory = TempDir::new();
        let path = directory.write(&vec![b'#'; MAX_CATALOG_BYTES as usize]);
        assert_eq!(load_user_catalog(&path, &known).unwrap(), Some(Vec::new()));
    }
}
