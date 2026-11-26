//! The bounded goal-turn loop that drives decisions, actions and turn completion.

use super::action::{execute_action_with_revision, summarize_observation_activity};
use super::types::{
    workspace_mutation_target, ActionExecution, BoundedDecisionSource, DecisionSource,
    GoalTurnError, GoalTurnResult, GoalTurnStopReason, TurnCompletionData, WorkspaceObservation,
};
use crate::contracts::extensions::ExtensionEffect;
use crate::task_lifecycle::{TaskSession, TurnActivitySummary, TurnOutcome};
use crate::tools::permission_policy::{
    classify_registered_operation, decide, EffectClass, PermissionDecision, PermissionMode,
};
use uuid::Uuid;

/// Synchronous host confirmation for one exact task action. The action and both IDs are
/// borrowed only for this dispatch attempt; a successful answer is never retained or reused.
pub(crate) type PermissionPrompt<'a> =
    dyn FnMut(Uuid, Uuid, &super::WorkspaceAction) -> Result<bool, ()> + 'a;

/// Host-owned per-turn action gate. The fixed process capability flag describes only whether
/// the embedding has provisioned the inputs needed to build the existing fixed offline profile;
/// TaskSession's B4 grant remains the final process authorization.
pub(crate) struct ActionPermissionGate<'a> {
    mode: PermissionMode,
    fixed_offline_process_available: bool,
    prompt: Option<&'a mut PermissionPrompt<'a>>,
}

impl<'a> ActionPermissionGate<'a> {
    pub(crate) fn new(
        mode: PermissionMode,
        fixed_offline_process_available: bool,
        prompt: Option<&'a mut PermissionPrompt<'a>>,
    ) -> Self {
        Self {
            mode,
            fixed_offline_process_available,
            prompt,
        }
    }

    fn authorize(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        action: &super::WorkspaceAction,
        extensions: Option<&TurnExtensions<'_>>,
    ) -> Result<(), GoalTurnError> {
        use super::WorkspaceAction;

        // Unsupported actions are inert error reports handled by the dispatcher. Let them reach
        // that path so malformed input and provider failures retain their normal turn outcome.
        if matches!(action, WorkspaceAction::Unsupported { .. }) {
            return Ok(());
        }

        let action_name = super::workspace_action_name(action);
        let effect = match action {
            WorkspaceAction::Complete { .. }
            | WorkspaceAction::Continue
            | WorkspaceAction::Cancel => EffectClass::TaskControl,
            WorkspaceAction::Extension { name, .. } => {
                let Some(entry) = extensions.and_then(|extensions| {
                    extensions
                        .registry
                        .iter()
                        .find(|entry| entry.tool.descriptor.qualified_name() == name.as_str())
                }) else {
                    return Err(GoalTurnError::PermissionDenied {
                        action: action_name,
                    });
                };
                // A registered effect is a risk hint only. Extensions execute host handlers
                // outside TaskSession's bounded workspace calls, so every extension needs an
                // exact prompt and still passes its existing capability/schema gate afterward.
                match entry.tool.descriptor.effect {
                    ExtensionEffect::ReadWorkspace
                    | ExtensionEffect::MutateWorkspace
                    | ExtensionEffect::ExecuteProcess
                    | ExtensionEffect::Network
                    | ExtensionEffect::ExternalService => EffectClass::ExternalOrUnclassified,
                }
            }
            WorkspaceAction::Unsupported { .. } => EffectClass::Unsupported,
            // The registry is the only source of effect classifications for executable tools.
            // An action without registered metadata is treated as risky and prompts in Ask or
            // fails closed when no trusted prompt channel exists.
            _ => classify_registered_operation(action_name),
        };

        if effect == EffectClass::FixedOfflineProcess && !self.fixed_offline_process_available {
            return Err(GoalTurnError::PermissionUnavailable {
                action: action_name,
            });
        }

        match decide(self.mode, effect) {
            PermissionDecision::Allow => Ok(()),
            PermissionDecision::Deny => Err(GoalTurnError::PermissionDenied {
                action: action_name,
            }),
            PermissionDecision::Unavailable => Err(GoalTurnError::PermissionUnavailable {
                action: action_name,
            }),
            PermissionDecision::Ask => {
                let prompt = self.prompt.as_deref_mut().ok_or(
                    GoalTurnError::PermissionPromptUnavailable {
                        action: action_name,
                    },
                )?;
                match prompt(task_id, turn_id, action) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(GoalTurnError::PermissionDenied {
                        action: action_name,
                    }),
                    Err(()) => Err(GoalTurnError::PermissionPromptFailed {
                        action: action_name,
                    }),
                }
            }
        }
    }
}

/// Host-owned extension configuration for an existing goal-turn execution.
///
/// A provider may request a name but cannot supply the registry, grant, or
/// capability context. Unregistered names keep the ordinary fail-closed path.
pub struct TurnExtensions<'a> {
    /// Trusted host handlers and their grants; never supplied by the model.
    pub registry: &'a [super::RegisteredExtensionWithArguments<'a, String>],
    /// Trusted capability context for the active task and generation.
    pub context: crate::tools::capability_policy::CapabilityContext,
}

fn execute_with_extensions(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    action: &super::WorkspaceAction,
    extensions: Option<&TurnExtensions<'_>>,
    expected_revision: Option<&str>,
    permission_gate: Option<&mut ActionPermissionGate<'_>>,
) -> Result<ActionExecution, GoalTurnError> {
    if let Some(permission_gate) = permission_gate {
        permission_gate.authorize(task_id, turn_id, action, extensions)?;
    }
    if let (super::WorkspaceAction::Extension { name, arguments }, Some(extensions)) =
        (action, extensions)
    {
        let result = super::dispatch_registered_extension_with_arguments_for_turn(
            session,
            task_id,
            turn_id,
            extensions.registry,
            extensions.context,
            name,
            arguments,
        );
        return match result {
            Ok(result) => Ok(ActionExecution::Observation(
                super::WorkspaceObservation::ExtensionCompleted {
                    name: name.clone(),
                    result,
                },
            )),
            Err(_) => {
                execute_action_with_revision(session, task_id, turn_id, action, expected_revision)
            }
        };
    }
    execute_action_with_revision(session, task_id, turn_id, action, expected_revision)
}

/// Runs one decision-selected action for an already running, task-owned turn.
///
/// This preserves the original one-action API. Use [`run_goal_turn_with_budget`] when the
/// decision source should receive prior observations and the caller needs to set an action cap.
/// A rejected directory-creation action returns a failed result with its observation; other
/// pre-existing action errors keep their existing `Err` behavior.
pub fn run_goal_turn<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_extensions(session, task_id, turn_id, goal, decision_source, None)
}

/// Run the existing single-action turn with optional host-registered tools.
pub fn run_goal_turn_with_extensions<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
    extensions: Option<&TurnExtensions<'_>>,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(GoalTurnError::WorkspaceRead)?;

    let action = decision_source.choose_action(goal);
    match execute_with_extensions(session, task_id, turn_id, &action, extensions, None, None)? {
        ActionExecution::Complete(result) => finish_turn(
            session,
            task_id,
            turn_id,
            Vec::new(),
            TurnCompletionData {
                activity: vec![TurnActivitySummary::Completed],
                actions_taken: 1,
                stop_reason: GoalTurnStopReason::Completed,
                outcome: TurnOutcome::Success { result },
            },
        ),
        ActionExecution::Continue => finish_turn(
            session,
            task_id,
            turn_id,
            Vec::new(),
            TurnCompletionData {
                activity: vec![TurnActivitySummary::Continued],
                actions_taken: 1,
                stop_reason: GoalTurnStopReason::Continued,
                outcome: TurnOutcome::Success {
                    result: "continued".to_owned(),
                },
            },
        ),
        ActionExecution::Cancel => {
            cancel_turn(session, task_id, turn_id, Vec::new(), Vec::new(), 1)
        }
        ActionExecution::Observation(observation) => {
            let result = match &observation {
                WorkspaceObservation::ProcessFinished { result } => {
                    format!("offline Cargo {:?}", result.outcome)
                }
                WorkspaceObservation::FileRead { contents, .. } => contents.clone(),
                WorkspaceObservation::FileRangeRead { excerpt, .. } => excerpt.clone(),
                WorkspaceObservation::DirectoryListed { entries, .. } => {
                    format!("listed {} entries", entries.len())
                }
                WorkspaceObservation::DirectoryCreated { .. } => "created directory".to_owned(),
                WorkspaceObservation::TextSearched { locations, .. } => {
                    format!("found {} matching locations", locations.len())
                }
                WorkspaceObservation::TreeTextSearched { locations, .. } => {
                    format!("found {} matching locations", locations.len())
                }
                WorkspaceObservation::FileCreated { bytes_written, .. } => {
                    format!("created file ({bytes_written} bytes)")
                }
                WorkspaceObservation::TextReplaced { bytes_written, .. } => {
                    format!("replaced text ({bytes_written} bytes)")
                }
                WorkspaceObservation::PatchApplied { bytes_written, .. } => {
                    format!("replaced text ({bytes_written} bytes)")
                }
                WorkspaceObservation::ExtensionCompleted { result, .. } => result.clone(),
                WorkspaceObservation::FileCreationFailed { .. }
                | WorkspaceObservation::DirectoryCreationFailed { .. }
                | WorkspaceObservation::TextReplacementFailed { .. }
                | WorkspaceObservation::PatchFailed { .. }
                | WorkspaceObservation::TextSearchFailed { .. }
                | WorkspaceObservation::TreeTextSearchFailed { .. }
                | WorkspaceObservation::ActionFailed { .. } => String::new(),
            };
            let activity = summarize_observation_activity(&observation);
            finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                TurnCompletionData {
                    activity,
                    actions_taken: 1,
                    stop_reason: GoalTurnStopReason::ActionCompleted,
                    outcome: TurnOutcome::Success { result },
                },
            )
        }
        ActionExecution::Failed { observation, error } => {
            let detail = error.to_string();
            let activity = summarize_observation_activity(&observation);
            let returns_failed_result = matches!(
                &observation,
                WorkspaceObservation::DirectoryCreationFailed { .. }
            );
            let result = finish_turn(
                session,
                task_id,
                turn_id,
                vec![observation],
                TurnCompletionData {
                    activity,
                    actions_taken: 1,
                    stop_reason: GoalTurnStopReason::ActionFailed,
                    outcome: TurnOutcome::Failure { detail },
                },
            )?;
            if returns_failed_result {
                Ok(result)
            } else {
                Err(error)
            }
        }
    }
}

/// Runs up to `max_actions` decisions for one already running, task-owned turn.
///
/// Each decision receives the original goal and every earlier observation in order. The action
/// budget counts decisions, including explicit completion. Workspace read/list/search failures
/// are returned in the observations and recorded as the turn's one failure outcome. Ownership
/// and turn-state errors return `Err` without invoking the decision source or changing lifecycle
/// state.
pub fn run_goal_turn_with_budget<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_budget_and_extensions(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
        None,
    )
}

/// Run the bounded turn through the same action dispatcher with host tools.
pub fn run_goal_turn_with_budget_and_extensions<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
    extensions: Option<&TurnExtensions<'_>>,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_budget_and_gate(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
        extensions,
        None,
    )
}

/// Runs a bounded turn with a host-owned permission gate immediately before dispatch.
///
/// Existing turn APIs remain compatibility wrappers without a permission policy. Policy-aware
/// callers must provide this gate for every planned, verified, or correction execution path.
pub(crate) fn run_goal_turn_with_budget_and_permission_gate<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
    permission_gate: &mut ActionPermissionGate<'_>,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_budget_and_gate(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
        None,
        Some(permission_gate),
    )
}

#[allow(clippy::too_many_arguments)] // Preserves the execution API while adding host-owned extensions and permission.
pub(crate) fn run_goal_turn_with_budget_and_extensions_and_gate<
    D: BoundedDecisionSource + ?Sized,
>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
    extensions: Option<&TurnExtensions<'_>>,
    permission_gate: Option<&mut ActionPermissionGate<'_>>,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_budget_and_gate(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
        extensions,
        permission_gate,
    )
}

#[allow(clippy::too_many_arguments)] // Keeps the shared execution inputs visible at the dispatch boundary.
fn run_goal_turn_with_budget_and_gate<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
    extensions: Option<&TurnExtensions<'_>>,
    mut permission_gate: Option<&mut ActionPermissionGate<'_>>,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(GoalTurnError::WorkspaceRead)?;

    let mut observations = Vec::new();
    let mut activity = Vec::new();
    let mut actions_taken = 0;
    let needs_root_project_guidance = decision_source.needs_root_project_guidance();
    let root_project_guidance = if needs_root_project_guidance && max_actions > 0 {
        session
            .read_root_project_guidance(task_id)
            .map_err(GoalTurnError::RootProjectGuidance)?
    } else {
        None
    };
    while actions_taken < max_actions {
        let mut decision_guidance = root_project_guidance.clone();
        let action = loop {
            let action = if needs_root_project_guidance {
                decision_source.choose_action_with_root_project_guidance(
                    goal,
                    &observations,
                    decision_guidance.as_deref(),
                )
            } else {
                decision_source.choose_action(goal, &observations)
            };
            actions_taken += 1;

            if needs_root_project_guidance {
                if let Some(target) = workspace_mutation_target(&action) {
                    let applicable_guidance = match session
                        .read_project_guidance_for_targets(task_id, std::slice::from_ref(target))
                    {
                        Ok(guidance) => guidance,
                        Err(
                            crate::task_lifecycle::WorkspaceGuidanceReadError::InvalidTargetPath,
                        ) => break action,
                        Err(error) => {
                            return Err(GoalTurnError::RootProjectGuidance(error));
                        }
                    };
                    if applicable_guidance != decision_guidance {
                        if actions_taken == max_actions {
                            activity.push(TurnActivitySummary::BudgetExhausted {
                                actions: actions_taken,
                            });
                            return finish_turn(
                                session,
                                task_id,
                                turn_id,
                                observations,
                                TurnCompletionData {
                                    activity,
                                    actions_taken,
                                    stop_reason: GoalTurnStopReason::ActionBudgetExhausted,
                                    outcome: TurnOutcome::Failure {
                                        detail: format!(
                                            "action budget exhausted after {actions_taken} actions"
                                        ),
                                    },
                                },
                            );
                        }
                        decision_guidance = applicable_guidance;
                        continue;
                    }
                }
            }
            break action;
        };
        let expected_revision = decision_source.expected_file_revision(&action);
        match execute_with_extensions(
            session,
            task_id,
            turn_id,
            &action,
            extensions,
            expected_revision.as_deref(),
            permission_gate.as_deref_mut(),
        )? {
            ActionExecution::Complete(result) => {
                activity.push(TurnActivitySummary::Completed);
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::Completed,
                        outcome: TurnOutcome::Success { result },
                    },
                );
            }
            ActionExecution::Continue => {
                activity.push(TurnActivitySummary::Continued);
                if actions_taken == max_actions {
                    activity.push(TurnActivitySummary::BudgetExhausted {
                        actions: actions_taken,
                    });
                    return finish_turn(
                        session,
                        task_id,
                        turn_id,
                        observations,
                        TurnCompletionData {
                            activity,
                            actions_taken,
                            stop_reason: GoalTurnStopReason::ActionBudgetExhausted,
                            outcome: TurnOutcome::Failure {
                                detail: format!(
                                    "action budget exhausted after {actions_taken} actions"
                                ),
                            },
                        },
                    );
                }
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::Continued,
                        outcome: TurnOutcome::Success {
                            result: "continued".to_owned(),
                        },
                    },
                );
            }
            ActionExecution::Cancel => {
                return cancel_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    activity,
                    actions_taken,
                );
            }
            ActionExecution::Observation(observation) => {
                activity.extend(summarize_observation_activity(&observation));
                observations.push(observation);
            }
            ActionExecution::Failed { observation, error } => {
                let detail = error.to_string();
                activity.extend(summarize_observation_activity(&observation));
                observations.push(observation);
                return finish_turn(
                    session,
                    task_id,
                    turn_id,
                    observations,
                    TurnCompletionData {
                        activity,
                        actions_taken,
                        stop_reason: GoalTurnStopReason::ActionFailed,
                        outcome: TurnOutcome::Failure { detail },
                    },
                );
            }
        }
    }

    activity.push(TurnActivitySummary::BudgetExhausted {
        actions: actions_taken,
    });
    finish_turn(
        session,
        task_id,
        turn_id,
        observations,
        TurnCompletionData {
            activity,
            actions_taken,
            stop_reason: GoalTurnStopReason::ActionBudgetExhausted,
            outcome: TurnOutcome::Failure {
                detail: format!("action budget exhausted after {actions_taken} actions"),
            },
        },
    )
}

pub(super) fn cancel_turn(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    observations: Vec<WorkspaceObservation>,
    mut activity: Vec<TurnActivitySummary>,
    actions_taken: usize,
) -> Result<GoalTurnResult, GoalTurnError> {
    activity.push(TurnActivitySummary::Cancelled);
    session
        .cancel_turn_with_activity(task_id, turn_id, &activity)
        .map_err(GoalTurnError::Lifecycle)?;
    let read_result = observations
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::ProcessFinished { .. } => None,
            WorkspaceObservation::FileRead { contents, .. } => Some(contents.clone()),
            WorkspaceObservation::FileRangeRead { excerpt, .. } => Some(excerpt.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        observations,
        actions_taken,
        stop_reason: GoalTurnStopReason::Cancelled,
        outcome: None,
    })
}

pub(super) fn finish_turn(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    observations: Vec<WorkspaceObservation>,
    completion: TurnCompletionData,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .complete_turn_with_activity(
            task_id,
            turn_id,
            completion.outcome.clone(),
            &completion.activity,
        )
        .map_err(GoalTurnError::Lifecycle)?;
    let read_result = observations
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::ProcessFinished { .. } => None,
            WorkspaceObservation::FileRead { contents, .. } => Some(contents.clone()),
            WorkspaceObservation::FileRangeRead { excerpt, .. } => Some(excerpt.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Ok(GoalTurnResult {
        task_id,
        turn_id,
        read_result,
        observations,
        actions_taken: completion.actions_taken,
        stop_reason: completion.stop_reason,
        outcome: Some(completion.outcome),
    })
}

#[cfg(test)]
#[path = "turn/extension_tests.rs"]
mod extension_tests;

#[cfg(test)]
#[path = "turn/permission_tests.rs"]
mod permission_tests;
