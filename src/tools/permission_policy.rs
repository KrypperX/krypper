//! Host-selected permissions for task effects.
//!
//! The mode is resolved by the CLI from per-user settings. Model output,
//! workspace files, provider settings, and checkpoints cannot select it.

/// The permission profile selected by a user for a task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionMode {
    /// Ask before effects that exceed bounded workspace authority.
    Ask,
    /// Automatically allow bounded workspace effects and the fixed offline process profile.
    RiskBased,
    /// Direct host process execution after explicit, task-level user confirmation.
    FullAccess,
}

// The architecture guard does not inspect the enum's `#[default]` variant attribute.
#[allow(clippy::derivable_impls)]
impl Default for PermissionMode {
    fn default() -> Self {
        Self::Ask
    }
}

impl PermissionMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::RiskBased => "risk-based",
            Self::FullAccess => "full-access",
        }
    }

    pub(crate) const fn display_name(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::RiskBased => "Risk-based approval",
            Self::FullAccess => "Full Access",
        }
    }

    /// Parse a user-visible permission name. Full Access is a saved preference only;
    /// each task must obtain its own trusted confirmation before provider startup.
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "ask" => Ok(Self::Ask),
            "risk-based" => Ok(Self::RiskBased),
            "full-access" => Ok(Self::FullAccess),
            _ => Err("permission mode must be ask, risk-based, or full-access"),
        }
    }
}

/// Risk class from the host-owned action vocabulary, never model descriptions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectClass {
    Workspace,
    FixedOfflineProcess,
    FullAccessHostProcess,
    ExternalOrUnclassified,
    TaskControl,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionDecision {
    Allow,
    Ask,
    Deny,
    Unavailable,
}

/// Resolve registered CLI operations through the same host-owned registry metadata used by
/// provider tool declarations. Missing metadata is treated as a risky external effect.
pub(crate) fn classify_registered_operation(name: &str) -> EffectClass {
    use crate::tools::registry::EffectClass as RegisteredEffect;

    match crate::tools::registry::effect_class_for(name) {
        Some(RegisteredEffect::ReadOnlyWorkspaceAccess | RegisteredEffect::WorkspaceMutation) => {
            EffectClass::Workspace
        }
        Some(RegisteredEffect::FixedSandboxedOfflineCargoTest) => EffectClass::FixedOfflineProcess,
        Some(RegisteredEffect::SandboxedOfflineReadOnlyExec) => EffectClass::ExternalOrUnclassified,
        Some(RegisteredEffect::FullAccessHostProcess) => EffectClass::FullAccessHostProcess,
        None => EffectClass::ExternalOrUnclassified,
    }
}

/// Decide whether the trusted host can dispatch an action under the selected mode.
pub(crate) fn decide(mode: PermissionMode, effect: EffectClass) -> PermissionDecision {
    match effect {
        EffectClass::Workspace | EffectClass::TaskControl => PermissionDecision::Allow,
        EffectClass::FullAccessHostProcess if mode == PermissionMode::FullAccess => {
            PermissionDecision::Allow
        }
        EffectClass::FullAccessHostProcess => PermissionDecision::Unavailable,
        EffectClass::ExternalOrUnclassified if mode == PermissionMode::FullAccess => {
            PermissionDecision::Unavailable
        }
        EffectClass::FixedOfflineProcess if mode == PermissionMode::RiskBased => {
            PermissionDecision::Allow
        }
        EffectClass::FixedOfflineProcess | EffectClass::ExternalOrUnclassified => {
            PermissionDecision::Ask
        }
        EffectClass::Unsupported => PermissionDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_registered_operation, decide, EffectClass, PermissionDecision, PermissionMode,
    };

    #[test]
    fn ask_and_risk_based_keep_workspace_effects_inside_existing_scope() {
        for mode in [PermissionMode::Ask, PermissionMode::RiskBased] {
            assert_eq!(
                decide(mode, EffectClass::Workspace),
                PermissionDecision::Allow
            );
            assert_eq!(
                decide(mode, EffectClass::TaskControl),
                PermissionDecision::Allow
            );
        }
    }

    #[test]
    fn ask_prompts_for_each_process_effect_while_risk_based_uses_only_the_fixed_profile() {
        assert_eq!(
            decide(PermissionMode::Ask, EffectClass::FixedOfflineProcess),
            PermissionDecision::Ask
        );
        assert_eq!(
            decide(PermissionMode::Ask, EffectClass::ExternalOrUnclassified),
            PermissionDecision::Ask
        );
        assert_eq!(
            decide(PermissionMode::RiskBased, EffectClass::FixedOfflineProcess),
            PermissionDecision::Allow
        );
        assert_eq!(
            decide(
                PermissionMode::RiskBased,
                EffectClass::ExternalOrUnclassified
            ),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn model_selected_read_only_exec_still_prompts_under_risk_based_mode() {
        let effect = classify_registered_operation("offline_read_only_exec");

        assert_eq!(effect, EffectClass::ExternalOrUnclassified);
        assert_eq!(decide(PermissionMode::Ask, effect), PermissionDecision::Ask);
        assert_eq!(
            decide(PermissionMode::RiskBased, effect),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn full_access_allows_registered_workspace_and_host_effects_but_keeps_unknowns_closed() {
        assert_eq!(
            decide(PermissionMode::FullAccess, EffectClass::Workspace),
            PermissionDecision::Allow
        );
        assert_eq!(
            decide(PermissionMode::FullAccess, EffectClass::TaskControl),
            PermissionDecision::Allow
        );
        assert_eq!(
            decide(
                PermissionMode::FullAccess,
                EffectClass::FullAccessHostProcess
            ),
            PermissionDecision::Allow
        );
        assert_eq!(
            decide(PermissionMode::FullAccess, EffectClass::FixedOfflineProcess),
            PermissionDecision::Ask
        );
        assert_eq!(
            decide(
                PermissionMode::FullAccess,
                EffectClass::ExternalOrUnclassified
            ),
            PermissionDecision::Unavailable
        );
        assert_eq!(
            decide(PermissionMode::FullAccess, EffectClass::Unsupported),
            PermissionDecision::Deny
        );
        assert_eq!(
            decide(PermissionMode::RiskBased, EffectClass::Unsupported),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn full_access_preference_does_not_replace_task_confirmation() {
        assert_eq!(
            PermissionMode::parse("full-access"),
            Ok(PermissionMode::FullAccess)
        );
        assert_eq!(PermissionMode::parse("ask"), Ok(PermissionMode::Ask));
        assert_eq!(
            PermissionMode::parse("risk-based"),
            Ok(PermissionMode::RiskBased)
        );
    }

    #[test]
    fn registered_cli_effects_share_the_tools_registry_and_unknowns_are_risky() {
        assert_eq!(
            classify_registered_operation("read_file"),
            EffectClass::Workspace
        );
        assert_eq!(
            classify_registered_operation("apply_patch"),
            EffectClass::Workspace
        );
        assert_eq!(
            classify_registered_operation("offline_cargo_test"),
            EffectClass::FixedOfflineProcess
        );
        assert_eq!(
            classify_registered_operation("unknown_effect"),
            EffectClass::ExternalOrUnclassified
        );
        assert_eq!(
            classify_registered_operation("full_access_host_process"),
            EffectClass::FullAccessHostProcess
        );
    }

    #[test]
    fn full_access_does_not_change_existing_offline_process_authorization() {
        assert_eq!(
            classify_registered_operation("offline_cargo_test"),
            EffectClass::FixedOfflineProcess
        );
        assert_eq!(
            decide(PermissionMode::FullAccess, EffectClass::FixedOfflineProcess),
            PermissionDecision::Ask
        );
        assert_eq!(
            classify_registered_operation("offline_read_only_exec"),
            EffectClass::ExternalOrUnclassified
        );
        assert_eq!(
            decide(
                PermissionMode::FullAccess,
                classify_registered_operation("offline_read_only_exec")
            ),
            PermissionDecision::Unavailable
        );
    }
}
