use crate::brain::provider_bridge::{
    authorize_dispatch_with_arguments, BridgeError, ExtensionTool,
};
use crate::tools::capability_policy::CapabilityContext;

fn dispatch_authorized_extension_with_arguments<T>(
    tools: &[ExtensionTool<'_>],
    context: CapabilityContext,
    name: &str,
    arguments: &serde_json::Value,
    execute: impl FnOnce(&serde_json::Value) -> T,
) -> Result<T, BridgeError> {
    authorize_dispatch_with_arguments(tools, context, name, arguments)?;
    Ok(execute(arguments))
}

/// A host-owned handler that receives validated model arguments.
/// Registration is not a capability grant.
pub(crate) struct RegisteredExtensionWithArguments<'a, T> {
    pub tool: ExtensionTool<'a>,
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
    context: CapabilityContext,
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
        .find(|entry| entry.tool.descriptor.qualified_name() == name)
        .ok_or(ExtensionDispatchError::Bridge(BridgeError::UnknownTool))?;
    let tools = registry
        .iter()
        .map(|entry| ExtensionTool {
            descriptor: entry.tool.descriptor.clone(),
            description: entry.tool.description,
            parameters: entry.tool.parameters.clone(),
            grant: entry.tool.grant,
        })
        .collect::<Vec<_>>();
    dispatch_authorized_extension_with_arguments(&tools, context, name, arguments, selected.execute)
        .map_err(ExtensionDispatchError::Bridge)
}

/// Distinguishes lifecycle failures from capability denials without executing a handler.
#[derive(Debug)]
pub(crate) enum ExtensionDispatchError {
    Turn(crate::task_lifecycle::WorkspaceReadError),
    TaskMismatch,
    Bridge(BridgeError),
}
