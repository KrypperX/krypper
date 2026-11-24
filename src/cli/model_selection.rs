//! Model selection: the user configuration defaults and the provider decision source.

use super::config::{config_path, load_model_defaults, ModelDefaults};
use super::provider_profiles::{is_profile_id_candidate, load_provider_profiles, selected_profile};
use super::settings::{load_credentials, user_settings_paths, CredentialStore};
use crate::brain::{
    provider_descriptor, validate_model_name_for_provider, AdapterId, AuthFieldRole,
    CloudflareAccountId, CloudflareGatewayId, ModelDecisionSource, ModelProvider,
    OpenAiCompatibleCredentials, OpenAiCompatibleProvider, ProviderDecisionSourceRegistry,
    ProviderSourceConfig, ProviderSourceCredentials,
};
use std::io::Write;
use std::net::SocketAddr;

/// Looks up the user-level model defaults and announces any that apply.
///
/// The notice goes to standard error so a configured default can never start
/// model requests without the person seeing which model and file selected it.
#[cfg(test)]
pub(super) fn configured_model_selection(
    error: &mut dyn Write,
) -> Result<Option<(String, Option<String>)>, String> {
    configured_model_defaults(error)
        .map(|defaults| defaults.map(|defaults| (defaults.model, defaults.provider)))
}

pub(super) fn configured_model_defaults(
    error: &mut dyn Write,
) -> Result<Option<ModelDefaults>, String> {
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
    if defaults
        .provider
        .as_deref()
        .is_some_and(is_profile_id_candidate)
    {
        let user_model_path = user_settings_paths()
            .map_err(|_| "cannot locate per-user settings for provider profiles".to_owned())?
            .map(|paths| paths.model);
        if user_model_path.as_deref() != Some(defaults.path.as_path()) {
            return Err(
                "custom provider profiles can be selected from the per-user model config or with explicit --provider, not from KRYPPER_CONFIG"
                    .to_owned(),
            );
        }
    }
    let _ = writeln!(
        error,
        "krypper: using model '{}' (provider: {}) from {}; pass --no-config to ignore it",
        defaults.model,
        defaults.provider.as_deref().unwrap_or("ollama"),
        defaults.path.display()
    );
    Ok(Some(defaults))
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

#[cfg(test)]
pub(super) fn prepare_model_source_with_credentials(
    model: Option<String>,
    provider: Option<ModelProvider>,
    action_budget: usize,
    ollama_endpoint: SocketAddr,
    use_saved_credentials: bool,
) -> Result<Option<ModelDecisionSource>, &'static str> {
    prepare_model_source_for_selection(
        model,
        provider.map(ProviderSelection::BuiltIn),
        action_budget,
        ollama_endpoint,
        use_saved_credentials,
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProviderSelection {
    BuiltIn(ModelProvider),
    Profile(String),
}

pub(super) fn parse_provider_selection(value: &str) -> Option<ProviderSelection> {
    ModelProvider::parse(value)
        .map(ProviderSelection::BuiltIn)
        .or_else(|| {
            is_profile_id_candidate(value).then(|| ProviderSelection::Profile(value.to_owned()))
        })
}

pub(super) fn prepare_model_source_for_selection(
    model: Option<String>,
    provider: Option<ProviderSelection>,
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
    let provider = provider.unwrap_or(ProviderSelection::BuiltIn(ModelProvider::Ollama));
    let mut credential_resolver = CredentialResolver::new(use_saved_credentials);
    let source = match provider {
        ProviderSelection::Profile(profile_id) => {
            let paths = user_settings_paths()
                .map_err(|_| "cannot locate per-user provider profiles")?
                .ok_or("per-user provider profiles are unavailable")?;
            let profiles = load_provider_profiles(&paths.provider_profiles)
                .map_err(|_| "cannot load per-user provider profiles; inspect per-user settings")?;
            let (protocol, endpoint, credential_name) =
                selected_profile(&profiles, &profile_id, &model)?;
            let credentials = match credential_name {
                Some(name) => ProviderSourceCredentials::HostedApiKey(hosted_api_key(
                    &name,
                    "provider profile API key must be set and nonempty",
                    &mut credential_resolver,
                )?),
                None => ProviderSourceCredentials::None,
            };
            let registry = ProviderDecisionSourceRegistry::with_builtins();
            registry
                .create(ProviderSourceConfig::for_profile(
                    protocol,
                    endpoint,
                    model,
                    action_budget,
                    ollama_endpoint,
                    credentials,
                ))
                .map_err(|error| error.message())?
        }
        ProviderSelection::BuiltIn(provider) => {
            validate_model_name_for_provider(&model, provider)?;
            let source_credentials = match provider {
                ModelProvider::Ollama => ProviderSourceCredentials::None,
                ModelProvider::Gemini
                | ModelProvider::OpenAi
                | ModelProvider::Xai
                | ModelProvider::Anthropic => ProviderSourceCredentials::HostedApiKey(
                    hosted_api_key_for(provider, &mut credential_resolver)?,
                ),
                ModelProvider::OpenAiCompatible(compatible_provider) => {
                    let credentials = match compatible_provider {
                        OpenAiCompatibleProvider::Cloudflare => {
                            let account_id = required_auth_field(
                                provider,
                                AuthFieldRole::CloudflareAccountId,
                                &mut credential_resolver,
                            )?;
                            let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                            let api_token = hosted_api_key_for(provider, &mut credential_resolver)?;
                            OpenAiCompatibleCredentials::Cloudflare {
                                account_id,
                                api_token,
                            }
                        }
                        OpenAiCompatibleProvider::CloudflareAiGateway => {
                            let account_id = required_auth_field(
                                provider,
                                AuthFieldRole::CloudflareAccountId,
                                &mut credential_resolver,
                            )?;
                            let account_id = CloudflareAccountId::try_from(account_id.as_str())?;
                            let api_token = hosted_api_key_for(provider, &mut credential_resolver)?;
                            let gateway_id =
                                cloudflare_gateway_id_from_env(&mut credential_resolver, provider)?;
                            OpenAiCompatibleCredentials::CloudflareAiGateway {
                                account_id,
                                api_token,
                                gateway_id,
                            }
                        }
                        OpenAiCompatibleProvider::Groq => OpenAiCompatibleCredentials::Groq {
                            api_key: hosted_api_key_for(provider, &mut credential_resolver)?,
                        },
                        OpenAiCompatibleProvider::OpenRouter => {
                            OpenAiCompatibleCredentials::OpenRouter {
                                api_key: hosted_api_key_for(provider, &mut credential_resolver)?,
                            }
                        }
                        OpenAiCompatibleProvider::DeepSeek => {
                            OpenAiCompatibleCredentials::DeepSeek {
                                api_key: hosted_api_key_for(provider, &mut credential_resolver)?,
                            }
                        }
                        OpenAiCompatibleProvider::Zai => OpenAiCompatibleCredentials::Zai {
                            api_key: hosted_api_key_for(provider, &mut credential_resolver)?,
                        },
                        OpenAiCompatibleProvider::Mistral => OpenAiCompatibleCredentials::Mistral {
                            api_key: hosted_api_key_for(provider, &mut credential_resolver)?,
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
            apply_anthropic_workspace_id(source, &mut credential_resolver)?
        }
    };
    Ok(Some(source))
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
    let workspace_field = auth_field(
        ModelProvider::Anthropic,
        AuthFieldRole::AnthropicWorkspaceId,
    );
    let workspace_id = credentials.optional(
        workspace_field.name,
        workspace_field
            .invalid_utf8_message
            .expect("optional workspace IDs have a UTF-8 validation message"),
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
        Err(std::env::VarError::NotUnicode(_)) => Err(auth_field(
            ModelProvider::Anthropic,
            AuthFieldRole::AnthropicWorkspaceId,
        )
        .invalid_utf8_message
        .expect("optional workspace IDs have a UTF-8 validation message")),
    }
}

fn configured_value_from_sources(
    environment_value: Result<String, std::env::VarError>,
    saved_value: Option<&str>,
    missing_message: &'static str,
) -> Result<String, &'static str> {
    match super::settings::effective_credential_value(environment_value, saved_value) {
        super::settings::EffectiveCredentialValue::Configured(value) => Ok(value),
        super::settings::EffectiveCredentialValue::Missing
        | super::settings::EffectiveCredentialValue::InvalidEnvironment => Err(missing_message),
    }
}

fn hosted_api_key(
    name: &str,
    missing_message: &'static str,
    credentials: &mut CredentialResolver,
) -> Result<String, &'static str> {
    let key = credentials.required(name, missing_message)?;
    if key.trim().is_empty() || key.trim() != key || key.chars().any(char::is_control) {
        return Err(missing_message);
    }
    Ok(key)
}

fn auth_field(
    provider: ModelProvider,
    role: AuthFieldRole,
) -> &'static crate::brain::ProviderAuthField {
    provider_descriptor(provider)
        .auth_field(role)
        .expect("provider descriptor includes the requested auth field")
}

fn required_auth_field(
    provider: ModelProvider,
    role: AuthFieldRole,
    credentials: &mut CredentialResolver,
) -> Result<String, &'static str> {
    let field = auth_field(provider, role);
    credentials.required(
        field.name,
        field
            .missing_message
            .expect("required auth fields have a missing-value message"),
    )
}

fn hosted_api_key_for(
    provider: ModelProvider,
    credentials: &mut CredentialResolver,
) -> Result<String, &'static str> {
    let field = auth_field(provider, AuthFieldRole::ApiKey);
    hosted_api_key(
        field.name,
        field
            .missing_message
            .expect("required API keys have a missing-value message"),
        credentials,
    )
}

fn cloudflare_gateway_id_from_env(
    credentials: &mut CredentialResolver,
    provider: ModelProvider,
) -> Result<CloudflareGatewayId, &'static str> {
    let field = auth_field(provider, AuthFieldRole::CloudflareGatewayId);
    let value = credentials
        .optional(
            field.name,
            field
                .invalid_utf8_message
                .expect("optional gateway IDs have a UTF-8 validation message"),
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
        name: &str,
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
