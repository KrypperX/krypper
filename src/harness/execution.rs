//! Goal-turn execution: the action vocabulary, one action execution and the turn loop.
//!
//! `types` owns the action/observation/decision vocabulary, `action` executes one workspace
//! action, `turn` runs the bounded turn loop and `planned` owns plan and correction flows.
//! This file is a facade so every existing `crate::harness::execution::*` path keeps working.

mod action;
mod planned;
mod turn;
mod types;

pub use self::planned::{
    create_plan_with_workspace_context, run_goal_turn_with_completion_correction,
    run_goal_turn_with_plan, run_goal_turn_with_process_failure_correction,
    CompletionCorrectionError, CompletionCorrectionResult, ProcessFailureCorrectionError,
    ProcessFailureCorrectionResult, ProcessTestVerificationStatus,
};
pub use self::turn::{run_goal_turn, run_goal_turn_with_budget};
pub use self::types::{
    BoundedDecisionSource, DecisionSource, GoalTurnError, GoalTurnResult, GoalTurnStopReason,
    WorkspaceAction, WorkspaceObservation,
};

pub(crate) use self::action::summarize_observation;
pub(crate) use self::types::workspace_action_name;

/// Recheck host-issued extension authority at the Harness boundary.
///
/// This gate never executes a provider-supplied call. Future extension
/// executors must pass this check before using the existing TaskSession path.
pub(crate) fn authorize_extension_at_harness(
    tools: &[crate::brain::provider_bridge::ExtensionTool<'_>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
) -> Result<(), crate::brain::provider_bridge::BridgeError> {
    crate::brain::provider_bridge::authorize_dispatch(tools, context, name)
}

#[cfg(test)]
mod extension_gate_tests {
    use super::authorize_extension_at_harness;
    use crate::brain::provider_bridge::{BridgeError, ExtensionTool};
    use crate::tools::capability_policy::{
        CapabilityContext, CapabilityDenial, CapabilityGrant,
    };
    use crate::tools::extension_contract::{
        ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
    };
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn harness_rechecks_task_and_grant_before_extension_dispatch() {
        let task_id = Uuid::new_v4();
        let grant = CapabilityGrant {
            task_id,
            namespace: "external".into(),
            operation: "read".into(),
            effect: ExtensionEffect::ReadWorkspace,
            generation: 4,
            host_approved: false,
        };
        let tools = [ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({"type":"object","properties":{}}),
            grant: &grant,
        }];
        let context = CapabilityContext {
            task_id,
            generation: 4,
            task_authorized: true,
        };
        assert_eq!(
            authorize_extension_at_harness(&tools, context, "external_read"),
            Ok(())
        );
        assert_eq!(
            authorize_extension_at_harness(&tools, context, "external_write"),
            Err(BridgeError::UnknownTool)
        );
        assert_eq!(
            authorize_extension_at_harness(
                &tools,
                CapabilityContext { generation: 5, ..context },
                "external_read"
            ),
            Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
        );
        assert_eq!(
            authorize_extension_at_harness(
                &tools,
                CapabilityContext { task_authorized: false, ..context },
                "external_read"
            ),
            Err(BridgeError::Denied(CapabilityDenial::TaskUnauthorized))
        );
    }
}
