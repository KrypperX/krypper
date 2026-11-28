//! Interactive, per-user model and provider configuration.

use super::config::{
    load_model_defaults, validate_route_settings, ModelDefaults, RoutingMode,
    MAX_FALLBACK_BUDGET_MICRO_USD,
};
use super::help::{usage_error, write_setup_help};
use super::model_catalog::{cache_path_for_user_settings, load_catalog, CatalogOrigin};
use super::model_routing::ModelRoutingService;
use super::provider_profiles::load_provider_profiles;
use super::settings::{
    load_credentials, load_legacy_credentials, load_offline_process_configuration,
    load_permission_mode, remove_settings, save_credentials,
    save_credentials_after_approved_migration, save_model_configuration,
    save_offline_process_configuration, save_permission_mode, user_settings_paths,
    valid_credential, CredentialStore, CredentialStoreError, UserSettingsPaths,
};
use super::setup_probe::test_connection;
use crate::brain::{
    provider_descriptor_by_selector, provider_descriptors, validate_model_name_for_provider,
    AnthropicWorkspaceId, AuthFieldRole, CloudflareAccountId, CloudflareGatewayId,
    ProviderDescriptor,
};
use crate::tools::permission_policy::PermissionMode;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
#[cfg(test)]
use std::path::PathBuf;

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

    if arguments
        .iter()
        .any(|argument| argument == "--routing-mode")
    {
        return set_routing_mode(arguments, output, error);
    }

    if arguments
        .iter()
        .any(|argument| argument == "--permission-mode")
    {
        return set_permission_mode(arguments, output, error);
    }

    if arguments
        .iter()
        .any(|argument| argument == "--offline-process")
    {
        return set_offline_process_configuration(arguments, output, error);
    }

    let (mode, status_json) = match arguments {
        [] => (SetupMode::Configure, false),
        [argument] if argument == "--status" => (SetupMode::Status, false),
        [status, json] if status == "--status" && json == "--json" => (SetupMode::Status, true),
        [argument] if argument == "--reset" => (SetupMode::Reset, false),
        _ => return usage_error(error, Some("setup"), "unrecognized or duplicate option"),
    };

    if mode == SetupMode::Status {
        return show_status(output, status_json);
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

fn set_offline_process_configuration(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if arguments
        .first()
        .is_none_or(|argument| argument != "--offline-process")
    {
        return usage_error(
            error,
            Some("setup"),
            "use --offline-process --toolchain-root PATH --registry-root PATH",
        );
    }
    if !cfg!(target_os = "linux") {
        return usage_error(
            error,
            Some("setup"),
            "the offline process profile is currently available only on Linux",
        );
    }
    let mut toolchain_root = None;
    let mut registry_root = None;
    let mut index = 1;
    while index < arguments.len() {
        let Some(option) = arguments[index].to_str() else {
            return usage_error(
                error,
                Some("setup"),
                "offline process options must be valid UTF-8",
            );
        };
        let slot = match option {
            "--toolchain-root" if toolchain_root.is_none() => &mut toolchain_root,
            "--registry-root" if registry_root.is_none() => &mut registry_root,
            _ => {
                return usage_error(
                    error,
                    Some("setup"),
                    "use --offline-process --toolchain-root PATH --registry-root PATH without duplicate options",
                )
            }
        };
        index += 1;
        let Some(value) = arguments.get(index) else {
            return usage_error(
                error,
                Some("setup"),
                "offline process root path is required",
            );
        };
        if value.to_str().is_none() {
            return usage_error(
                error,
                Some("setup"),
                "offline process root paths must be valid UTF-8",
            );
        }
        *slot = Some(std::path::PathBuf::from(value));
        index += 1;
    }
    let (Some(toolchain_root), Some(registry_root)) = (toolchain_root, registry_root) else {
        return usage_error(
            error,
            Some("setup"),
            "--offline-process needs both --toolchain-root and --registry-root",
        );
    };
    let Some(paths) = user_settings_paths().map_err(io::Error::other)? else {
        return usage_error(
            error,
            Some("setup"),
            "cannot find a user configuration directory; set XDG_CONFIG_HOME or HOME",
        );
    };
    if let Err(message) =
        save_offline_process_configuration(&paths.offline_process, &toolchain_root, &registry_root)
    {
        return usage_error(error, Some("setup"), &message);
    }
    writeln!(
        output,
        "Saved the preprovisioned offline process profile for this user."
    )?;
    writeln!(
        output,
        "No toolchain or registry files were installed or downloaded."
    )?;
    Ok(0)
}

fn set_permission_mode(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    if arguments.len() != 2 || arguments[0] != "--permission-mode" {
        return usage_error(
            error,
            Some("setup"),
            "use --permission-mode ask|risk-based|full-access",
        );
    }
    let Some(value) = arguments[1].to_str() else {
        return usage_error(error, Some("setup"), "permission mode must be valid UTF-8");
    };
    let mode = match PermissionMode::parse(value) {
        Ok(mode) => mode,
        Err(message) => return usage_error(error, Some("setup"), message),
    };
    let Some(paths) = user_settings_paths().map_err(io::Error::other)? else {
        return usage_error(
            error,
            Some("setup"),
            "cannot find a user configuration directory; set XDG_CONFIG_HOME or HOME",
        );
    };
    if let Err(cause) = save_permission_mode(&paths.permission_mode, mode) {
        return usage_error(error, Some("setup"), &cause.to_string());
    }
    writeln!(
        output,
        "Saved {} permission mode for this user.",
        mode.as_str()
    )?;
    if mode == PermissionMode::FullAccess {
        writeln!(
            output,
            "This is only a per-user preference. Each Linux task still needs an explicit controlling-terminal confirmation before its provider starts."
        )?;
    }
    Ok(0)
}

fn set_routing_mode(
    arguments: &[OsString],
    output: &mut dyn Write,
    error: &mut dyn Write,
) -> io::Result<i32> {
    let mut selected_mode = None;
    let mut allow_cross_provider_data_transfer = false;
    let mut fallback_budget_micro_usd = None;
    let mut index = 0;
    while index < arguments.len() {
        let Some(option) = arguments[index].to_str() else {
            return usage_error(error, Some("setup"), "arguments must be valid UTF-8");
        };
        match option {
            "--routing-mode" if selected_mode.is_none() => {
                index += 1;
                let Some(value) = arguments.get(index).and_then(|value| value.to_str()) else {
                    return usage_error(
                        error,
                        Some("setup"),
                        "--routing-mode needs manual or automatic",
                    );
                };
                selected_mode = match value {
                    "manual" => Some(RoutingMode::Manual),
                    "automatic" | "auto" => Some(RoutingMode::Automatic),
                    _ => {
                        return usage_error(
                            error,
                            Some("setup"),
                            "routing mode must be manual or automatic",
                        )
                    }
                };
            }
            "--allow-cross-provider" if !allow_cross_provider_data_transfer => {
                allow_cross_provider_data_transfer = true;
            }
            "--fallback-budget-usd" if fallback_budget_micro_usd.is_none() => {
                index += 1;
                let Some(value) = arguments.get(index).and_then(|value| value.to_str()) else {
                    return usage_error(
                        error,
                        Some("setup"),
                        "--fallback-budget-usd needs a positive USD amount",
                    );
                };
                fallback_budget_micro_usd = match parse_usd_micro(value) {
                    Some(value) => Some(value),
                    None => return usage_error(error, Some("setup"), "fallback budget must be a positive amount with at most six decimal places and no more than $1000"),
                };
            }
            _ => {
                return usage_error(
                    error,
                    Some("setup"),
                    "unrecognized or duplicate routing option",
                )
            }
        }
        index += 1;
    }
    let Some(routing_mode) = selected_mode else {
        return usage_error(error, Some("setup"), "--routing-mode is required");
    };
    if let Err(message) = validate_route_settings(
        routing_mode,
        allow_cross_provider_data_transfer,
        fallback_budget_micro_usd,
    ) {
        return usage_error(error, Some("setup"), message);
    }
    let Some(paths) = user_settings_paths().map_err(io::Error::other)? else {
        return usage_error(
            error,
            Some("setup"),
            "cannot find a user configuration directory; set XDG_CONFIG_HOME or HOME",
        );
    };
    let defaults = match load_model_defaults(&paths.model) {
        Ok(Some(defaults)) => defaults,
        Ok(None) => {
            return usage_error(
                error,
                Some("setup"),
                "run `krypper setup` first to save a provider and model",
            )
        }
        Err(message) => return usage_error(error, Some("setup"), &message),
    };
    let provider = defaults.provider.as_deref().unwrap_or("ollama");
    if let Err(message) = save_model_configuration(
        &paths.model,
        provider,
        &defaults.model,
        routing_mode,
        allow_cross_provider_data_transfer,
        fallback_budget_micro_usd,
    ) {
        return usage_error(error, Some("setup"), &message);
    }
    let mode_name = routing_mode.as_str();
    writeln!(output, "Saved {mode_name} routing for this user.")?;
    if routing_mode == RoutingMode::Automatic {
        writeln!(
            output,
            "Cross-provider task data transfer: {}",
            if allow_cross_provider_data_transfer {
                "allowed"
            } else {
                "blocked"
            }
        )?;
        match fallback_budget_micro_usd {
            Some(budget) => writeln!(
                output,
                "Bounded fallback: enabled with a ${:.6} aggregate estimate ceiling.",
                budget as f64 / 1_000_000.0
            )?,
            None => writeln!(
                output,
                "Bounded fallback: disabled; use --fallback-budget-usd USD to opt in."
            )?,
        }
    }
    if has_config_override() {
        writeln!(
            output,
            "KRYPPER_CONFIG is set and may override this mode for task runs."
        )?;
    }
    Ok(0)
}

fn parse_usd_micro(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()? * 10_u64.checked_pow((6 - fraction.len()) as u32)?
    };
    let total = whole.checked_mul(1_000_000)?.checked_add(fractional)?;
    (total > 0 && total <= MAX_FALLBACK_BUDGET_MICRO_USD).then_some(total)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    Configure,
    Status,
    Reset,
}

fn show_status(output: &mut dyn Write, json: bool) -> io::Result<i32> {
    let paths = match user_settings_paths() {
        Ok(Some(paths)) => paths,
        Ok(None) => {
            writeln!(output, "No per-user configuration directory is available.")?;
            writeln!(output, "Permission mode: ask (default)")?;
            writeln!(
                output,
                "Full Access: Linux only; each task requires terminal confirmation"
            )?;
            return Ok(0);
        }
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    let model_routing = ModelRoutingService::for_user_settings(&paths);
    let defaults = match model_routing.load_user_defaults() {
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
    let permission_mode = match load_permission_mode(&paths.permission_mode) {
        Ok(mode) => mode,
        Err(cause) => {
            writeln!(output, "krypper setup: {cause}")?;
            return Ok(2);
        }
    };
    let profile_count = match load_provider_profiles(&paths.provider_profiles) {
        Ok(profiles) => profiles.len(),
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    #[cfg(target_os = "linux")]
    let offline_process_configured =
        match load_offline_process_configuration(&paths.offline_process) {
            Ok(configuration) => configuration.is_some(),
            Err(message) => {
                writeln!(output, "krypper setup: {message}")?;
                return Ok(2);
            }
        };
    #[cfg(not(target_os = "linux"))]
    let offline_process_configured = false;

    let readiness = match defaults.as_ref() {
        Some(defaults) => match model_routing.provider_readiness_for_default(defaults) {
            Ok(readiness) => readiness,
            Err(message) => {
                writeln!(output, "krypper setup: {message}")?;
                return Ok(2);
            }
        },
        None => None,
    };
    let catalog = match load_catalog(Some(&cache_path_for_user_settings(&paths.model))) {
        Ok(catalog) => catalog,
        Err(message) => {
            writeln!(output, "krypper setup: {message}")?;
            return Ok(2);
        }
    };
    let route: Option<Result<crate::routing::RoutingSelection, String>> = defaults
        .as_ref()
        .map(|defaults| model_routing.configured_route(defaults));

    if json {
        let route_value = route.as_ref().map(|route| match route {
            Ok(route) => serde_json::json!({
                "provider": route.provider,
                "model": route.model,
                "reason": route.reason.as_str(),
            }),
            Err(message) => serde_json::json!({ "error": message }),
        });
        let value = serde_json::json!({
            "provider": defaults.as_ref().and_then(|value| value.provider.as_deref()).unwrap_or("ollama"),
            "model": defaults.as_ref().map(|value| value.model.as_str()),
            "routing_mode": defaults.as_ref().map(|value| value.routing_mode.as_str()),
            "allow_cross_provider_data_transfer": defaults.as_ref().is_some_and(|value| value.allow_cross_provider_data_transfer),
            "fallback_budget_micro_usd": defaults.as_ref().and_then(|value| value.fallback_budget_micro_usd),
            "provider_readiness": readiness,
            "route": route_value,
            "catalog": {
                "origin": match catalog.origin { CatalogOrigin::Embedded => "embedded", CatalogOrigin::RefreshedCache => "refreshed_cache" },
                "stale": catalog.stale,
                "provenance": catalog.view.provenance,
            },
            "credential_count": credential_count,
            "provider_profile_count": profile_count,
            "permission_mode": permission_mode.as_str(),
            "offline_process_profile": {
                "configured": offline_process_configured,
                "available": cfg!(target_os = "linux"),
                "settings_file": paths.offline_process,
                "roots_must_be_disjoint_from_private_settings_directory": true,
                "settings_file_must_be_outside_task_workspace": true,
            },
            "full_access_available": cfg!(target_os = "linux"),
            "full_access_task_confirmation_required": true,
            "config_override": has_config_override(),
        });
        serde_json::to_writer_pretty(&mut *output, &value)?;
        writeln!(output)?;
        return Ok(0);
    }

    write_status(
        output,
        defaults.as_ref(),
        credential_count,
        profile_count,
        &paths,
        has_config_override(),
    )?;
    writeln!(output, "Permission mode: {}", permission_mode.as_str())?;
    writeln!(
        output,
        "Offline process profile: {}",
        if cfg!(target_os = "linux") {
            if offline_process_configured {
                "configured"
            } else {
                "not configured"
            }
        } else {
            "unavailable on this platform"
        }
    )?;
    writeln!(
        output,
        "Offline profile roots must be disjoint from each other and the private settings directory; each task also requires disjoint roots and the settings file outside its workspace."
    )?;
    writeln!(
        output,
        "Full Access: Linux direct-argv host process; per-task terminal confirmation required"
    )?;
    match readiness {
        Some(readiness) => writeln!(
            output,
            "Provider readiness: discoverable={} executable={} configured={} verification={:?}",
            readiness.discoverable,
            readiness.executable,
            readiness.configured,
            readiness.verification
        )?,
        None => writeln!(
            output,
            "Provider readiness: unknown (no saved built-in provider/model)"
        )?,
    }
    match route {
        Some(Ok(route)) => writeln!(
            output,
            "Selected route: {}/{} ({})",
            route.provider.as_deref().unwrap_or("unknown"),
            route.model,
            route.reason.as_str()
        )?,
        Some(Err(message)) => writeln!(output, "Selected route: unavailable ({message})")?,
        None => writeln!(output, "Selected route: not configured")?,
    }
    writeln!(
        output,
        "Catalog: {}{} ({} source(s))",
        match catalog.origin {
            CatalogOrigin::Embedded => "embedded",
            CatalogOrigin::RefreshedCache => "refreshed cache",
        },
        if catalog.stale { "; stale" } else { "" },
        catalog.view.provenance.len()
    )?;
    Ok(0)
}

fn write_status(
    output: &mut dyn Write,
    defaults: Option<&ModelDefaults>,
    credential_count: usize,
    profile_count: usize,
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
            writeln!(output, "Routing: {}", defaults.routing_mode.as_str())?;
            if defaults.routing_mode == RoutingMode::Automatic {
                writeln!(
                    output,
                    "Cross-provider task data transfer: {}",
                    if defaults.allow_cross_provider_data_transfer {
                        "allowed"
                    } else {
                        "blocked"
                    }
                )?;
                match defaults.fallback_budget_micro_usd {
                    Some(budget) => writeln!(
                        output,
                        "Bounded fallback: enabled (aggregate estimate ceiling ${:.6})",
                        budget as f64 / 1_000_000.0
                    )?,
                    None => writeln!(output, "Bounded fallback: disabled")?,
                }
            }
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
        writeln!(
            output,
            "Encrypted credential file: {}",
            paths.credentials.display()
        )?;
    }
    if config_override {
        writeln!(
            output,
            "KRYPPER_CONFIG is set and overrides these saved model defaults for task runs."
        )?;
    }
    if profile_count == 0 {
        writeln!(output, "Provider profiles: none")?;
    } else {
        writeln!(
            output,
            "Provider profiles: {profile_count} configured ({})",
            paths.provider_profiles.display()
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
        "Remove saved defaults, permission mode, provider profiles, stored credentials, and local provider evidence from this user account? [y/N] ",
        false,
    )? {
        writeln!(output, "No settings removed.")?;
        return Ok(0);
    }
    match remove_settings(paths) {
        Ok(()) => {
            writeln!(
                output,
                "Per-user provider settings and runtime evidence removed."
            )?;
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
    let mut migration_approved = false;
    let existing_defaults = match load_model_defaults(&paths.model) {
        Ok(defaults) => defaults,
        Err(message) => return usage_error(error, Some("setup"), &message),
    };
    let mut credentials = match load_credentials(&paths.credentials) {
        Ok(Some(credentials)) => credentials,
        Ok(None) => CredentialStore::empty(),
        Err(CredentialStoreError::LegacyPlaintext(_)) => {
            writeln!(
                output,
                "The existing credential file contains plaintext values. Saving setup will store them in the system credential vault and replace the active file."
            )?;
            if !prompt_yes_no(
                input,
                output,
                "Allow migration when these setup settings are saved? [y/N] ",
                false,
            )? {
                writeln!(output, "No settings changed.")?;
                return Ok(0);
            }
            migration_approved = true;
            match load_legacy_credentials(&paths.credentials) {
                Ok(credentials) => credentials,
                Err(message) => return usage_error(error, Some("setup"), &message.to_string()),
            }
        }
        Err(message) => return usage_error(error, Some("setup"), &message.to_string()),
    };
    if existing_defaults.is_some() || !credentials.is_empty() {
        writeln!(
            output,
            "Existing settings found. Continuing replaces the provider/model defaults and updates the selected provider's credentials; credentials for other providers are kept."
        )?;
        if !prompt_yes_no(input, output, "Continue? [y/N] ", false)? {
            if migration_approved {
                writeln!(
                    output,
                    "No model settings saved; the approved legacy credential file was left in place."
                )?;
            } else {
                writeln!(output, "No settings changed.")?;
            }
            return Ok(0);
        }
    }

    let Some(provider) = choose_provider(input, output, existing_defaults.as_ref())? else {
        write_cancelled_setup(output, migration_approved)?;
        return Ok(0);
    };
    let current_model = existing_defaults
        .as_ref()
        .filter(|defaults| {
            defaults
                .provider
                .as_deref()
                .unwrap_or("ollama")
                .eq_ignore_ascii_case(provider.selector)
        })
        .map(|defaults| defaults.model.as_str());
    let Some(model) = choose_model(input, output, provider, paths, current_model)? else {
        write_cancelled_setup(output, migration_approved)?;
        return Ok(0);
    };
    if let Err(message) = collect_credentials(input, output, provider, &mut credentials) {
        return usage_error(error, Some("setup"), &message);
    }

    let selected_credential_count = provider
        .auth_fields
        .iter()
        .filter(|field| credentials.get(field.name).is_some())
        .count();
    let retained_credential_count = credentials.len().saturating_sub(selected_credential_count);
    write_review(
        output,
        paths,
        provider,
        &model,
        selected_credential_count,
        retained_credential_count,
    )?;
    if !prompt_yes_no(
        input,
        output,
        "Save these settings for this user? [y/N] ",
        false,
    )? {
        write_cancelled_setup(output, migration_approved)?;
        return Ok(0);
    }

    let save_result = if migration_approved {
        save_credentials_after_approved_migration(&paths.credentials, &mut credentials)
    } else {
        save_credentials(&paths.credentials, &mut credentials)
    };
    if let Err(message) = save_result {
        return usage_error(error, Some("setup"), &message);
    }
    let routing = existing_defaults.as_ref();
    if let Err(message) = save_model_configuration(
        &paths.model,
        provider.selector,
        &model,
        routing.map_or(RoutingMode::Manual, |value| value.routing_mode),
        routing.is_some_and(|value| value.allow_cross_provider_data_transfer),
        routing.and_then(|value| value.fallback_budget_micro_usd),
    ) {
        return usage_error(error, Some("setup"), &message);
    }
    if migration_approved {
        writeln!(
            output,
            "The approved legacy credentials were migrated to encrypted system-vault storage."
        )?;
    }
    writeln!(
        output,
        "Saved {} with model `{model}` for this user.\nModel defaults: {}",
        provider.setup_label,
        paths.model.display()
    )?;
    if !provider.auth_fields.is_empty() {
        writeln!(
            output,
            "Credentials are encrypted with a key stored in the system credential vault."
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

    let test_prompt = if provider.selector == "ollama" {
        "Send one short request to the local Ollama instance to test it? [y/N] "
    } else {
        "Send one short request to test this provider? It sends a prompt to the selected service and may incur charges. [y/N] "
    };
    if prompt_yes_no(input, output, test_prompt, false)? {
        test_connection(provider, &model, paths, &credentials, output)?;
    }
    Ok(0)
}

fn write_cancelled_setup(output: &mut dyn Write, migration_approved: bool) -> io::Result<()> {
    if migration_approved {
        writeln!(
            output,
            "Setup cancelled; no settings saved and the approved legacy credential file was left in place."
        )
    } else {
        writeln!(output, "Setup cancelled; no settings changed.")
    }
}

fn write_review(
    output: &mut dyn Write,
    paths: &UserSettingsPaths,
    provider: ProviderDescriptor,
    model: &str,
    selected_credential_count: usize,
    retained_credential_count: usize,
) -> io::Result<()> {
    writeln!(output, "\nReview your setup:")?;
    writeln!(output, "  Provider: {}", provider.setup_label)?;
    writeln!(output, "  Model: {model}")?;
    if selected_credential_count == 0 {
        writeln!(output, "  Credentials: none required for this provider")?;
    } else {
        writeln!(
            output,
            "  Credentials for this provider: {selected_credential_count} field(s); values hidden"
        )?;
    }
    if retained_credential_count > 0 {
        writeln!(
            output,
            "  Other saved credentials kept: {retained_credential_count} field(s); values hidden"
        )?;
    }
    if selected_credential_count + retained_credential_count > 0 {
        writeln!(
            output,
            "  Encrypted credential file: {}",
            paths.credentials.display()
        )?;
        writeln!(
            output,
            "  A key stored in the system credential vault protects the credential values."
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
    existing_defaults: Option<&ModelDefaults>,
) -> io::Result<Option<ProviderDescriptor>> {
    let current = existing_defaults
        .and_then(|defaults| {
            provider_descriptor_by_selector(defaults.provider.as_deref().unwrap_or("ollama"))
        })
        .copied();
    let default_provider = current.or_else(|| provider_descriptors().first().copied());
    writeln!(output, "Choose a model provider:")?;
    for (index, provider) in provider_descriptors().iter().enumerate() {
        let current_marker = if current.is_some_and(|current| current.selector == provider.selector)
        {
            " (current)"
        } else {
            ""
        };
        let credential_hint = if provider.auth_fields.is_empty() {
            "no credentials needed"
        } else {
            "credentials needed"
        };
        writeln!(
            output,
            "  {}. {} — {}{}",
            index + 1,
            provider.setup_label,
            credential_hint,
            current_marker
        )?;
    }
    let default_index = default_provider
        .and_then(|provider| {
            provider_descriptors()
                .iter()
                .position(|candidate| candidate.selector == provider.selector)
        })
        .map_or(1, |index| index + 1);
    loop {
        let prompt = if current.is_some() {
            format!("Provider [{default_index}; Enter keeps current; q cancels]: ")
        } else {
            format!("Provider [{default_index}; q cancels]: ")
        };
        let Some(value) = prompt_line(input, output, &prompt)? else {
            return Ok(None);
        };
        if value.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if value.trim().is_empty() {
            return Ok(default_provider);
        }
        if let Ok(index) = value.trim().parse::<usize>() {
            if let Some(provider) = index
                .checked_sub(1)
                .and_then(|index| provider_descriptors().get(index))
            {
                return Ok(Some(*provider));
            }
        }
        if let Some(provider) = provider_descriptors().iter().find(|provider| {
            provider.selector.eq_ignore_ascii_case(value.trim())
                || provider.setup_label.eq_ignore_ascii_case(value.trim())
        }) {
            return Ok(Some(*provider));
        }
        writeln!(output, "Choose a provider number or name, or enter q.")?;
    }
}

fn choose_model(
    input: &mut dyn Read,
    output: &mut dyn Write,
    provider: ProviderDescriptor,
    paths: &UserSettingsPaths,
    current_model: Option<&str>,
) -> io::Result<Option<String>> {
    let prompt = match current_model {
        Some(model) => format!(
            "Search offline models by name or ID (Enter keeps `{}`; q cancels): ",
            escape_terminal_text(model)
        ),
        None => {
            "Search offline models by name or ID (Enter to type a model ID; q cancels): ".to_owned()
        }
    };
    let Some(query) = prompt_line(input, output, &prompt)? else {
        return Ok(None);
    };
    let query = query.trim();
    if query.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let query = query.to_owned();

    if query.is_empty() {
        if let Some(model) = current_model {
            return validate_model(provider.selector, model)
                .map(Some)
                .map_err(io::Error::other);
        }
        return prompt_model_id(input, output, provider);
    }
    let catalog = load_catalog(Some(&cache_path_for_user_settings(&paths.model)))
        .map_err(io::Error::other)?;
    if let Some(exact) = catalog.view.models.iter().find(|entry| {
        crate::catalog_data::provider_matches(provider.selector, &entry.model.provider)
            && entry.model.id.eq_ignore_ascii_case(&query)
    }) {
        return validate_model(provider.selector, &exact.model.id)
            .map(Some)
            .map_err(io::Error::other);
    }
    let query = query.to_ascii_lowercase();
    let candidates: Vec<_> = catalog
        .view
        .models
        .iter()
        .filter(|entry| {
            crate::catalog_data::provider_matches(provider.selector, &entry.model.provider)
        })
        .filter(|entry| {
            entry.model.id.to_ascii_lowercase().contains(&query)
                || entry.model.name.to_ascii_lowercase().contains(&query)
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
            "  {}. {} ({})",
            index + 1,
            escape_terminal_text(&model.model.name),
            escape_terminal_text(&model.model.id)
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
            return Ok(Some(candidates[0].model.id.clone()));
        }
        if let Ok(index) = selection.parse::<usize>() {
            if let Some(candidate) = index.checked_sub(1).and_then(|index| candidates.get(index)) {
                return Ok(Some(candidate.model.id.clone()));
            }
        }
        match validate_model(provider.selector, selection) {
            Ok(model) => return Ok(Some(model)),
            Err(message) => writeln!(output, "{message}")?,
        }
    }
}

fn prompt_model_id(
    input: &mut dyn Read,
    output: &mut dyn Write,
    provider: ProviderDescriptor,
) -> io::Result<Option<String>> {
    loop {
        let Some(model) = prompt_line(input, output, "Model ID (q cancels): ")? else {
            return Ok(None);
        };
        let model = model.trim();
        if model.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        match validate_model(provider.selector, model) {
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
    provider: ProviderDescriptor,
    credentials: &mut CredentialStore,
) -> Result<(), String> {
    for field in provider.auth_fields {
        match field.role {
            AuthFieldRole::ApiKey if field.secret => {
                let saved = credentials.get(field.name).is_some();
                if std::env::var_os(field.name).is_some() {
                    writeln!(
                        output,
                        "{} is set in the environment and overrides the saved credential while set.",
                        field.name
                    )
                    .map_err(|_| "could not display setup guidance".to_owned())?;
                }
                if saved
                    && prompt_yes_no(
                        input,
                        output,
                        &format!("Keep the saved {} credential? [Y/n] ", field.name),
                        true,
                    )
                    .map_err(|_| "could not read credential preference".to_owned())?
                {
                    continue;
                }
                credentials.insert(
                    field.name,
                    prompt_required_secret(output, field.setup_prompt)?,
                );
            }
            AuthFieldRole::CloudflareAccountId => {
                let account_id = prompt_required_line(input, output, field.setup_prompt)?;
                CloudflareAccountId::try_from(account_id.as_str())
                    .map_err(|_| "invalid Cloudflare Account ID".to_owned())?;
                credentials.insert(field.name, account_id);
            }
            AuthFieldRole::CloudflareGatewayId => {
                let gateway_id = prompt_line(input, output, field.setup_prompt)
                    .map_err(|_| "could not read interactive input".to_owned())?
                    .ok_or_else(|| "setup cancelled; no settings were changed".to_owned())?;
                if gateway_id.trim().is_empty() {
                    credentials.remove(field.name);
                } else {
                    let gateway_id = gateway_id.trim().to_owned();
                    CloudflareGatewayId::try_from(gateway_id.as_str())
                        .map_err(|_| "invalid Cloudflare AI Gateway ID".to_owned())?;
                    credentials.insert(field.name, gateway_id);
                }
            }
            AuthFieldRole::AnthropicWorkspaceId => {
                let workspace_id = prompt_line(input, output, field.setup_prompt)
                    .map_err(|_| "could not read interactive input".to_owned())?
                    .ok_or_else(|| "setup cancelled; no settings were changed".to_owned())?;
                if workspace_id.trim().is_empty() {
                    credentials.remove(field.name);
                } else {
                    let workspace_id = workspace_id.trim();
                    if AnthropicWorkspaceId::try_from(workspace_id).is_err() {
                        return Err("invalid Anthropic Workspace ID".to_owned());
                    }
                    credentials.insert(field.name, workspace_id.to_owned());
                }
            }
            AuthFieldRole::ApiKey => {
                return Err("provider API key metadata is invalid".to_owned());
            }
        }
    }
    Ok(())
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

fn provider_label(provider: &str) -> &str {
    provider_descriptor_by_selector(provider).map_or(provider, |descriptor| descriptor.setup_label)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{lock_environment, RestoreEnvironmentVariable, TempDir};
    use std::fs;

    fn settings_paths(directory: &crate::test_support::TempDir) -> UserSettingsPaths {
        let config = directory.path().join("krypper");
        UserSettingsPaths {
            model: config.join("config.toml"),
            credentials: config.join("credentials.json"),
            permission_mode: config.join("permission-mode"),
            provider_profiles: config.join("provider-profiles.json"),
            model_evidence: config.join("model-evidence.json"),
            model_evidence_key: config.join("model-evidence.key"),
            offline_process: config.join("offline-process.json"),
        }
    }

    #[cfg(target_os = "linux")]
    fn create_offline_process_roots(directory: &TempDir) -> (PathBuf, PathBuf) {
        let toolchain = directory.path().join("toolchain");
        fs::create_dir_all(toolchain.join("bin")).expect("create toolchain bin directory");
        for executable in ["cargo", "rustc"] {
            fs::write(toolchain.join("bin").join(executable), b"\x7fELF")
                .expect("write toolchain identity fixture");
        }
        let registry = directory.path().join("registry");
        fs::create_dir_all(&registry).expect("create registry root");
        (toolchain, registry)
    }

    #[cfg(target_os = "linux")]
    fn tree_snapshot(root: &std::path::Path) -> Vec<(PathBuf, bool, Vec<u8>)> {
        fn collect(
            root: &std::path::Path,
            directory: &std::path::Path,
            snapshot: &mut Vec<(PathBuf, bool, Vec<u8>)>,
        ) {
            for entry in fs::read_dir(directory).expect("read fixture tree") {
                let entry = entry.expect("read fixture entry");
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .expect("fixture path is below root")
                    .to_path_buf();
                let file_type = entry.file_type().expect("read fixture file type");
                if file_type.is_dir() {
                    snapshot.push((relative, true, Vec::new()));
                    collect(root, &path, snapshot);
                } else {
                    snapshot.push((relative, false, fs::read(path).expect("read fixture file")));
                }
            }
        }

        let mut snapshot = Vec::new();
        collect(root, root, &mut snapshot);
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn offline_process_setup_saves_private_roots_without_installing_and_status_reads_them() {
        let _environment_lock = lock_environment();
        let directory = TempDir::new();
        let _restore_config_home =
            RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", directory.path());
        let (toolchain, registry) = create_offline_process_roots(&directory);
        let toolchain_before = tree_snapshot(&toolchain);
        let registry_before = tree_snapshot(&registry);
        let paths = settings_paths(&directory);

        let arguments = vec![
            OsString::from("--offline-process"),
            OsString::from("--toolchain-root"),
            toolchain.as_os_str().to_owned(),
            OsString::from("--registry-root"),
            registry.as_os_str().to_owned(),
        ];
        let mut input = &b""[..];
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            run(&arguments, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Saved the preprovisioned offline process profile"));
        assert!(output.contains("No toolchain or registry files were installed or downloaded"));
        assert_eq!(tree_snapshot(&toolchain), toolchain_before);
        assert_eq!(tree_snapshot(&registry), registry_before);
        assert!(
            super::super::settings::load_offline_process_configuration(&paths.offline_process)
                .unwrap()
                .is_some()
        );

        let arguments = ["--status", "--json"].map(OsString::from);
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            run(&arguments, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        let status: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            status["offline_process_profile"]["configured"],
            serde_json::Value::Bool(true)
        );
        let expected_settings_path = paths.offline_process.to_string_lossy().into_owned();
        assert_eq!(
            status["offline_process_profile"]["settings_file"].as_str(),
            Some(expected_settings_path.as_str())
        );
        assert_eq!(
            status["offline_process_profile"]["settings_file_must_be_outside_task_workspace"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            status["offline_process_profile"]
                ["roots_must_be_disjoint_from_private_settings_directory"],
            serde_json::Value::Bool(true)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn offline_process_setup_rejects_missing_and_invalid_roots_without_writing_settings() {
        let _environment_lock = lock_environment();
        let directory = TempDir::new();
        let _restore_config_home =
            RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", directory.path());
        let (toolchain, _registry) = create_offline_process_roots(&directory);
        let settings_path = settings_paths(&directory).offline_process;
        let mut input = &b""[..];

        let cases = [
            (
                vec![
                    OsString::from("--offline-process"),
                    OsString::from("--toolchain-root"),
                    toolchain.as_os_str().to_owned(),
                ],
                "needs both --toolchain-root and --registry-root",
            ),
            (
                vec![
                    OsString::from("--offline-process"),
                    OsString::from("--toolchain-root"),
                    toolchain.as_os_str().to_owned(),
                    OsString::from("--registry-root"),
                    OsString::from("relative/registry"),
                ],
                "toolchain and offline registry roots must be absolute paths",
            ),
            (
                vec![
                    OsString::from("--offline-process"),
                    OsString::from("--toolchain-root"),
                    toolchain.as_os_str().to_owned(),
                    OsString::from("--registry-root"),
                    directory.path().join("missing-registry").into_os_string(),
                ],
                "cannot resolve offline registry root",
            ),
        ];

        for (arguments, expected_error) in cases {
            let mut output = Vec::new();
            let mut error = Vec::new();
            assert_eq!(
                run(&arguments, &mut input, &mut output, &mut error).unwrap(),
                2
            );
            assert!(String::from_utf8(error).unwrap().contains(expected_error));
            assert!(output.is_empty());
            assert!(!settings_path.exists());
        }
        assert!(!directory.path().join("missing-registry").exists());
    }

    #[test]
    fn legacy_credential_migration_requires_explicit_consent() {
        let directory = crate::test_support::TempDir::new();
        let paths = settings_paths(&directory);
        let secret = "legacy-token-never-print";
        let mut values = std::collections::BTreeMap::new();
        values.insert("OPENAI_API_KEY", secret);
        let legacy = serde_json::to_vec(&values).unwrap();
        super::super::settings::write_private_file(&paths.credentials, &legacy, "credential store")
            .unwrap();

        let mut input = &b"n\n"[..];
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            configure(&paths, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        assert_eq!(fs::read(&paths.credentials).unwrap(), legacy);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Allow migration when these setup settings are saved?"));
    }

    #[test]
    fn approved_legacy_migration_commits_only_after_settings_are_saved() {
        let directory = crate::test_support::TempDir::new();
        let paths = settings_paths(&directory);
        let secret = "legacy-token-never-print";
        let values = std::collections::BTreeMap::from([("OPENAI_API_KEY", secret)]);
        super::super::settings::write_private_file(
            &paths.credentials,
            &serde_json::to_vec(&values).unwrap(),
            "credential store",
        )
        .unwrap();

        let mut input = &b"y\ny\n\n\nllama3.2:1b\ny\n"[..];
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            configure(&paths, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("credentials were migrated to encrypted system-vault storage"));
        assert!(!output.contains(secret));
        let serialized = fs::read(&paths.credentials).unwrap();
        assert!(!String::from_utf8_lossy(&serialized).contains(secret));
        assert!(super::super::settings::load_credentials(&paths.credentials)
            .unwrap()
            .unwrap()
            .get("OPENAI_API_KEY")
            .is_some_and(|value| value == secret));
    }

    #[test]
    fn approved_legacy_migration_keeps_plaintext_file_when_setup_is_cancelled() {
        let directory = crate::test_support::TempDir::new();
        let paths = settings_paths(&directory);
        let secret = "legacy-token-never-print";
        let values = std::collections::BTreeMap::from([("OPENAI_API_KEY", secret)]);
        let legacy = serde_json::to_vec(&values).unwrap();
        super::super::settings::write_private_file(&paths.credentials, &legacy, "credential store")
            .unwrap();

        let mut input = &b"y\nn\n"[..];
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            configure(&paths, &mut input, &mut output, &mut error).unwrap(),
            0
        );
        assert_eq!(fs::read(&paths.credentials).unwrap(), legacy);
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("approved legacy credential file was left in place"));
        assert!(!paths.model.exists());
    }

    #[test]
    fn provider_list_matches_every_supported_cli_provider() {
        for provider in provider_descriptors() {
            assert!(crate::brain::ModelProvider::parse(provider.selector).is_some());
        }
        assert_eq!(
            provider_descriptors()
                .iter()
                .find(|provider| provider.selector == "cloudflare")
                .unwrap()
                .setup_label,
            "Cloudflare Workers AI (direct)"
        );
        assert_eq!(
            provider_descriptors()
                .iter()
                .find(|provider| provider.selector == "cloudflare-ai-gateway")
                .unwrap()
                .setup_label,
            "Cloudflare AI Gateway"
        );
    }

    #[test]
    fn provider_picker_keeps_the_saved_choice_and_accepts_provider_names() {
        let defaults = ModelDefaults {
            model: "gpt-4.1".to_owned(),
            provider: Some("openai".to_owned()),
            routing_mode: RoutingMode::Manual,
            allow_cross_provider_data_transfer: false,
            fallback_budget_micro_usd: None,
            path: PathBuf::from("/user/config/krypper/config.toml"),
        };
        let mut input = &b"\n"[..];
        let mut output = Vec::new();
        let selected = choose_provider(&mut input, &mut output, Some(&defaults))
            .unwrap()
            .unwrap();
        assert_eq!(selected.selector, "openai");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("OpenAI — credentials needed (current)"));
        assert!(output.contains("Enter keeps current"));

        let mut input = &b"Google Gemini\n"[..];
        let mut output = Vec::new();
        let selected = choose_provider(&mut input, &mut output, None)
            .unwrap()
            .unwrap();
        assert_eq!(selected.selector, "gemini");
    }

    #[test]
    fn model_picker_keeps_the_saved_model_and_shows_catalog_names_with_ids() {
        let directory = crate::test_support::TempDir::new();
        let paths = settings_paths(&directory);
        let openai = provider_descriptor_by_selector("openai").unwrap();
        let mut input = &b"\n"[..];
        let mut output = Vec::new();
        assert_eq!(
            choose_model(&mut input, &mut output, *openai, &paths, Some("gpt-4.1"))
                .unwrap()
                .as_deref(),
            Some("gpt-4.1")
        );

        let mut input = &b"gpt-4.1-m\n\n"[..];
        let mut output = Vec::new();
        let model = choose_model(&mut input, &mut output, *openai, &paths, None)
            .unwrap()
            .unwrap();
        assert!(model.starts_with("gpt-4"));
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("GPT-4.1 mini (gpt-4.1-mini)"), "{output}");
    }

    #[test]
    fn credential_prompt_keeps_an_existing_secret_by_default() {
        let provider = provider_descriptor_by_selector("openai").unwrap();
        let field = provider.auth_fields[0];
        let mut credentials = CredentialStore::empty();
        credentials.insert(field.name, "existing-secret-never-print".to_owned());
        let mut input = &b"\n"[..];
        let mut output = Vec::new();
        collect_credentials(&mut input, &mut output, *provider, &mut credentials).unwrap();
        assert!(credentials
            .get(field.name)
            .is_some_and(|value| value == "existing-secret-never-print"));
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Keep the saved OPENAI_API_KEY credential? [Y/n]"));
        assert!(!output.contains("existing-secret-never-print"));
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
    fn parses_bounded_usd_fallback_budgets_to_micro_usd() {
        for (value, expected) in [
            ("1", Some(1_000_000)),
            ("0.25", Some(250_000)),
            ("0.000001", Some(1)),
            ("1000.000000", Some(1_000_000_000)),
            ("0", None),
            ("0.0000001", None),
            ("1000.000001", None),
            ("1e2", None),
            ("-1", None),
        ] {
            assert_eq!(parse_usd_micro(value), expected, "{value}");
        }
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
            permission_mode: PathBuf::from("/user/config/krypper/permission-mode"),
            provider_profiles: PathBuf::from("/user/config/krypper/provider-profiles.json"),
            model_evidence: PathBuf::from("/user/config/krypper/model-evidence.json"),
            model_evidence_key: PathBuf::from("/user/config/krypper/model-evidence.key"),
            offline_process: PathBuf::from("/user/config/krypper/offline-process.json"),
        };
        let defaults = ModelDefaults {
            model: "test-model".to_owned(),
            provider: Some("openai".to_owned()),
            routing_mode: RoutingMode::Manual,
            allow_cross_provider_data_transfer: false,
            fallback_budget_micro_usd: None,
            path: paths.model.clone(),
        };
        let mut output = Vec::new();
        write_status(
            &mut output,
            Some(&defaults),
            credentials.len(),
            0,
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
            permission_mode: PathBuf::from("/user/config/krypper/permission-mode"),
            provider_profiles: PathBuf::from("/user/config/krypper/provider-profiles.json"),
            model_evidence: PathBuf::from("/user/config/krypper/model-evidence.json"),
            model_evidence_key: PathBuf::from("/user/config/krypper/model-evidence.key"),
            offline_process: PathBuf::from("/user/config/krypper/offline-process.json"),
        };
        let provider = provider_descriptors()
            .iter()
            .find(|provider| provider.selector == "openai")
            .unwrap();
        let mut output = Vec::new();
        write_review(&mut output, &paths, *provider, "gpt-4.1", 1, 0).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("OpenAI"));
        assert!(output.contains("gpt-4.1"));
        assert!(output.contains("values hidden"));
        assert!(!output.contains(secret));
    }
}
