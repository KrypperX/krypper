//! The bounded goal-turn loop that drives decisions, actions and turn completion.

use super::action::execute_action_with_revision;
use super::summarize_observation_activity;
use super::types::{ActionExecution, TurnCompletionData};
use crate::contracts::goal_turn::{
    workspace_mutation_target, BoundedDecisionSource, DecisionSource, GoalTurnError,
    GoalTurnResult, GoalTurnStopReason, WorkspaceObservation,
};
use crate::task_lifecycle::{TaskSession, TurnActivitySummary, TurnOutcome};
use crate::tools::permission_policy::{
    classify_registered_operation, decide, EffectClass, PermissionDecision, PermissionMode,
};
use std::any::Any;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const OFFLINE_PROCESS_APPROVAL_LIFETIME: Duration = Duration::from_secs(60);

fn process_result_summary(result: &crate::ProcessResult) -> String {
    match result.profile {
        crate::contracts::process_execution::ProcessExecutionProfile::OfflineCargoTest => {
            format!("offline Cargo {:?}", result.outcome)
        }
        crate::contracts::process_execution::ProcessExecutionProfile::OfflineReadOnlyExecV1 => {
            format!("offline read-only process {:?}", result.outcome)
        }
        crate::contracts::process_execution::ProcessExecutionProfile::FullAccessHostProcessV1 => {
            format!("Full Access host process {:?}", result.outcome)
        }
    }
}

/// Synchronous host confirmation for one exact task action. The action and both IDs are
/// borrowed only for this dispatch attempt; a successful answer is never retained or reused.
pub(crate) type PermissionPrompt<'a> =
    dyn FnMut(Uuid, Uuid, &super::WorkspaceAction) -> Result<bool, ()> + 'a;

/// Installs a host-owned interrupt handler for one exact offline process grant and returns an
/// opaque RAII guard that remains alive only until that action dispatch finishes.
pub(crate) type ProcessInterruptRegistrar<'a> =
    dyn FnMut(Arc<AtomicBool>) -> io::Result<Box<dyn Any>> + 'a;

/// Host-owned per-turn action gate. The process capability flag describes whether the embedding
/// has provisioned the inputs used by the offline profiles; TaskSession remains the final grant
/// authority for each process request.
pub(crate) struct ActionPermissionGate<'a> {
    mode: PermissionMode,
    offline_process_available: bool,
    offline_policy: Option<&'a crate::OfflineCargoPolicy>,
    prompt: Option<&'a mut PermissionPrompt<'a>>,
    interrupt_registrar: Option<&'a mut ProcessInterruptRegistrar<'a>>,
}

impl<'a> ActionPermissionGate<'a> {
    pub(crate) fn new(
        mode: PermissionMode,
        offline_process_available: bool,
        prompt: Option<&'a mut PermissionPrompt<'a>>,
    ) -> Self {
        Self {
            mode,
            offline_process_available,
            offline_policy: None,
            prompt,
            interrupt_registrar: None,
        }
    }

    /// Build the CLI gate around a captured host policy. It forks that immutable template into a
    /// new one-shot grant only after each process action passes this gate.
    pub(crate) fn with_offline_policy(
        mode: PermissionMode,
        offline_policy: &'a crate::OfflineCargoPolicy,
        prompt: Option<&'a mut PermissionPrompt<'a>>,
    ) -> Self {
        Self {
            mode,
            offline_process_available: true,
            offline_policy: Some(offline_policy),
            prompt,
            interrupt_registrar: None,
        }
    }

    /// Attach the CLI's scoped signal handler. Embedders can continue to manage cancellation
    /// through `TaskSession` directly without installing process-global signal handlers.
    pub(crate) fn with_process_interrupt_registrar(
        mut self,
        registrar: &'a mut ProcessInterruptRegistrar<'a>,
    ) -> Self {
        self.interrupt_registrar = Some(registrar);
        self
    }

    fn authorize(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        action: &super::WorkspaceAction,
        _extensions: Option<&TurnExtensions<'_>>,
    ) -> Result<(), GoalTurnError> {
        self.authorize_decision(task_id, turn_id, action)
            .map(|_| ())
    }

    fn authorize_for_dispatch(
        &mut self,
        session: &TaskSession,
        task_id: Uuid,
        turn_id: Uuid,
        action: &super::WorkspaceAction,
        extensions: Option<&TurnExtensions<'_>>,
    ) -> Result<Option<Box<dyn Any>>, GoalTurnError> {
        if let super::WorkspaceAction::FullAccessHostProcess { .. } = action {
            if self.mode != PermissionMode::FullAccess
                || !session.task_has_confirmed_full_access(task_id)
            {
                return Err(GoalTurnError::PermissionUnavailable {
                    action: "full_access_host_process",
                });
            }
            return self
                .authorize(task_id, turn_id, action, extensions)
                .map(|()| None);
        }
        match action {
            super::WorkspaceAction::OfflineReadOnlyExec { request } => {
                let host_approved = self.authorize_decision(task_id, turn_id, action)?;
                if !host_approved {
                    return Err(GoalTurnError::PermissionUnavailable {
                        action: "offline_read_only_exec",
                    });
                }
                let authorization = if let Some(policy) = self.offline_policy {
                    let policy = policy.fork_for_new_grant().map_err(|_| {
                        GoalTurnError::PermissionUnavailable {
                            action: "offline_read_only_exec",
                        }
                    })?;
                    session.authorize_offline_read_only_exec_once(
                        task_id,
                        policy,
                        request.clone(),
                        OFFLINE_PROCESS_APPROVAL_LIFETIME,
                    )
                } else {
                    session.authorize_offline_read_only_exec_once_from_current_policy(
                        task_id,
                        request.clone(),
                        OFFLINE_PROCESS_APPROVAL_LIFETIME,
                    )
                };
                authorization.map_err(|_| GoalTurnError::PermissionUnavailable {
                    action: "offline_read_only_exec",
                })?;
                self.register_interrupt_for_grant(session, task_id, "offline_read_only_exec")
            }
            super::WorkspaceAction::OfflineCargoTest { test_filter } => {
                self.authorize_decision(task_id, turn_id, action)?;
                if let Some(policy) = self.offline_policy {
                    let policy = policy.fork_for_new_grant().map_err(|_| {
                        GoalTurnError::PermissionUnavailable {
                            action: "offline_cargo_test",
                        }
                    })?;
                    session
                        .authorize_offline_cargo_test_once(
                            task_id,
                            policy,
                            test_filter.as_deref(),
                            OFFLINE_PROCESS_APPROVAL_LIFETIME,
                        )
                        .map_err(|_| GoalTurnError::PermissionUnavailable {
                            action: "offline_cargo_test",
                        })?;
                }
                self.register_interrupt_for_grant(session, task_id, "offline_cargo_test")
            }
            _ => self
                .authorize(task_id, turn_id, action, extensions)
                .map(|()| None),
        }
    }

    fn register_interrupt_for_grant(
        &mut self,
        session: &TaskSession,
        task_id: Uuid,
        action: &'static str,
    ) -> Result<Option<Box<dyn Any>>, GoalTurnError> {
        let Some(registrar) = self.interrupt_registrar.as_mut() else {
            return Ok(None);
        };
        let Some(cancellation) = session.offline_process_grant_cancellation(task_id) else {
            let _ = session.revoke_offline_cargo_tests(task_id);
            return Err(GoalTurnError::PermissionUnavailable { action });
        };
        match registrar(Arc::clone(&cancellation)) {
            Ok(registration) => Ok(Some(registration)),
            Err(_) => {
                cancellation.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = session.revoke_offline_cargo_tests(task_id);
                Err(GoalTurnError::PermissionUnavailable { action })
            }
        }
    }

    fn authorize_decision(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        action: &super::WorkspaceAction,
    ) -> Result<bool, GoalTurnError> {
        use super::WorkspaceAction;

        // Unsupported actions are inert error reports handled by the dispatcher. Let them reach
        // that path so malformed input and provider failures retain their normal turn outcome.
        if matches!(action, WorkspaceAction::Unsupported { .. }) {
            return Ok(false);
        }

        let action_name = super::workspace_action_name(action);
        let effect = match action {
            WorkspaceAction::Complete { .. }
            | WorkspaceAction::Continue
            | WorkspaceAction::Cancel => EffectClass::TaskControl,
            WorkspaceAction::Extension { .. } => {
                // A generic action prompt does not expose the exact extension intent.
                return Err(GoalTurnError::PermissionPromptUnavailable {
                    action: "registered extension",
                });
            }
            WorkspaceAction::Unsupported { .. } => EffectClass::Unsupported,
            // The registry is the only source of effect classifications for executable tools.
            // An action without registered metadata is treated as risky and prompts in Ask or
            // fails closed when no trusted prompt channel exists.
            _ => classify_registered_operation(action_name),
        };

        if (effect == EffectClass::FixedOfflineProcess
            || matches!(action, WorkspaceAction::OfflineReadOnlyExec { .. }))
            && !self.offline_process_available
        {
            return Err(GoalTurnError::PermissionUnavailable {
                action: action_name,
            });
        }

        match decide(self.mode, effect) {
            PermissionDecision::Allow => Ok(false),
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
                    Ok(true) => Ok(true),
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

#[allow(clippy::too_many_arguments)] // Keeps both authorization gates visible at the shared dispatch boundary.
fn execute_with_extensions(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    action: &super::WorkspaceAction,
    extensions: Option<&TurnExtensions<'_>>,
    expected_revision: Option<&str>,
    permission_gate: Option<&mut ActionPermissionGate<'_>>,
    extension_approval: Option<&mut super::ExtensionApprovalGate<'_>>,
) -> Result<ActionExecution, GoalTurnError> {
    if let (super::WorkspaceAction::Extension { name, arguments }, Some(extensions)) =
        (action, extensions)
    {
        let result = if let Some(approval) = extension_approval {
            super::dispatch_registered_extension_with_approval_for_turn(
                session,
                task_id,
                turn_id,
                extensions.registry,
                extensions.context,
                approval,
                name,
                arguments,
            )
            .map_err(extension_dispatch_error_to_goal_turn)?
        } else {
            return Err(GoalTurnError::PermissionPromptUnavailable {
                action: "registered extension",
            });
        };
        return Ok(ActionExecution::Observation(
            super::WorkspaceObservation::ExtensionCompleted {
                name: name.clone(),
                result,
            },
        ));
    }
    let _process_interrupt_registration = if let Some(permission_gate) = permission_gate {
        permission_gate.authorize_for_dispatch(session, task_id, turn_id, action, extensions)?
    } else {
        None
    };
    execute_action_with_revision(session, task_id, turn_id, action, expected_revision)
}

fn extension_dispatch_error_to_goal_turn(error: super::ExtensionDispatchError) -> GoalTurnError {
    match error {
        super::ExtensionDispatchError::Turn(error) => GoalTurnError::WorkspaceRead(error),
        super::ExtensionDispatchError::ApprovalPromptFailed => {
            GoalTurnError::PermissionPromptFailed {
                action: "registered extension",
            }
        }
        super::ExtensionDispatchError::ApprovalRequired => {
            GoalTurnError::PermissionPromptUnavailable {
                action: "registered extension",
            }
        }
        super::ExtensionDispatchError::AuthorizationUnavailable
        | super::ExtensionDispatchError::InvalidIntent => GoalTurnError::PermissionUnavailable {
            action: "registered extension",
        },
        super::ExtensionDispatchError::TaskMismatch
        | super::ExtensionDispatchError::Bridge(_)
        | super::ExtensionDispatchError::ApprovalDenied
        | super::ExtensionDispatchError::StaleAuthorization
        | super::ExtensionDispatchError::StaleWorkspace
        | super::ExtensionDispatchError::ExpiredApproval
        | super::ExtensionDispatchError::InvalidApproval => GoalTurnError::PermissionDenied {
            action: "registered extension",
        },
    }
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
    run_goal_turn_with_extensions_and_approval(
        session,
        task_id,
        turn_id,
        goal,
        decision_source,
        extensions,
        None,
    )
}

/// Run the existing single-action turn with host-approved registered effects.
pub fn run_goal_turn_with_approved_extensions<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
    extensions: &TurnExtensions<'_>,
    approval: &mut super::ExtensionApprovalGate<'_>,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_extensions_and_approval(
        session,
        task_id,
        turn_id,
        goal,
        decision_source,
        Some(extensions),
        Some(approval),
    )
}

fn run_goal_turn_with_extensions_and_approval<D: DecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    decision_source: &mut D,
    extensions: Option<&TurnExtensions<'_>>,
    extension_approval: Option<&mut super::ExtensionApprovalGate<'_>>,
) -> Result<GoalTurnResult, GoalTurnError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(GoalTurnError::WorkspaceRead)?;

    if session.task_cancellation_requested(task_id) {
        return cancel_turn(session, task_id, turn_id, Vec::new(), Vec::new(), 0);
    }
    let action = decision_source.choose_action(goal);
    if session.task_cancellation_requested(task_id) {
        return cancel_turn(session, task_id, turn_id, Vec::new(), Vec::new(), 1);
    }
    match execute_with_extensions(
        session,
        task_id,
        turn_id,
        &action,
        extensions,
        None,
        None,
        extension_approval,
    )? {
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
            if session.task_cancellation_requested(task_id) {
                return cancel_turn(session, task_id, turn_id, vec![observation], Vec::new(), 1);
            }
            let result = match &observation {
                WorkspaceObservation::ProcessFinished { result } => process_result_summary(result),
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
            if session.task_cancellation_requested(task_id) {
                let activity = summarize_observation_activity(&observation);
                return cancel_turn(session, task_id, turn_id, vec![observation], activity, 1);
            }
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
        None,
    )
}

/// Run a bounded turn with host-approved registered effects.
#[allow(clippy::too_many_arguments)] // Mirrors the existing bounded-turn inputs while adding the approval gate.
pub fn run_goal_turn_with_budget_and_approved_extensions<D: BoundedDecisionSource + ?Sized>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    goal: &str,
    max_actions: usize,
    decision_source: &mut D,
    extensions: &TurnExtensions<'_>,
    approval: &mut super::ExtensionApprovalGate<'_>,
) -> Result<GoalTurnResult, GoalTurnError> {
    run_goal_turn_with_budget_and_gate(
        session,
        task_id,
        turn_id,
        goal,
        max_actions,
        decision_source,
        Some(extensions),
        None,
        Some(approval),
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
        None,
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
        None,
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
    mut extension_approval: Option<&mut super::ExtensionApprovalGate<'_>>,
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
        if session.task_cancellation_requested(task_id) {
            return cancel_turn(
                session,
                task_id,
                turn_id,
                observations,
                activity,
                actions_taken,
            );
        }
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
        if session.task_cancellation_requested(task_id) {
            return cancel_turn(
                session,
                task_id,
                turn_id,
                observations,
                activity,
                actions_taken,
            );
        }
        let expected_revision = decision_source.expected_file_revision(&action);
        match execute_with_extensions(
            session,
            task_id,
            turn_id,
            &action,
            extensions,
            expected_revision.as_deref(),
            permission_gate.as_deref_mut(),
            extension_approval.as_deref_mut(),
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
                if session.task_cancellation_requested(task_id) {
                    return cancel_turn(
                        session,
                        task_id,
                        turn_id,
                        observations,
                        activity,
                        actions_taken,
                    );
                }
            }
            ActionExecution::Failed { observation, error } => {
                let detail = error.to_string();
                activity.extend(summarize_observation_activity(&observation));
                observations.push(observation);
                if session.task_cancellation_requested(task_id) {
                    return cancel_turn(
                        session,
                        task_id,
                        turn_id,
                        observations,
                        activity,
                        actions_taken,
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

#[cfg(test)]
mod process_result_summary_tests {
    use super::process_result_summary;
    use crate::contracts::process_execution::{
        ProcessExecutionProfile, ProcessOutcome, ProcessResult,
    };

    #[test]
    fn process_summaries_name_the_profile_that_produced_the_result() {
        let cargo = ProcessResult::empty(ProcessOutcome::Exited(0));
        assert_eq!(process_result_summary(&cargo), "offline Cargo Exited(0)");

        let mut read_only = ProcessResult::empty(ProcessOutcome::Exited(0));
        read_only.profile = ProcessExecutionProfile::OfflineReadOnlyExecV1;
        assert_eq!(
            process_result_summary(&read_only),
            "offline read-only process Exited(0)"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod offline_read_only_exec_approval_tests {
    use super::{run_goal_turn_with_budget_and_permission_gate, ActionPermissionGate};
    use crate::contracts::goal_turn::{GoalTurnError, WorkspaceAction, WorkspaceObservation};
    use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
    use crate::tools::permission_policy::PermissionMode;
    use crate::tools::process::{OfflineReadOnlyExecRequest, SandboxExecutable};
    use crate::{OfflineCargoPolicy, ProcessExecutionProfile, ProcessOutcome, ProcessResult};
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use uuid::Uuid;

    struct TestInterruptRegistration(signal_hook::SigId);

    impl Drop for TestInterruptRegistration {
        fn drop(&mut self) {
            signal_hook::low_level::unregister(self.0);
        }
    }

    fn running_turn(workspace: &Path) -> (TaskSession, Uuid, Uuid) {
        let mut session = TaskSession::new_in_workspace(workspace).expect("workspace session");
        let task_id = session.create_task("direct argv approval").id();
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("running task");
        let turn_id = task.create_turn().expect("turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("running turn");
        (session, task_id, turn_id)
    }

    fn policy(toolchain: &Path) -> OfflineCargoPolicy {
        fs::create_dir_all(toolchain.join("bin")).expect("toolchain bin");
        fs::write(toolchain.join("bin/cargo"), b"\x7fELF").expect("fake Cargo identity");
        fs::write(toolchain.join("bin/rustc"), b"\x7fELF").expect("fake rustc identity");
        OfflineCargoPolicy::new(toolchain, toolchain).expect("host process policy")
    }

    fn request() -> OfflineReadOnlyExecRequest {
        OfflineReadOnlyExecRequest::new(
            SandboxExecutable::Cargo,
            vec!["metadata".into(), "--offline".into()],
            ".",
        )
        .expect("bounded direct argv request")
    }

    #[cfg(unix)]
    #[test]
    fn interrupt_registration_targets_only_the_current_grant_and_is_scoped() {
        let workspace = crate::test_support::TempDir::with_prefix("direct-exec-cancel-workspace-");
        let toolchain = crate::test_support::TempDir::with_prefix("direct-exec-cancel-toolchain-");
        let (session, task_id, turn_id) = running_turn(workspace.path());
        session
            .set_task_permission_mode(task_id, PermissionMode::Ask, false)
            .expect("permission mode");
        let task_cancellation = session
            .task_cancellation_flag(task_id)
            .expect("task cancellation flag");
        let policy = policy(toolchain.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut prompt = |_, _, _: &WorkspaceAction| Ok(true);
        let registered_cancellation = std::cell::RefCell::new(None);
        let mut registrar = |cancellation: Arc<AtomicBool>| {
            signal_hook::flag::register(signal_hook::consts::SIGUSR2, Arc::clone(&cancellation))
                .map(|registration| {
                    registered_cancellation.replace(Some(Arc::clone(&cancellation)));
                    Box::new(TestInterruptRegistration(registration)) as Box<dyn std::any::Any>
                })
        };
        let mut gate = ActionPermissionGate::with_offline_policy(
            PermissionMode::Ask,
            &policy,
            Some(&mut prompt),
        )
        .with_process_interrupt_registrar(&mut registrar);

        let registration = gate
            .authorize_for_dispatch(&session, task_id, turn_id, &action, None)
            .expect("offline action authorized")
            .expect("interrupt registration");
        let grant_cancellation = session
            .offline_process_grant_cancellation(task_id)
            .expect("current grant cancellation flag");
        let registered_cancellation = registered_cancellation
            .borrow()
            .as_ref()
            .cloned()
            .expect("registered cancellation flag");
        assert!(Arc::ptr_eq(&grant_cancellation, &registered_cancellation));
        assert!(!Arc::ptr_eq(&grant_cancellation, &task_cancellation));

        // SAFETY: this test registered SIGUSR2 above; the handler only sets the grant flag.
        unsafe {
            libc::raise(signal_hook::consts::SIGUSR2);
        }
        assert!(grant_cancellation.load(Ordering::SeqCst));
        assert!(!task_cancellation.load(Ordering::SeqCst));

        drop(registration);
        grant_cancellation.store(false, Ordering::SeqCst);
        // SAFETY: the scoped registration was dropped, so SIGUSR2 has no grant handler now.
        unsafe {
            libc::raise(signal_hook::consts::SIGUSR2);
        }
        assert!(!grant_cancellation.load(Ordering::SeqCst));
    }

    #[test]
    fn interrupt_registration_failure_revokes_grant_before_process_dispatch() {
        let workspace = crate::test_support::TempDir::with_prefix("direct-exec-no-hook-workspace-");
        let toolchain = crate::test_support::TempDir::with_prefix("direct-exec-no-hook-toolchain-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        session.queue_offline_cargo_results_for_test(
            task_id,
            [ProcessResult::empty(ProcessOutcome::Exited(0))],
        );
        let policy = policy(toolchain.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let mut prompt = |_, _, _: &WorkspaceAction| Ok(true);
        let mut registrar = |_cancellation: Arc<AtomicBool>| {
            Err(std::io::Error::other("signal registration unavailable"))
        };
        let mut gate = ActionPermissionGate::with_offline_policy(
            PermissionMode::Ask,
            &policy,
            Some(&mut prompt),
        )
        .with_process_interrupt_registrar(&mut registrar);

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionUnavailable {
                action: "offline_read_only_exec"
            })
        ));
        assert!(session.offline_cargo_filters_for_test(task_id).is_empty());
        assert!(session
            .offline_process_grant_cancellation(task_id)
            .is_none());
    }

    #[test]
    fn positive_host_prompt_installs_exact_one_shot_before_harness_dispatch() {
        let workspace = crate::test_support::TempDir::with_prefix("harness-direct-exec-workspace-");
        let toolchain = crate::test_support::TempDir::with_prefix("harness-direct-exec-policy-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        session
            .authorize_offline_cargo_tests(task_id, policy(toolchain.path()))
            .expect("host-installed policy");
        session.queue_offline_cargo_results_for_test(
            task_id,
            [ProcessResult::empty(ProcessOutcome::Exited(0))],
        );

        let approved_request = request();
        let action = WorkspaceAction::OfflineReadOnlyExec {
            request: approved_request.clone(),
        };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let mut prompt = |seen_task, seen_turn, seen_action: &WorkspaceAction| {
            assert_eq!(seen_task, task_id);
            assert_eq!(seen_turn, turn_id);
            assert_eq!(seen_action, &action);
            Ok(true)
        };
        let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        )
        .expect("approved request should dispatch through TaskSession");

        let Some(WorkspaceObservation::ProcessFinished { result }) = result.observations().first()
        else {
            panic!("process result observation expected");
        };
        assert_eq!(
            result.profile,
            ProcessExecutionProfile::OfflineReadOnlyExecV1
        );
        assert_eq!(result.outcome, ProcessOutcome::Exited(0));
        // The approved intent itself was carried through the action and gate unchanged.
        assert_eq!(approved_request.executable(), SandboxExecutable::Cargo);
    }

    #[test]
    fn positive_prompt_without_active_host_policy_fails_before_process_dispatch() {
        let workspace = crate::test_support::TempDir::with_prefix("harness-direct-exec-no-policy-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let mut prompt = |_, _, seen_action: &WorkspaceAction| {
            assert_eq!(seen_action, &action);
            Ok(true)
        };
        let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionUnavailable {
                action: "offline_read_only_exec"
            })
        ));
    }

    #[test]
    fn missing_host_prompt_fails_before_process_dispatch() {
        let workspace = crate::test_support::TempDir::with_prefix("harness-direct-exec-no-prompt-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, None);

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionPromptUnavailable {
                action: "offline_read_only_exec"
            })
        ));
    }

    #[test]
    fn missing_process_capability_fails_before_asking_for_direct_argv_approval() {
        let workspace = crate::test_support::TempDir::with_prefix("harness-direct-exec-disabled-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let prompt_calls = std::cell::Cell::new(0);
        let mut prompt = |_, _, _: &WorkspaceAction| {
            prompt_calls.set(prompt_calls.get() + 1);
            Ok(true)
        };
        let mut gate =
            ActionPermissionGate::new(PermissionMode::RiskBased, false, Some(&mut prompt));

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionUnavailable {
                action: "offline_read_only_exec"
            })
        ));
        assert_eq!(prompt_calls.get(), 0);
    }

    #[test]
    fn denied_host_prompt_never_installs_or_dispatches_a_process_grant() {
        let workspace = crate::test_support::TempDir::with_prefix("harness-direct-exec-denied-");
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        let action = WorkspaceAction::OfflineReadOnlyExec { request: request() };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let mut prompt = |_, _, seen_action: &WorkspaceAction| {
            assert_eq!(seen_action, &action);
            Ok(false)
        };
        let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "inspect metadata",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionDenied {
                action: "offline_read_only_exec"
            })
        ));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod offline_profile_permission_tests {
    use super::{run_goal_turn_with_budget_and_permission_gate, ActionPermissionGate};
    use crate::contracts::goal_turn::{GoalTurnError, WorkspaceAction, WorkspaceObservation};
    use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
    use crate::tools::permission_policy::PermissionMode;
    use crate::tools::process::{OfflineReadOnlyExecRequest, SandboxExecutable};
    use crate::{OfflineCargoPolicy, ProcessOutcome, ProcessResult};
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use uuid::Uuid;

    fn running_turn(workspace: &Path) -> (TaskSession, Uuid, Uuid) {
        let mut session = TaskSession::new_in_workspace(workspace).expect("workspace session");
        let task_id = session.create_task("offline profile permission gate").id();
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("running task");
        let turn_id = task.create_turn().expect("turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("running turn");
        (session, task_id, turn_id)
    }

    fn captured_policy() -> (crate::test_support::TempDir, OfflineCargoPolicy) {
        let toolchain = crate::test_support::TempDir::with_prefix("offline-profile-toolchain-");
        let registry = crate::test_support::TempDir::with_prefix("offline-profile-registry-");
        fs::create_dir_all(toolchain.path().join("bin")).expect("toolchain bin");
        for executable in ["cargo", "rustc"] {
            fs::write(toolchain.path().join("bin").join(executable), b"\x7fELF")
                .expect("fake ELF identity");
        }
        let policy = OfflineCargoPolicy::new(toolchain.path(), registry.path())
            .expect("captured offline process policy");
        policy
            .validate_roots_are_disjoint()
            .expect("separate trusted roots");
        (toolchain, policy)
    }

    fn direct_request() -> OfflineReadOnlyExecRequest {
        OfflineReadOnlyExecRequest::new(
            SandboxExecutable::Cargo,
            vec!["metadata".into(), "--offline".into()],
            ".",
        )
        .expect("bounded direct argv request")
    }

    #[test]
    fn direct_argv_requires_a_fresh_prompt_in_ask_and_risk_based_modes() {
        for mode in [PermissionMode::Ask, PermissionMode::RiskBased] {
            let workspace = crate::test_support::TempDir::with_prefix("offline-profile-workspace-");
            let (_toolchain, policy) = captured_policy();
            let (mut session, task_id, turn_id) = running_turn(workspace.path());
            session.queue_offline_cargo_results_for_test(
                task_id,
                [ProcessResult::empty(ProcessOutcome::Exited(0))],
            );
            let action = WorkspaceAction::OfflineReadOnlyExec {
                request: direct_request(),
            };
            let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
            let prompt_calls = Cell::new(0);
            let mut prompt = |_, _, seen_action: &WorkspaceAction| {
                assert_eq!(seen_action, &action);
                prompt_calls.set(prompt_calls.get() + 1);
                Ok(true)
            };
            let mut gate =
                ActionPermissionGate::with_offline_policy(mode, &policy, Some(&mut prompt));

            let result = run_goal_turn_with_budget_and_permission_gate(
                &mut session,
                task_id,
                turn_id,
                "inspect metadata",
                1,
                &mut decisions,
                &mut gate,
            )
            .expect("approved direct argv should receive a one-shot grant");

            assert_eq!(
                prompt_calls.get(),
                1,
                "{mode:?} must require exact approval"
            );
            assert!(matches!(
                result.observations().first(),
                Some(WorkspaceObservation::ProcessFinished { result })
                    if result.outcome == ProcessOutcome::Exited(0)
            ));
        }
    }

    #[test]
    fn fixed_cargo_preserves_ask_and_risk_based_modes_and_requires_a_new_grant_per_action() {
        for (mode, expected_prompt_calls) in
            [(PermissionMode::Ask, 1), (PermissionMode::RiskBased, 0)]
        {
            let workspace = crate::test_support::TempDir::with_prefix("offline-profile-workspace-");
            let (_toolchain, policy) = captured_policy();
            let (mut session, task_id, turn_id) = running_turn(workspace.path());
            session.queue_offline_cargo_results_for_test(
                task_id,
                [ProcessResult::empty(ProcessOutcome::Exited(0))],
            );
            let action = WorkspaceAction::OfflineCargoTest {
                test_filter: Some("selected_case".to_owned()),
            };
            let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
            let prompt_calls = Cell::new(0);
            let mut prompt = |_, _, seen_action: &WorkspaceAction| {
                assert_eq!(seen_action, &action);
                prompt_calls.set(prompt_calls.get() + 1);
                Ok(true)
            };
            let mut gate =
                ActionPermissionGate::with_offline_policy(mode, &policy, Some(&mut prompt));

            let result = run_goal_turn_with_budget_and_permission_gate(
                &mut session,
                task_id,
                turn_id,
                "run fixed cargo test",
                1,
                &mut decisions,
                &mut gate,
            )
            .expect("fixed Cargo keeps its existing mode semantics");

            assert_eq!(prompt_calls.get(), expected_prompt_calls);
            assert!(matches!(
                result.observations().first(),
                Some(WorkspaceObservation::ProcessFinished { result })
                    if result.outcome == ProcessOutcome::Exited(0)
            ));
            assert_eq!(
                session.offline_cargo_filters_for_test(task_id),
                vec![Some("selected_case".to_owned())]
            );
        }
    }

    #[test]
    fn fixed_cargo_requires_a_fresh_ask_approval_after_the_previous_one_shot_is_consumed() {
        let workspace = crate::test_support::TempDir::with_prefix("offline-profile-workspace-");
        let (_toolchain, policy) = captured_policy();
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        session.queue_offline_cargo_results_for_test(
            task_id,
            [ProcessResult::empty(ProcessOutcome::Exited(0))],
        );
        let action = WorkspaceAction::OfflineCargoTest {
            test_filter: Some("selected_case".to_owned()),
        };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let prompt_calls = Cell::new(0);
        let mut prompt = |_, _, seen_action: &WorkspaceAction| {
            assert_eq!(seen_action, &action);
            let call = prompt_calls.get() + 1;
            prompt_calls.set(call);
            Ok(call == 1)
        };
        let mut gate = ActionPermissionGate::with_offline_policy(
            PermissionMode::Ask,
            &policy,
            Some(&mut prompt),
        );

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "repeat fixed cargo action",
            2,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionDenied {
                action: "offline_cargo_test"
            })
        ));
        assert_eq!(prompt_calls.get(), 2);
        assert_eq!(
            session.offline_cargo_filters_for_test(task_id),
            vec![Some("selected_case".to_owned())],
            "the declined second action must not replay or dispatch"
        );
    }

    #[test]
    fn full_access_does_not_silently_authorize_direct_argv_profile_actions() {
        let workspace = crate::test_support::TempDir::with_prefix("offline-profile-workspace-");
        let (_toolchain, policy) = captured_policy();
        let (mut session, task_id, turn_id) = running_turn(workspace.path());
        let action = WorkspaceAction::OfflineReadOnlyExec {
            request: direct_request(),
        };
        let mut decisions = |_: &str, _: &[WorkspaceObservation]| action.clone();
        let prompt_calls = Cell::new(0);
        let mut prompt = |_, _, _: &WorkspaceAction| {
            prompt_calls.set(prompt_calls.get() + 1);
            Ok(true)
        };
        let mut gate = ActionPermissionGate::with_offline_policy(
            PermissionMode::FullAccess,
            &policy,
            Some(&mut prompt),
        );

        let result = run_goal_turn_with_budget_and_permission_gate(
            &mut session,
            task_id,
            turn_id,
            "run direct argv with Full Access",
            1,
            &mut decisions,
            &mut gate,
        );

        assert!(matches!(
            result,
            Err(GoalTurnError::PermissionUnavailable {
                action: "offline_read_only_exec"
            })
        ));
        assert_eq!(prompt_calls.get(), 0);
    }
}
