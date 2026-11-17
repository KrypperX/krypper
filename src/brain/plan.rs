use crate::harness::execution::{workspace_action_name, WorkspaceAction};
use crate::task_lifecycle::WorkspaceGuidanceReadError;
use crate::tools::registry::workspace_operations;
use std::fmt;

pub(crate) const MAX_PLAN_OPERATIONS: usize = 32;

/// An ordered set of workspace operations proposed for one task turn.
///
/// Plans contain no task-control actions and do not carry a success claim. Harness executes each
/// operation through TaskSession and the Tools registry before using the existing control path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePlan {
    operations: Vec<WorkspaceAction>,
}

impl WorkspacePlan {
    /// Creates a plan containing only registered workspace operations up to the requested limit.
    pub fn try_from_actions(
        operations: Vec<WorkspaceAction>,
        maximum: usize,
    ) -> Result<Self, WorkspacePlanError> {
        let maximum = maximum.min(MAX_PLAN_OPERATIONS);
        if operations.is_empty() {
            return Err(WorkspacePlanError::EmptyPlan);
        }
        if operations.len() > maximum {
            return Err(WorkspacePlanError::TooManyOperations { maximum });
        }
        if operations.iter().any(|action| {
            !is_workspace_operation(action)
                || !workspace_operations()
                    .iter()
                    .any(|operation| operation.name() == workspace_action_name(action))
        }) {
            return Err(WorkspacePlanError::UnsupportedOperation);
        }

        Ok(Self { operations })
    }

    /// Returns the operations in their proposed execution order.
    pub fn operations(&self) -> &[WorkspaceAction] {
        &self.operations
    }

    pub(crate) fn into_actions(self) -> Vec<WorkspaceAction> {
        self.operations
    }
}

fn is_workspace_operation(action: &WorkspaceAction) -> bool {
    matches!(
        action,
        WorkspaceAction::OfflineCargoTest { .. }
            | WorkspaceAction::ReadFile { .. }
            | WorkspaceAction::ReadRange { .. }
            | WorkspaceAction::ListDirectory { .. }
            | WorkspaceAction::CreateDirectory { .. }
            | WorkspaceAction::SearchText { .. }
            | WorkspaceAction::SearchTreeText { .. }
            | WorkspaceAction::CreateFile { .. }
            | WorkspaceAction::ReplaceText { .. }
            | WorkspaceAction::ApplyPatch { .. }
    )
}

/// Why Brain could not provide a valid bounded workspace plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspacePlanError {
    /// A plan must contain at least one workspace operation.
    EmptyPlan,
    /// The model proposed more operations than the configured bound.
    TooManyOperations { maximum: usize },
    /// The response contained a tool that is not a registered workspace operation.
    UnsupportedOperation,
    /// A workspace operation had invalid arguments.
    InvalidOperation,
    /// The model response did not contain a valid ordered plan.
    InvalidResponse,
    /// The model request could not be completed.
    ModelRequestFailed,
    /// No action-budget slot remains for a workspace operation and existing control decision.
    ActionBudgetExhausted,
    /// Root project guidance could not be read safely from the task workspace.
    RootGuidanceRead(WorkspaceGuidanceReadError),
}

impl fmt::Display for WorkspacePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPlan => formatter.write_str("workspace plan contained no operations"),
            Self::TooManyOperations { maximum } => write!(
                formatter,
                "workspace plan exceeded its limit of {maximum} operations"
            ),
            Self::UnsupportedOperation => {
                formatter.write_str("workspace plan contained an unsupported operation")
            }
            Self::InvalidOperation => {
                formatter.write_str("workspace plan contained an invalid operation")
            }
            Self::InvalidResponse => {
                formatter.write_str("model response did not contain a valid workspace plan")
            }
            Self::ModelRequestFailed => {
                formatter.write_str("model request for a workspace plan failed")
            }
            Self::ActionBudgetExhausted => formatter.write_str(
                "action budget must leave room for a workspace operation and task control",
            ),
            Self::RootGuidanceRead(error) => {
                write!(formatter, "root project guidance read failed: {error}")
            }
        }
    }
}

impl std::error::Error for WorkspacePlanError {}
