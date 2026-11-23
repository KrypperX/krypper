//! Interactive, per-user model and provider configuration.

use super::config::{load_model_defaults, ModelDefaults};
use super::help::{usage_error, write_setup_help};
use super::settings::{
    load_credentials, remove_settings, save_credentials, save_model_defaults, user_settings_paths,
    valid_credential, CredentialStore, UserSettingsPaths,
};
use crate::brain::{validate_model_name_for_provider, CloudflareAccountId, CloudflareGatewayId};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

#[derive(Clone, Copy)]
struct ProviderChoice {
    id: &'static str,
    label: &'static str,
}

const PROVIDERS: &[ProviderChoice] = &[
    ProviderChoice {
        id: "ollama",
        label: "Ollama (local)",
    },
    ProviderChoice {
        id: "gemini",
        label: "Google Gemini",
    },
    ProviderChoice {
        id: "openai",
        label: "OpenAI",
    },
    ProviderChoice {
        id: "xai",
        label: "xAI",
    },
    ProviderChoice {
        id: "anthropic",
        label: "Anthropic",
    },
    ProviderChoice {
        id: "cloudflare",
        label: "Cloudflare Workers AI (direct)",
    },
    ProviderChoice {
        id: "cloudflare-ai-gateway",
        label: "Cloudflare AI Gateway",
    },
    ProviderChoice {
        id: "groq",
        label: "Groq",
    },
    ProviderChoice {
        id: "openrouter",
        label: "OpenRouter",
    },
    ProviderChoice {
        id: "deepseek",
        label: "DeepSeek",
    },
    ProviderChoice {
        id: "zai",
        label: "Z.AI",
    },
    ProviderChoice {
        id: "mistral",
        label: "Mistral",
    },
];

pub(super) fn run(
    arguments: &[OsString],
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        write_setup_help(output)?;
        return Ok(0);
    }

    let mode = match arguments {
        [] => SetupMode::Configure,
        [argument] if argument == "--status" => SetupMode::Status,
        [argument] if argument == "--reset" => SetupMode::Reset,
        _ => return usage_error(error, Some("setup"), "unrecognized or duplicate option"),
    };

    if mode == SetupMode::Status {
        return show_status(output);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return usage_error(
            error,
            Some("setup"),
            "setup requires an interactive terminal; set provider environment variables to configure non-interactive runs",
        );
    }

    let paths = match user_settings_paths() {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            return usage_error(
                error,
                Some("setup"),
                "cannot find a user configuration directory; set XDG_CONFIG_HOME or HOME",
            )
        }
        Err(message) => return usage_error(error, Some("setup"), &message),
    };

    if mode == SetupMode::Reset {
        return reset_settings(&paths, input, output, error);
    }
    configure(&paths, input, output, error)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    Configure,
    Status,
    Reset,
}

fn show_status(output: &mut dyn Write) -> io::Result<i32> {
    let paths = match user_settings_paths() {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            writeln!(output, "No per-user configuration directory is available.")?;
            return Ok(0);
        }
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    let defaults = match load_model_defaults(&paths.model) {
        Ok(defaults) => defaults,
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    let credentials = match load_credentials(&paths.credentials) {
        Ok(credentials) => credentials,
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    let credential_count = credentials.as_ref().map_or(0, |store| store.len());

    write_status(
        output,
        defaults.as_ref(),
        credential_count,
        &paths,
        has_config_override(),
    )?;
    Ok(0)
}

fn write_status(
    output: &mut dyn Write,
    defaults: Option<&ModelDefaults>,
    credential_count: usize,
    paths: &UserSettingsPaths,
    config_override: bool,
) -> io::Result<()> {
    match defaults {
        Some(defaults) => {
            let provider = defaults.provider.as_deref().unwrap_or("ollama");
            writeln!(
                output,
                "Provider: {} ({provider})\nModel: {}\nDefaults file: {}",
                provider_label(provider),
                defaults.model,
                paths.model.display()
            )?;
        }
        None => writeln!(output, "No saved provider/model defaults.")?,
    }
    if credential_count == 0 {
        writeln!(output, "Stored credentials: none")?;
    } else {
        writeln!(
            output,
            "Stored credentials: present ({credential_count} field(s); values hidden)"
        )?;
        writeln!(output, "Credential file: {}", paths.credentials.display())?;
    }
    if config_override {
        writeln!(
            output,
            "KRYPPER_CONFIG is set and overrides these saved model defaults for task runs."
        )?;
    }
    writeln!(
        output,
        "Environment values override saved credentials. Use `krypper setup --reset` to remove these per-user settings."
    )?;
    Ok(())
}

fn reset_settings(
    paths: &UserSettingsPaths,
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if !prompt_yes_no(
        input,
        output,
        "Remove saved provider/model defaults and stored credentials from this user account? [y/N] ",
        false,
    )? {
        writeln!(output, "No settings removed.")?;
        return Ok(0);
    }
    match remove_settings(paths) {
        Ok(()) => {
            writeln!(output, "Per-user provider settings removed.")?;
            Ok(0)
        }
        Err(message) => usage_error(error, Some("setup"), &message),
    }
}

fn configure(
    paths: &UserSettingsPaths,
    input: &mut dyn Read,
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    let existing_defaults = match load_model_defaults(&paths.model) {
        Ok(defaults) => defaults,
        Err(message) => return usage_error(error, Some("setup"), &message),
    };
    let mut credentials = match load_credentials(&paths.credentials) {
        Ok(Some(credentials)) => credentials,
        Ok(None) => CredentialStore::empty(),
        Err(message) => return usage_error(error, Some("setup"), &message),
    };
    if existing_defaults.is_some() || !credentials.is_empty() {
        writeln!(
            output,
            "Existing settings found. Continuing replaces the provider/model defaults and updates the selected provider's credentials; credentials for other providers are kept."
        )?;
        if !prompt_yes_no(input, output, "Continue? [y/N] ", false)? {
            writeln!(output, "No settings changed.")?;
            return Ok(0);
        }
    }

    let Some(provider) = choose_provider(input, output)? else {
        writeln!(output, "Setup cancelled; no settings changed.")?;
        return Ok(0);
    };
    let Some(model) = choose_model(input, output, provider)? else {
        writeln!(output, "Setup cancelled; no settings changed.")?;
        return Ok(0);
    };
    if let Err(message) = collect_credentials(input, output, provider, &mut credentials) {
        return usage_error(error, Some("setup"), &message);
    }

    let selected_credential_count = provider_secret_names(provider.id)
        .iter()
        .filter(|name| credentials.get(name).is_some())
        .count();
    write_review(output, paths, provider, &model, selected_credential_count)?;
    if !prompt_yes_no(
        input,
        output,
        "Save these settings for this user? [y/N] ",
        false,
    )? {
        writeln!(output, "No settings changed.")?;
        return Ok(0);
    }

    if !credentials.is_empty() {
        if let Err(message) = save_credentials(&paths.credentials, &credentials) {
            return usage_error(error, Some("setup"), &message);
        }
    }
    if let Err(message) = save_model_defaults(&paths.model, provider.id, &model) {
        return usage_error(error, Some("setup"), &message);
    }
    writeln!(
        output,
        "Saved {} with model `{model}` for this user.\nModel defaults: {}",
        provider.label,
        paths.model.display()
    )?;
    if !provider_secret_names(provider.id).is_empty() {
        writeln!(
            output,
            "Credentials were saved locally. Unix restricts access to your user; other systems use permissions inherited from the per-user config directory. The credential file is not encrypted."
        )?;
        writeln!(
            output,
            "Environment variables override saved values; unset an existing variable to use the saved value."
        )?;
    } else {
        writeln!(output, "Ollama does not need an API credential.")?;
    }
    if has_config_override() {
        writeln!(
            output,
            "KRYPPER_CONFIG is set and overrides these saved model defaults for task runs."
        )?;
    }

    let test_prompt = if provider.id == "ollama" {
        "Send one short request to the local Ollama instance to test it? [y/N] "
    } else {
        "Send one short request to test this provider? It sends a prompt to the selected service and may incur charges. [y/N] "
    };
    if prompt_yes_no(input, output, test_prompt, false)? {
        test_connection(provider, &model, output)?;
    }
    Ok(0)
}

fn write_review(
    output: &mut dyn Write,
    paths: &UserSettingsPaths,
    provider: ProviderChoice,
    model: &str,
    credential_count: usize,
) -> io::Result<()> {
    writeln!(output, "\nReview your setup:")?;
    writeln!(output, "  Provider: {}", provider.label)?;
    writeln!(output, "  Model: {model}")?;
    if credential_count == 0 {
        writeln!(output, "  Credentials: none required")?;
    } else {
        writeln!(
            output,
            "  Credentials: {credential_count} field(s); values hidden"
        )?;
        writeln!(output, "  Credential file: {}", paths.credentials.display())?;
        writeln!(
            output,
            "  Credential file is local plain text, not encrypted."
        )?;
    }
    writeln!(output, "  Model defaults: {}", paths.model.display())?;
    Ok(())
}

fn has_config_override() -> bool {
    std::env::var_os("KRYPPER_CONFIG").is_some_and(|value| !value.as_os_str().is_empty())
}

fn choose_provider(
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> io::Result<Option<ProviderChoice>> {
    writeln!(output, "Choose a model provider:")?;
    for (index, provider) in PROVIDERS.iter().enumerate() {
        writeln!(output, "  {}. {}", index + 1, provider.label)?;
    }
    loop {
        let Some(value) = prompt_line(input, output, "Provider [1; q cancels]: ")? else {
            return Ok(None);
        };
        if value.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if value.trim().is_empty() {
            return Ok(Some(PROVIDERS[0]));
        }
        if let Ok(index) = value.trim().parse::<usize>() {
            if let Some(provider) = index.checked_sub(1).and_then(|index| PROVIDERS.get(index)) {
                return Ok(Some(*provider));
            }
        }
        writeln!(
            output,
            "Choose a number from 1 to {} or enter q.",
            PROVIDERS.len()
        )?;
    }
}

fn choose_model(
    input: &mut dyn Read,
    output: &mut dyn Write,
    provider: ProviderChoice,
) -> io::Result<Option<String>> {
    let Some(query) = prompt_line(
        input,
        output,
        "Search the offline model catalog by name or ID (Enter to type an ID; q cancels): ",
    )?
    else {
        return Ok(None);
    };
    let query = query.trim();
    if query.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let query = query.to_owned();

    if query.is_empty() {
        return prompt_model_id(input, output, provider);
    }
    let all_models = crate::catalog_data::models().map_err(io::Error::other)?;
    if all_models.iter().any(|model| {
        crate::catalog_data::provider_matches(provider.id, &model.provider)
            && model.id.eq_ignore_ascii_case(&query)
    }) {
        return validate_model(provider.id, &query)
            .map(Some)
            .map_err(io::Error::other);
    }
    let query = query.to_ascii_lowercase();
    let candidates: Vec<_> = all_models
        .iter()
        .filter(|model| crate::catalog_data::provider_matches(provider.id, &model.provider))
        .filter(|model| {
            model.id.to_ascii_lowercase().contains(&query)
                || model.name.to_ascii_lowercase().contains(&query)
        })
        .take(20)
        .collect();
    if candidates.is_empty() {
        writeln!(
            output,
            "No catalog matches. You can still enter a model ID."
        )?;
        return prompt_model_id(input, output, provider);
    }
    for (index, model) in candidates.iter().enumerate() {
        writeln!(
            output,
            "  {}. {}",
            index + 1,
            escape_terminal_text(&model.id)
        )?;
    }
    if candidates.len() == 20 {
        writeln!(
            output,
            "Showing the first 20 matches; use a narrower search if needed."
        )?;
    }
    loop {
        let Some(selection) = prompt_line(
            input,
            output,
            "Choose a number or enter a custom model ID [1; q cancels]: ",
        )?
        else {
            return Ok(None);
        };
        let selection = selection.trim();
        if selection.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if selection.is_empty() {
            return Ok(Some(candidates[0].id.clone()));
        }
        if let Ok(index) = selection.parse::<usize>() {
            if let Some(candidate) = index.checked_sub(1).and_then(|index| candidates.get(index)) {
                return Ok(Some(candidate.id.clone()));
            }
        }
        match validate_model(provider.id, selection) {
            Ok(model) => return Ok(Some(model)),
            Err(message) => writeln!(output, "{message}")?,
        }
    }
}

fn prompt_model_id(
    input: &mut dyn Read,
    output: &mut dyn Write,
    provider: ProviderChoice,
) -> io::Result<Option<String>> {
    loop {
        let Some(model) = prompt_line(input, output, "Model ID (q cancels): ")? else {
            return Ok(None);
        };
        let model = model.trim();
        if model.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        match validate_model(provider.id, model) {
            Ok(model) => return Ok(Some(model)),
            Err(message) => writeln!(output, "{message}")?,
        }
    }
}

fn validate_model(provider: &str, model: &str) -> Result<String, &'static str> {
    let provider = crate::brain::ModelProvider::parse(provider).ok_or("unsupported provider")?;
    validate_model_name_for_provider(model, provider)?;
    Ok(model.to_owned())
}

fn collect_credentials(
    input: &mut dyn Read,
    output: &mut dyn Write,
    provider: ProviderChoice,
    credentials: &mut CredentialStore,
) -> Result<(), String> {
    match provider.id {
        "ollama" => Ok(()),
        "cloudflare" | "cloudflare-ai-gateway" => {
            let account_id = prompt_required_line(input, output, "Cloudflare Account ID: ")?;
            CloudflareAccountId::try_from(account_id.as_str())
                .map_err(|_| "invalid Cloudflare Account ID".to_owned())?;
            credentials.insert("CLOUDFLARE_ACCOUNT_ID", account_id);
            let token = prompt_required_secret(output, "Cloudflare API token: ")?;
            credentials.insert("CLOUDFLARE_API_TOKEN", token);
            if provider.id == "cloudflare-ai-gateway" {
                let gateway_id = prompt_line(input, output, "AI Gateway ID [default]: ")
                    .map_err(|_| "could not read interactive input".to_owned())?
                    .ok_or_else(|| "setup cancelled; no settings were changed".to_owned())?;
                let gateway_id = if gateway_id.trim().is_empty() {
                    credentials.remove("CLOUDFLARE_AI_GATEWAY_ID");
                    None
                } else {
                    let gateway_id = gateway_id.trim().to_owned();
                    CloudflareGatewayId::try_from(gateway_id.as_str())
                        .map_err(|_| "invalid Cloudflare AI Gateway ID".to_owned())?;
                    Some(gateway_id)
                };
                if let Some(gateway_id) = gateway_id {
                    credentials.insert("CLOUDFLARE_AI_GATEWAY_ID", gateway_id);
                }
            }
            Ok(())
        }
        "anthropic" => {
            let key = prompt_required_secret(output, "Anthropic API key: ")?;
            credentials.insert("ANTHROPIC_API_KEY", key);
            let workspace_id = prompt_line(
                input,
                output,
                "Anthropic Workspace ID (optional; Enter to skip): ",
            )
            .map_err(|_| "could not read interactive input".to_owned())?
            .ok_or_else(|| "setup cancelled; no settings were changed".to_owned())?;
            if workspace_id.trim().is_empty() {
                credentials.remove("ANTHROPIC_WORKSPACE_ID");
            } else {
                let workspace_id = workspace_id.trim();
                if workspace_id.len() > 128
                    || !workspace_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                {
                    return Err("invalid Anthropic Workspace ID".to_owned());
                }
                credentials.insert("ANTHROPIC_WORKSPACE_ID", workspace_id.to_owned());
            }
            Ok(())
        }
        "gemini" => {
            credentials.insert(
                "GEMINI_API_KEY",
                prompt_required_secret(output, "Gemini API key: ")?,
            );
            Ok(())
        }
        "openai" => {
            credentials.insert(
                "OPENAI_API_KEY",
                prompt_required_secret(output, "OpenAI API key: ")?,
            );
            Ok(())
        }
        "xai" => {
            credentials.insert(
                "XAI_API_KEY",
                prompt_required_secret(output, "xAI API key: ")?,
            );
            Ok(())
        }
        "groq" => {
            credentials.insert(
                "GROQ_API_KEY",
                prompt_required_secret(output, "Groq API key: ")?,
            );
            Ok(())
        }
        "openrouter" => {
            credentials.insert(
                "OPENROUTER_API_KEY",
                prompt_required_secret(output, "OpenRouter API key: ")?,
            );
            Ok(())
        }
        "deepseek" => {
            credentials.insert(
                "DEEPSEEK_API_KEY",
                prompt_required_secret(output, "DeepSeek API key: ")?,
            );
            Ok(())
        }
        "zai" => {
            credentials.insert(
                "ZAI_API_KEY",
                prompt_required_secret(output, "Z.AI API key: ")?,
            );
            Ok(())
        }
        "mistral" => {
            credentials.insert(
                "MISTRAL_API_KEY",
                prompt_required_secret(output, "Mistral API key: ")?,
            );
            Ok(())
        }
        _ => Err("unsupported provider".to_owned()),
    }
}

fn prompt_required_secret(output: &mut dyn Write, prompt: &str) -> Result<String, String> {
    loop {
        let value = rpassword::prompt_password(prompt)
            .map_err(|_| "could not read hidden input; no credential was saved".to_owned())?;
        if valid_credential(&value) {
            return Ok(value);
        }
        writeln!(
            output,
            "Value must be nonempty and contain no surrounding whitespace or control characters."
        )
        .map_err(|_| "could not display setup prompt".to_owned())?;
    }
}

fn prompt_required_line(
    input: &mut dyn Read,
    output: &mut dyn Write,
    prompt: &str,
) -> Result<String, String> {
    let value = prompt_line(input, output, prompt)
        .map_err(|_| "could not read interactive input".to_owned())?
        .ok_or_else(|| "setup cancelled; no settings were changed".to_owned())?;
    let value = value.trim().to_owned();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("value must be nonempty and contain no control characters".to_owned());
    }
    Ok(value)
}

fn prompt_yes_no(
    input: &mut dyn Read,
    output: &mut dyn Write,
    prompt: &str,
    default: bool,
) -> io::Result<bool> {
    loop {
        let Some(value) = prompt_line(input, output, prompt)? else {
            return Ok(false);
        };
        match value.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Please answer y or n.")?,
        }
    }
}

fn prompt_line(
    input: &mut dyn Read,
    output: &mut dyn Write,
    prompt: &str,
) -> io::Result<Option<String>> {
    write!(output, "{prompt}")?;
    output.flush()?;
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => break,
            _ if byte[0] == b'\n' => break,
            _ if byte[0] != b'\r' => bytes.push(byte[0]),
            _ => {}
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "input must be UTF-8"))
}

fn provider_secret_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "gemini" => &["GEMINI_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY", "ANTHROPIC_WORKSPACE_ID"],
        "cloudflare" => &["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"],
        "cloudflare-ai-gateway" => &[
            "CLOUDFLARE_ACCOUNT_ID",
            "CLOUDFLARE_API_TOKEN",
            "CLOUDFLARE_AI_GATEWAY_ID",
        ],
        "groq" => &["GROQ_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        _ => &[],
    }
}

fn provider_label(provider: &str) -> &str {
    PROVIDERS
        .iter()
        .find(|choice| choice.id == provider)
        .map_or(provider, |choice| choice.label)
}

fn escape_terminal_text(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn test_connection(
    provider: ProviderChoice,
    model: &str,
    output: &mut dyn Write,
) -> io::Result<()> {
    let workspace = TempWorkspace::new()?;
    let arguments = [
        OsString::from("--goal"),
        OsString::from("Reply with a short confirmation and finish immediately."),
        OsString::from("--max-actions"),
        OsString::from("1"),
        OsString::from("--provider"),
        OsString::from(provider.id),
        OsString::from("--model"),
        OsString::from(model),
        OsString::from("--workspace"),
        workspace.0.as_os_str().to_owned(),
    ];
    let mut input = io::empty();
    let mut test_output = io::sink();
    let mut test_error = io::sink();
    match super::task::run_task(&arguments, &mut input, &mut test_output, &mut test_error) {
        Ok(0) => writeln!(output, "The provider test request completed."),
        _ => writeln!(
            output,
            "The provider test did not complete. Settings remain saved; no credential value was displayed."
        ),
    }
}

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> io::Result<Self> {
        for attempt in 0..16_u8 {
            let path = std::env::temp_dir().join(format!(
                "krypper-setup-test-{}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(cause) => return Err(cause),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temporary provider test workspace",
        ))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_list_matches_every_supported_cli_provider() {
        for provider in PROVIDERS {
            assert!(crate::brain::ModelProvider::parse(provider.id).is_some());
        }
        assert_eq!(
            PROVIDERS
                .iter()
                .find(|provider| provider.id == "cloudflare")
                .unwrap()
                .label,
            "Cloudflare Workers AI (direct)"
        );
        assert_eq!(
            PROVIDERS
                .iter()
                .find(|provider| provider.id == "cloudflare-ai-gateway")
                .unwrap()
                .label,
            "Cloudflare AI Gateway"
        );
    }

    #[test]
    fn catalog_search_keeps_gateway_and_workers_ai_models_available() {
        let models = crate::catalog_data::models().unwrap();
        for provider in ["cloudflare", "cloudflare-ai-gateway"] {
            assert!(models
                .iter()
                .any(|model| { crate::catalog_data::provider_matches(provider, &model.provider) }));
        }
    }

    #[test]
    fn catalog_labels_cannot_emit_terminal_control_sequences() {
        assert_eq!(escape_terminal_text("model\u{1b}[31m"), "model\\u{1b}[31m");
    }

    #[test]
    fn setup_status_does_not_render_saved_credential_values() {
        let secret = "must-not-appear-in-status";
        let credentials = {
            let mut credentials = CredentialStore::empty();
            credentials.insert("OPENAI_API_KEY", secret.to_owned());
            credentials
        };
        let paths = UserSettingsPaths {
            model: PathBuf::from("/user/config/krypper/config.toml"),
            credentials: PathBuf::from("/user/config/krypper/credentials.json"),
        };
        let defaults = ModelDefaults {
            model: "test-model".to_owned(),
            provider: Some("openai".to_owned()),
            path: paths.model.clone(),
        };
        let mut output = Vec::new();
        write_status(
            &mut output,
            Some(&defaults),
            credentials.len(),
            &paths,
            false,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(secret));
        assert!(output.contains("values hidden"));
    }

    #[test]
    fn setup_review_hides_credential_values() {
        let secret = "review-secret-must-not-appear";
        let mut credentials = CredentialStore::empty();
        credentials.insert("OPENAI_API_KEY", secret.to_owned());
        let paths = UserSettingsPaths {
            model: PathBuf::from("/user/config/krypper/config.toml"),
            credentials: PathBuf::from("/user/config/krypper/credentials.json"),
        };
        let provider = *PROVIDERS
            .iter()
            .find(|provider| provider.id == "openai")
            .unwrap();
        let mut output = Vec::new();
        write_review(&mut output, &paths, provider, "gpt-4.1", credentials.len()).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("OpenAI"));
        assert!(output.contains("gpt-4.1"));
        assert!(output.contains("values hidden"));
        assert!(!output.contains(secret));
    }
}
