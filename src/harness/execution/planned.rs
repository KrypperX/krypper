use super::{
    run_goal_turn_with_budget, workspace_action_name, BoundedDecisionSource, GoalTurnError,
    GoalTurnResult, WorkspaceAction, WorkspaceObservation,
};
use crate::brain::{WorkspacePlan, WorkspacePlanError};
use crate::task_lifecycle::TaskSession;
use crate::tools::registry::workspace_operations;
use uuid::Uuid;

/// Feeds plan operations through the existing bounded turn executor.
pub fn run_goal_turn_with_plan<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    plan: Result<WorkspacePlan, WorkspacePlanError>,
    control_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    let operations = match plan {
        Ok(plan) => plan.into_actions(),
        Err(error) => vec![WorkspaceAction::Unsupported {
            action: error.to_string(),
        }],
    };
    let mut source = PlanDecisionSource {
        operations: operations.into_iter(),
        control_source,
    };

    run_goal_turn_with_budget(session, task_id, turn_id, goal, max_actions, &mut source)
}

struct PlanDecisionSource<'a, D: ?Sized> {
    operations: std::vec::IntoIter<WorkspaceAction>,
    control_source: &'a mut D,
}

impl<D: BoundedDecisionSource + ?Sized> BoundedDecisionSource for PlanDecisionSource<'_, D> {
    fn choose_action(
        &mut self,
        goal: &str,
        observations: &[WorkspaceObservation],
    ) -> WorkspaceAction {
        match self.operations.next() {
            Some(action) if is_registered_workspace_operation(&action) => action,
            Some(action) => unsupported_action(action),
            None => match self.control_source.choose_action(goal, observations) {
                action @ (WorkspaceAction::Complete { .. }
                | WorkspaceAction::Continue
                | WorkspaceAction::Cancel) => action,
                action => unsupported_action(action),
            },
        }
    }
}

fn is_registered_workspace_operation(action: &WorkspaceAction) -> bool {
    matches!(
        action,
        WorkspaceAction::ReadFile { .. }
            | WorkspaceAction::ReadRange { .. }
            | WorkspaceAction::ListDirectory { .. }
            | WorkspaceAction::CreateDirectory { .. }
            | WorkspaceAction::SearchText { .. }
            | WorkspaceAction::SearchTreeText { .. }
            | WorkspaceAction::CreateFile { .. }
            | WorkspaceAction::ReplaceText { .. }
    ) && workspace_operations()
        .iter()
        .any(|operation| operation.name() == workspace_action_name(action))
}

fn unsupported_action(action: WorkspaceAction) -> WorkspaceAction {
    match action {
        WorkspaceAction::Unsupported { .. } => action,
        action => WorkspaceAction::Unsupported {
            action: workspace_action_name(&action).to_owned(),
        },
    }
}
