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

/// Invoke a host-owned extension handler only after rechecking task authority.
///
/// The callback represents the existing Harness-owned execution boundary;
/// provider output cannot supply either the grant or the handler.
pub(crate) fn dispatch_authorized_extension<T>(
    tools: &[crate::brain::provider_bridge::ExtensionTool<'_>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
    execute: impl FnOnce() -> T,
) -> Result<T, crate::brain::provider_bridge::BridgeError> {
    authorize_extension_at_harness(tools, context, name)?;
    Ok(execute())
}

/// Dispatch a host-registered extension only for an active task-owned turn.
///
/// The caller supplies the registered descriptor, host-issued grant and handler;
/// model output supplies only the tool name. Turn ownership is verified before
/// capability evaluation, and the handler is never called after either denial.
pub(crate) fn dispatch_extension_for_turn<T>(
    session: &crate::task_lifecycle::TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    tools: &[crate::brain::provider_bridge::ExtensionTool<'_>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
    execute: impl FnOnce() -> T,
) -> Result<T, ExtensionDispatchError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(ExtensionDispatchError::Turn)?;
    if context.task_id != task_id {
        return Err(ExtensionDispatchError::TaskMismatch);
    }
    dispatch_authorized_extension(tools, context, name, execute)
        .map_err(ExtensionDispatchError::Bridge)
}

/// Distinguishes lifecycle failures from capability denials without executing a handler.
#[derive(Debug)]
pub(crate) enum ExtensionDispatchError {
    Turn(crate::task_lifecycle::WorkspaceReadError),
    TaskMismatch,
    Bridge(crate::brain::provider_bridge::BridgeError),
}

#[cfg(test)]
mod extension_gate_tests {
    use super::authorize_extension_at_harness;
    use crate::brain::provider_bridge::{BridgeError, ExtensionTool};
    use crate::tools::capability_policy::{CapabilityContext, CapabilityDenial, CapabilityGrant};
    use crate::tools::extension_contract::{
        ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
    };
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn rejected_extension_never_invokes_executor() {
        let context = CapabilityContext {
            task_id: Uuid::new_v4(),
            generation: 1,
            task_authorized: true,
        };
        let mut executed = false;
        let result = super::dispatch_authorized_extension(&[], context, "external_unknown", || {
            executed = true;
        });
        assert_eq!(result, Err(BridgeError::UnknownTool));
        assert!(!executed);
    }

    #[test]
    fn authorized_extension_invokes_executor_once_and_revocation_blocks_it() {
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
        let mut executions = 0;
        let result = super::dispatch_authorized_extension(&tools, context, "external_read", || {
            executions += 1;
            "executed"
        });
        assert_eq!(result, Ok("executed"));
        assert_eq!(executions, 1);

        let revoked = CapabilityContext {
            generation: 5,
            ..context
        };
        let denied = super::dispatch_authorized_extension(&tools, revoked, "external_read", || {
            executions += 1;
            "must not execute"
        });
        assert_eq!(
            denied,
            Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
        );
        assert_eq!(executions, 1);
    }

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
                CapabilityContext {
                    generation: 5,
                    ..context
                },
                "external_read"
            ),
            Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
        );
        assert_eq!(
            authorize_extension_at_harness(
                &tools,
                CapabilityContext {
                    task_authorized: false,
                    ..context
                },
                "external_read"
            ),
            Err(BridgeError::Denied(CapabilityDenial::TaskUnauthorized))
        );
    }
}
