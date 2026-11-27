//! Goal-turn execution: the action vocabulary, one action execution and the turn loop.
//!
//! `types` owns the action/observation/decision vocabulary, `action` executes one workspace
//! action, `turn` runs the bounded turn loop, `dispatch` hosts extension execution and
//! `planned` owns plan and correction flows. This file is a facade so every existing
//! `crate::harness::execution::*` path keeps working.
//!
//! Extension handlers stay host-owned: a provider supplies only a registered name and JSON
//! arguments. Turn ownership, grants and schema are rechecked before an effect runs, and only the
//! prompt-aware API can approve it for one exact task/turn/workspace intent. Denials fail closed;
//! observations keep the name without raw arguments. Callers that supply no registry retain
//! built-in-only behavior; the `WorkspaceAction::Extension` variant is the scoped public-action
//! migration for downstream exhaustive matches.

mod action;
mod brain_corrections;
mod dispatch;
mod planned;
mod turn;
mod types;

#[cfg(test)]
#[path = "execution/extension_tests.rs"]
mod extension_tests;

pub(crate) use self::planned::process_test_verification;
pub(crate) use self::planned::review_existing_impact_plan;
pub(crate) use self::planned::stop_reason_after_test_verification;
pub(crate) use self::planned::task_test_verification;
pub use self::planned::{
    create_impact_checked_plan, create_plan_with_repository_context,
    create_plan_with_workspace_context, run_goal_turn_with_completion_correction,
    run_goal_turn_with_impact_checked_completion_correction,
    run_goal_turn_with_impact_checked_plan, run_goal_turn_with_plan,
    run_goal_turn_with_process_failure_correction, CompletionCorrectionError,
    CompletionCorrectionResult, ProcessFailureCorrectionError, ProcessFailureCorrectionResult,
    ProcessTestVerificationStatus,
};
pub(crate) use self::planned::{
    run_goal_turn_with_impact_checked_completion_correction_and_permission_gate,
    run_goal_turn_with_impact_checked_plan_and_permission_gate,
};
pub use self::turn::{run_goal_turn, run_goal_turn_with_budget};
pub use self::turn::{
    run_goal_turn_with_approved_extensions, run_goal_turn_with_budget_and_approved_extensions,
    run_goal_turn_with_budget_and_extensions, run_goal_turn_with_extensions, TurnExtensions,
};
pub(crate) use self::turn::{run_goal_turn_with_budget_and_permission_gate, ActionPermissionGate};
pub(crate) use crate::contracts::goal_turn::summarize_observation_activity;
pub use crate::contracts::goal_turn::{
    BoundedDecisionSource, DecisionFailureCode, DecisionSource, GoalTurnError, GoalTurnResult,
    GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};

pub use self::dispatch::{
    dispatch_registered_extension_with_approval_for_turn,
    dispatch_registered_extension_with_arguments_for_turn, CurrentExtensionContext,
    ExtensionApprovalGate, ExtensionApprovalPrompt, ExtensionApprovalRequest,
    ExtensionDispatchError, RegisteredExtensionWithArguments,
};
pub(crate) use crate::contracts::goal_turn::{workspace_action_name, workspace_mutation_target};
