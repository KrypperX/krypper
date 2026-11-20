//! Goal-turn execution: the action vocabulary, one action execution and the turn loop.
//!
//! `types` owns the action/observation/decision vocabulary, `action` executes one workspace
//! action, `turn` runs the bounded turn loop and `planned` owns plan and correction flows.
//! This file is a facade so every existing `crate::harness::execution::*` path keeps working.
//!
//! Extension handlers stay host-owned: a provider supplies only a registered name and JSON
//! arguments. Turn ownership, grants and schema are rechecked before a handler runs, denials
//! fail closed, and observations keep the name without raw arguments. Callers that supply no
//! registry retain built-in-only behavior; the new `WorkspaceAction::Extension` variant is the
//! scoped public-action migration for downstream exhaustive matches.

mod action;
mod planned;
mod turn;
mod types;

#[cfg(test)]
#[path = "execution/extension_tests.rs"]
mod extension_tests;

pub use self::planned::{
    create_plan_with_workspace_context, run_goal_turn_with_completion_correction,
    run_goal_turn_with_plan, run_goal_turn_with_process_failure_correction,
    CompletionCorrectionError, CompletionCorrectionResult, ProcessFailureCorrectionError,
    ProcessFailureCorrectionResult, ProcessTestVerificationStatus,
};
pub use self::turn::{run_goal_turn, run_goal_turn_with_budget};
pub(crate) use self::turn::{
    run_goal_turn_with_budget_and_extensions, run_goal_turn_with_extensions, TurnExtensions,
};
pub use self::types::{
    BoundedDecisionSource, DecisionSource, GoalTurnError, GoalTurnResult, GoalTurnStopReason,
    WorkspaceAction, WorkspaceObservation,
};

pub(crate) use self::action::summarize_observation;
pub(crate) use self::types::{workspace_action_name, workspace_mutation_target};

/// Dispatch a model-supplied argument object through the existing Harness gate.
///
/// Authority and schema validation both precede the host-owned executor.
/// The model may supply arguments, but never the handler or its grant.
pub(crate) fn dispatch_authorized_extension_with_arguments<T>(
    tools: &[crate::brain::provider_bridge::ExtensionTool<'_>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
    arguments: &serde_json::Value,
    execute: impl FnOnce(&serde_json::Value) -> T,
) -> Result<T, crate::brain::provider_bridge::BridgeError> {
    crate::brain::provider_bridge::authorize_dispatch_with_arguments(
        tools, context, name, arguments,
    )?;
    Ok(execute(arguments))
}

/// A host-owned handler that receives validated model arguments.
/// Registration is not a capability grant.
pub(crate) struct RegisteredExtensionWithArguments<'a, T> {
    pub tool: crate::brain::provider_bridge::ExtensionTool<'a>,
    pub execute: fn(&serde_json::Value) -> T,
}

/// Dispatch an argument-aware registered handler for an active, task-owned turn.
///
/// Both the host-issued capability and the declared JSON argument schema are
/// checked before invoking the handler. A model can choose a registered name
/// and supply arguments, but cannot install a handler or mint a grant.
pub(crate) fn dispatch_registered_extension_with_arguments_for_turn<T>(
    session: &crate::task_lifecycle::TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    registry: &[RegisteredExtensionWithArguments<'_, T>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<T, ExtensionDispatchError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(ExtensionDispatchError::Turn)?;
    if context.task_id != task_id {
        return Err(ExtensionDispatchError::TaskMismatch);
    }
    let selected = registry
        .iter()
        .find(|entry| {
            format!(
                "{}_{}",
                entry.tool.descriptor.namespace, entry.tool.descriptor.name
            ) == name
        })
        .ok_or(ExtensionDispatchError::Bridge(
            crate::brain::provider_bridge::BridgeError::UnknownTool,
        ))?;
    let tools = registry
        .iter()
        .map(|entry| crate::brain::provider_bridge::ExtensionTool {
            descriptor: entry.tool.descriptor.clone(),
            description: entry.tool.description,
            parameters: entry.tool.parameters.clone(),
            grant: entry.tool.grant,
        })
        .collect::<Vec<_>>();
    dispatch_extension_with_arguments_for_turn(
        session,
        task_id,
        turn_id,
        &tools,
        context,
        name,
        arguments,
        selected.execute,
    )
}

/// Invoke an argument-aware extension only within the active, owning turn.
///
/// This uses the same task lifecycle and capability gate as built-in actions;
/// no extension-owned execution loop or provider-issued grant is introduced.
pub(crate) fn dispatch_extension_with_arguments_for_turn<T>(
    session: &crate::task_lifecycle::TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    tools: &[crate::brain::provider_bridge::ExtensionTool<'_>],
    context: crate::tools::capability_policy::CapabilityContext,
    name: &str,
    arguments: &serde_json::Value,
    execute: impl FnOnce(&serde_json::Value) -> T,
) -> Result<T, ExtensionDispatchError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(ExtensionDispatchError::Turn)?;
    if context.task_id != task_id {
        return Err(ExtensionDispatchError::TaskMismatch);
    }
    dispatch_authorized_extension_with_arguments(tools, context, name, arguments, execute)
        .map_err(ExtensionDispatchError::Bridge)
}

/// Distinguishes lifecycle failures from capability denials without executing a handler.
#[derive(Debug)]
pub(crate) enum ExtensionDispatchError {
    Turn(crate::task_lifecycle::WorkspaceReadError),
    TaskMismatch,
    Bridge(crate::brain::provider_bridge::BridgeError),
}
