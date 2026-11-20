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
pub(crate) use self::types::{workspace_action_name, workspace_mutation_target};
