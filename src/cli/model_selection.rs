//! Model selection: the user configuration defaults and the provider decision source.

use super::config::{config_path, load_model_defaults};
use super::settings::{load_credentials, user_settings_paths, CredentialStore};
use crate::brain::{
    validate_model_name_for_provider, AdapterId, CloudflareAccountId, CloudflareGatewayId,
    ModelDecisionSource, ModelProvider, OpenAiCompatibleCredentials, OpenAiCompatibleProvider,
    ProviderDecisionSourceRegistry, ProviderSourceConfig, ProviderSourceCredentials,
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
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
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

#[cfg(test)]
pub(super) fn prepare_model_source(
    model: Option<String>,
    provider: Option<ModelProvider>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
) -> Result<Option<ModelDecisionSource>, &'static str> {
    prepare_model_source_with_credentials(model, provider, action_budget, ollama_endpoint, false)
}

pub(super) fn prepare_model_source_with_credentials(
    model: Option<String>,
    provider: Option<ModelProvider>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
    use_saved_credentials: bool,
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
    let mut credential_resolver = CredentialResolver::new(use_saved_credentials);
    let source_credentials = match provider {
        ModelProvider::Ollama => ProviderSourceCredentials::None,
        ModelProvider::Gemini => ProviderSourceCredentials::HostedApiKey(hosted_api_key(
            "GEMINI_API_KEY",
            "GEMINI_API_KEY must be set and nonempty for --provider gemini",
            &mut credential_resolver,
        )?),
        ModelProvider::OpenAi => ProviderSourceCredentials::HostedApiKey(hosted_api_key(
            "OPENAI_API_KEY",
            "OPENAI_API_KEY must be set and nonempty for --provider openai",
            &mut credential_resolver,
        )?),
        ModelProvider::Xai => ProviderSourceCredentials::HostedApiKey(hosted_api_key(
            "XAI_API_KEY",
            "XAI_API_KEY must be set and nonempty for --provider xai",
            &mut credential_resolver,
        )?),
        ModelProvider::Anthropic => ProviderSourceCredentials::HostedApiKey(hosted_api_key(
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_API_KEY must be set and nonempty for --provider anthropic",
            &mut credential_resolver,
        )?),
        ModelProvider::OpenAiCompatible(compatible_provider) => {
            let credentials = match compatible_provider {
                OpenAiCompatibleProvider::Cloudflare => {
                    let account_id = credential_resolver.required(
                        "CLOUDFLARE_ACCOUNT_ID",
                        "CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare",
                    )?;
                    let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                    let api_token = hosted_api_key(
                        "CLOUDFLARE_API_TOKEN",
                        "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare",
                        &mut credential_resolver,
                    )?;
                    OpenAiCompatibleCredentials::Cloudflare {
                        account_id,
                        api_token,
                    }
                }
                OpenAiCompatibleProvider::CloudflareAiGateway => {
                    let account_id = credential_resolver.required(
                        "CLOUDFLARE_ACCOUNT_ID",
                        "CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare-ai-gateway",
                    )?;
                    let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                    let api_token = hosted_api_key(
                        "CLOUDFLARE_API_TOKEN",
                        "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare-ai-gateway",
                        &mut credential_resolver,
                    )?;
                    let gateway_id = cloudflare_gateway_id_from_env(&mut credential_resolver)?;
                    OpenAiCompatibleCredentials::CloudflareAiGateway {
                        account_id,
                        api_token,
                        gateway_id,
                    }
                }
                OpenAiCompatibleProvider::Groq => OpenAiCompatibleCredentials::Groq {
                    api_key: hosted_api_key(
                        "GROQ_API_KEY",
                        "GROQ_API_KEY must be set and nonempty for --provider groq",
                        &mut credential_resolver,
                    )?,
                },
                OpenAiCompatibleProvider::OpenRouter => OpenAiCompatibleCredentials::OpenRouter {
                    api_key: hosted_api_key(
                        "OPENROUTER_API_KEY",
                        "OPENROUTER_API_KEY must be set and nonempty for --provider openrouter",
                        &mut credential_resolver,
                    )?,
                },
                OpenAiCompatibleProvider::DeepSeek => OpenAiCompatibleCredentials::DeepSeek {
                    api_key: hosted_api_key(
                        "DEEPSEEK_API_KEY",
                        "DEEPSEEK_API_KEY must be set and nonempty for --provider deepseek",
                        &mut credential_resolver,
                    )?,
                },
                OpenAiCompatibleProvider::Zai => OpenAiCompatibleCredentials::Zai {
                    api_key: hosted_api_key(
                        "ZAI_API_KEY",
                        "ZAI_API_KEY must be set and nonempty for --provider zai",
                        &mut credential_resolver,
                    )?,
                },
                OpenAiCompatibleProvider::Mistral => OpenAiCompatibleCredentials::Mistral {
                    api_key: hosted_api_key(
                        "MISTRAL_API_KEY",
                        "MISTRAL_API_KEY must be set and nonempty for --provider mistral",
                        &mut credential_resolver,
                    )?,
                },
            };
            ProviderSourceCredentials::OpenAiCompatible(credentials)
        }
    };
    let registry = ProviderDecisionSourceRegistry::with_builtins();
    let source = registry
        .create(ProviderSourceConfig::new(
            provider,
            model,
            action_budget,
            ollama_endpoint,
            source_credentials,
        ))
        .map_err(|error| error.message())?;
    Ok(Some(apply_anthropic_workspace_id(
        source,
        &mut credential_resolver,
    )?))
}

/// Applies the optional `ANTHROPIC_WORKSPACE_ID` to an Anthropic source.
///
/// Anthropic requires the workspace header only for API keys that are not
/// scoped to a workspace, so the variable is optional. Other providers pass
/// through untouched.
fn apply_anthropic_workspace_id(
    mut source: ModelDecisionSource,
    credentials: &mut CredentialResolver,
) -> Result<ModelDecisionSource, &'static str> {
    if source.adapter_id() != AdapterId::ANTHROPIC {
        return Ok(source);
    }
    let workspace_id = credentials.optional(
        "ANTHROPIC_WORKSPACE_ID",
        "ANTHROPIC_WORKSPACE_ID must be valid UTF-8",
    )?;
    if let Some(workspace_id) = workspace_id {
        source.set_anthropic_workspace_id(&workspace_id)?;
    }
    Ok(source)
}

/// An unset variable means "not configured"; a set value (even an empty one)
/// is passed on for validation so a typo fails fast instead of being ignored.
#[cfg(test)]
fn optional_workspace_id(
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, &'static str> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err("ANTHROPIC_WORKSPACE_ID must be valid UTF-8"),
    }
}

fn configured_value_from_sources(
    environment_value: Result<String, std::env::VarError>,
    saved_value: Option<&str>,
    missing_message: &'static str,
) -> Result<String, &'static str> {
    match environment_value {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => {
            saved_value.map(str::to_owned).ok_or(missing_message)
        }
        Err(std::env::VarError::NotUnicode(_)) => Err(missing_message),
    }
}

fn hosted_api_key(
    name: &'static str,
    missing_message: &'static str,
    credentials: &mut CredentialResolver,
) -> Result<String, &'static str> {
    let key = credentials.required(name, missing_message)?;
    if key.trim().is_empty() || key.trim() != key || key.chars().any(char::is_control) {
        return Err(missing_message);
    }
    Ok(key)
}

fn cloudflare_gateway_id_from_env(
    credentials: &mut CredentialResolver,
) -> Result<CloudflareGatewayId, &'static str> {
    let value = credentials
        .optional(
            "CLOUDFLARE_AI_GATEWAY_ID",
            "CLOUDFLARE_AI_GATEWAY_ID must be valid UTF-8",
        )?
        .unwrap_or_else(|| "default".to_owned());
    CloudflareGatewayId::try_from(value.as_str())
}

struct CredentialResolver {
    use_saved: bool,
    loaded: bool,
    load_failed: bool,
    store: Option<CredentialStore>,
}

impl CredentialResolver {
    fn new(use_saved: bool) -> Self {
        Self {
            use_saved,
            loaded: false,
            load_failed: false,
            store: None,
        }
    }

    fn required(
        &mut self,
        name: &'static str,
        missing_message: &'static str,
    ) -> Result<String, &'static str> {
        match std::env::var(name) {
            Ok(value) => Ok(value),
            Err(std::env::VarError::NotPresent) if self.use_saved => {
                self.load()?;
                configured_value_from_sources(
                    Err(std::env::VarError::NotPresent),
                    self.store.as_ref().and_then(|store| store.get(name)),
                    missing_message,
                )
            }
            Err(_) => Err(missing_message),
        }
    }

    fn optional(
        &mut self,
        name: &'static str,
        invalid_environment_message: &'static str,
    ) -> Result<Option<String>, &'static str> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotUnicode(_)) => Err(invalid_environment_message),
            Err(std::env::VarError::NotPresent) if self.use_saved => {
                if self.load().is_err() {
                    return Ok(None);
                }
                Ok(self
                    .store
                    .as_ref()
                    .and_then(|store| store.get(name))
                    .map(str::to_owned))
            }
            Err(std::env::VarError::NotPresent) => Ok(None),
        }
    }

    fn load(&mut self) -> Result<(), &'static str> {
        if self.loaded {
            return if self.load_failed {
                Err("cannot read saved credentials; run `krypper setup --status` to inspect")
            } else {
                Ok(())
            };
        }
        self.loaded = true;
        let loaded = match user_settings_paths() {
            Ok(Some(paths)) => load_credentials(&paths.credentials),
            Ok(None) => return Ok(()),
            Err(_) => {
                self.load_failed = true;
                return Err(
                    "cannot read saved credentials; run `krypper setup --status` to inspect",
                );
            }
        };
        match loaded {
            Ok(store) => {
                self.store = store;
                Ok(())
            }
            Err(_) => {
                self.load_failed = true;
                Err("cannot read saved credentials; run `krypper setup --status` to inspect")
            }
        }
    }
}

#[cfg(test)]
mod credential_precedence_tests {
    use super::configured_value_from_sources;
    use std::env::VarError;
    use std::ffi::OsString;

    #[test]
    fn environment_value_overrides_saved_value() {
        assert_eq!(
            configured_value_from_sources(
                Ok("environment-token".to_owned()),
                Some("saved-token"),
                "missing key"
            ),
            Ok("environment-token".to_owned())
        );
    }

    #[test]
    fn saved_value_is_used_only_when_environment_is_unset() {
        assert_eq!(
            configured_value_from_sources(
                Err(VarError::NotPresent),
                Some("saved-token"),
                "missing key"
            ),
            Ok("saved-token".to_owned())
        );
        assert_eq!(
            configured_value_from_sources(Err(VarError::NotPresent), None, "missing key"),
            Err("missing key")
        );
    }

    #[test]
    fn invalid_environment_values_do_not_fall_back_to_saved_values() {
        assert_eq!(
            configured_value_from_sources(
                Err(VarError::NotUnicode(OsString::from("invalid"))),
                Some("saved-token"),
                "missing key"
            ),
            Err("missing key")
        );
        assert_eq!(
            configured_value_from_sources(Ok(String::new()), Some("saved-token"), "missing key"),
            Ok(String::new())
        );
    }
}

#[cfg(test)]
mod workspace_id_tests {
    use super::optional_workspace_id;
    use std::env::VarError;
    use std::ffi::OsString;

    #[test]
    fn unset_is_not_configured_and_set_values_are_passed_on() {
        assert_eq!(optional_workspace_id(Err(VarError::NotPresent)), Ok(None));
        assert_eq!(
            optional_workspace_id(Ok("wrkspc_test".to_owned())),
            Ok(Some("wrkspc_test".to_owned()))
        );
        // An empty value is configured-but-invalid, so it reaches validation.
        assert_eq!(
            optional_workspace_id(Ok(String::new())),
            Ok(Some(String::new()))
        );
    }

    #[test]
    fn non_unicode_value_is_rejected() {
        assert_eq!(
            optional_workspace_id(Err(VarError::NotUnicode(OsString::from("x")))),
            Err("ANTHROPIC_WORKSPACE_ID must be valid UTF-8")
        );
    }
}
