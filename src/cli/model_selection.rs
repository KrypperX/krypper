//! Model selection: the user configuration defaults and the provider decision source.

use super::config::{config_path, load_model_defaults};
use crate::brain::{
    validate_model_name_for_provider, AnthropicDecisionSource, CloudflareAccountId,
    GeminiDecisionSource, ModelDecisionSource, ModelProvider, OllamaDecisionSource,
    OpenAiCompatibleCredentials, OpenAiCompatibleDecisionSource, OpenAiCompatibleProvider,
    ResponsesDecisionSource, ResponsesProvider,
};
use std::io::Write;
use std::net::SocketAddr;

/// Looks up the user-level model defaults and announces any that apply.
///
/// The notice goes to standard error so a configured default can never start
/// model requests without the person seeing which model and file selected it.
pub(super) fn configured_model_selection(
    error: &mut dyn Write,
) -> Result<Option<(String, Option<ModelProvider>)>, String> {
    let Some(path) = config_path(
        std::env::var_os("KRYPPER_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )?
    else {
        return Ok(None);
    };
    let Some(defaults) = load_model_defaults(&path)? else {
        return Ok(None);
    };
    let provider = match defaults.provider.as_deref() {
        Some(name) => Some(ModelProvider::parse(name).ok_or_else(|| {
            format!(
                "invalid config file '{}': unsupported provider '{name}'",
                defaults.path.display()
            )
        })?),
        None => None,
    };
    let _ = writeln!(
        error,
        "krypper: using model '{}' (provider: {}) from {}; pass --no-config to ignore it",
        defaults.model,
        defaults.provider.as_deref().unwrap_or("ollama"),
        defaults.path.display()
    );
    Ok(Some((defaults.model, provider)))
}

pub(super) fn prepare_model_source(
    model: Option<String>,
    provider: Option<ModelProvider>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
) -> Result<Option<ModelDecisionSource>, &'static str> {
    let Some(model) = model else {
        return if provider.is_some() {
            Err("--model is required with --provider")
        } else {
            Ok(None)
        };
    };
    let provider = provider.unwrap_or(ModelProvider::Ollama);
    validate_model_name_for_provider(&model, provider)?;
    let source = match provider {
        ModelProvider::Ollama => ModelDecisionSource::Ollama(OllamaDecisionSource::with_endpoint(
            model,
            action_budget,
            ollama_endpoint,
        )),
        ModelProvider::Gemini => ModelDecisionSource::Gemini(GeminiDecisionSource::new(
            model,
            hosted_api_key(
                "GEMINI_API_KEY",
                "GEMINI_API_KEY must be set and nonempty for --provider gemini",
            )?,
            action_budget,
        )?),
        ModelProvider::OpenAi => ModelDecisionSource::Responses(ResponsesDecisionSource::new(
            model,
            ResponsesProvider::OpenAi,
            hosted_api_key(
                "OPENAI_API_KEY",
                "OPENAI_API_KEY must be set and nonempty for --provider openai",
            )?,
            action_budget,
        )),
        ModelProvider::Xai => ModelDecisionSource::Responses(ResponsesDecisionSource::new(
            model,
            ResponsesProvider::Xai,
            hosted_api_key(
                "XAI_API_KEY",
                "XAI_API_KEY must be set and nonempty for --provider xai",
            )?,
            action_budget,
        )),
        ModelProvider::Anthropic => ModelDecisionSource::Anthropic(AnthropicDecisionSource::new(
            model,
            hosted_api_key(
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_API_KEY must be set and nonempty for --provider anthropic",
            )?,
            action_budget,
        )),
        ModelProvider::OpenAiCompatible(provider) => {
            let credentials = match provider {
                OpenAiCompatibleProvider::Cloudflare => {
                    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID").map_err(|_| {
                        "CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare"
                    })?;
                    let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                    let api_token = hosted_api_key(
                        "CLOUDFLARE_API_TOKEN",
                        "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare",
                    )?;
                    OpenAiCompatibleCredentials::Cloudflare {
                        account_id,
                        api_token,
                    }
                }
                OpenAiCompatibleProvider::Groq => OpenAiCompatibleCredentials::Groq {
                    api_key: hosted_api_key(
                        "GROQ_API_KEY",
                        "GROQ_API_KEY must be set and nonempty for --provider groq",
                    )?,
                },
                OpenAiCompatibleProvider::OpenRouter => OpenAiCompatibleCredentials::OpenRouter {
                    api_key: hosted_api_key(
                        "OPENROUTER_API_KEY",
                        "OPENROUTER_API_KEY must be set and nonempty for --provider openrouter",
                    )?,
                },
                OpenAiCompatibleProvider::DeepSeek => OpenAiCompatibleCredentials::DeepSeek {
                    api_key: hosted_api_key(
                        "DEEPSEEK_API_KEY",
                        "DEEPSEEK_API_KEY must be set and nonempty for --provider deepseek",
                    )?,
                },
                OpenAiCompatibleProvider::Zai => OpenAiCompatibleCredentials::Zai {
                    api_key: hosted_api_key(
                        "ZAI_API_KEY",
                        "ZAI_API_KEY must be set and nonempty for --provider zai",
                    )?,
                },
                OpenAiCompatibleProvider::Mistral => OpenAiCompatibleCredentials::Mistral {
                    api_key: hosted_api_key(
                        "MISTRAL_API_KEY",
                        "MISTRAL_API_KEY must be set and nonempty for --provider mistral",
                    )?,
                },
            };
            ModelDecisionSource::OpenAiCompatible(OpenAiCompatibleDecisionSource::new(
                model,
                credentials,
                action_budget,
            )?)
        }
    };
    Ok(Some(source))
}

fn hosted_api_key(
    name: &'static str,
    missing_message: &'static str,
) -> Result<String, &'static str> {
    let key = std::env::var(name).map_err(|_| missing_message)?;
    if key.trim().is_empty() || key.trim() != key || key.chars().any(char::is_control) {
        return Err(missing_message);
    }
    Ok(key)
}
