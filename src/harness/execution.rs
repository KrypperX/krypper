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
