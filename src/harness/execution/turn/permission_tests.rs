use super::*;
use crate::brain::provider_bridge::ExtensionTool;
use crate::brain::WorkspacePlan;
use crate::brain::WorkspacePlanError;
use crate::harness::execution::{
    run_goal_turn_with_impact_checked_completion_correction_and_permission_gate,
    CompletionCorrectionError, GoalTurnError, RegisteredExtensionWithArguments,
};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use crate::tools::capability_policy::{CapabilityContext, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use crate::tools::permission_policy::PermissionMode;
use serde_json::{json, Value};
use std::cell::Cell;

fn running_turn() -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new();
    let task_id = session.create_task("permission gate").id();
    let turn_id = {
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("running task");
        let turn_id = task.create_turn().expect("turn");
        task.transition_turn(turn_id, TurnState::Running)
            .expect("running turn");
        turn_id
    };
    (session, task_id, turn_id)
}

fn extension_result(_: &Value) -> String {
    "extension-ran".to_owned()
}

fn checked_plan(
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    plan: WorkspacePlan,
) -> crate::harness::impact::ImpactCheckedPlan {
    let (report, expected_revisions) =
        crate::harness::impact::analyze_plan(session, task_id, turn_id, &plan)
            .expect("impact analysis");
    crate::harness::impact::ImpactCheckedPlan {
        plan,
        report,
        expected_revisions,
    }
}

fn extension_panics(_: &Value) -> String {
    panic!("permission denial must happen before the extension handler")
}

fn extension_registry<'a>(
    grant: &'a CapabilityGrant,
    execute: fn(&Value) -> String,
) -> [RegisteredExtensionWithArguments<'a, String>; 1] {
    [RegisteredExtensionWithArguments {
        tool: ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read through a host extension",
            parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            grant,
        },
        execute,
    }]
}

fn extension_context(task_id: Uuid) -> CapabilityContext {
    CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    }
}

#[test]
fn risk_based_allows_task_control_without_a_prompt() {
    let (mut session, task_id, turn_id) = running_turn();
    let action = super::super::WorkspaceAction::Complete {
        result: "done".to_owned(),
    };
    let mut decide = |_: &str, _: &[WorkspaceObservation]| action.clone();
    let mut gate = ActionPermissionGate::new(PermissionMode::RiskBased, false, None);

    let result = run_goal_turn_with_budget_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut decide,
        &mut gate,
    )
    .expect("bounded task control is allowed");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
}

#[test]
fn unsupported_actions_keep_the_existing_failure_reporting_path() {
    let (_session, task_id, turn_id) = running_turn();
    let action = super::super::WorkspaceAction::Unsupported {
        action: "invalid task action".to_owned(),
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::Ask, false, None);

    assert_eq!(gate.authorize(task_id, turn_id, &action, None), Ok(()));
}

#[test]
fn ask_and_risk_based_allow_existing_bounded_workspace_actions_without_prompting() {
    let (_session, task_id, turn_id) = running_turn();
    let action = super::super::WorkspaceAction::ReadFile {
        path: "bounded.txt".into(),
    };

    for mode in [PermissionMode::Ask, PermissionMode::RiskBased] {
        let mut gate = ActionPermissionGate::new(mode, false, None);
        assert_eq!(gate.authorize(task_id, turn_id, &action, None), Ok(()));
    }
}

#[test]
fn risk_based_can_request_only_the_fixed_process_profile_when_the_host_provisions_it() {
    let (_session, task_id, turn_id) = running_turn();
    let action = super::super::WorkspaceAction::OfflineCargoTest { test_filter: None };
    let mut gate = ActionPermissionGate::new(PermissionMode::RiskBased, true, None);

    assert_eq!(gate.authorize(task_id, turn_id, &action, None), Ok(()));
}

#[test]
fn extension_approval_is_synchronous_action_bound_and_consumed_per_dispatch() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".to_owned(),
        operation: "read".to_owned(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = extension_registry(&grant, extension_result);
    let extensions = TurnExtensions {
        registry: &registry,
        context: extension_context(task_id),
    };
    let extension_action = super::super::WorkspaceAction::Extension {
        name: "external_read".to_owned(),
        arguments: json!({}),
    };
    let mut action_number = 0;
    let mut decide = |_: &str, _: &[WorkspaceObservation]| {
        action_number += 1;
        if action_number <= 2 {
            extension_action.clone()
        } else {
            super::super::WorkspaceAction::Complete {
                result: "done".to_owned(),
            }
        }
    };
    let approvals = Cell::new(0);
    let mut current_context = || Ok(extensions.context);
    let mut prompt = |request: &super::super::ExtensionApprovalRequest| {
        approvals.set(approvals.get() + 1);
        assert_eq!(request.task_id, task_id);
        assert_eq!(request.turn_id, turn_id);
        assert_eq!(request.qualified_name, "external_read");
        assert_eq!(request.arguments, json!({}));
        Ok(true)
    };
    let mut gate = super::super::ExtensionApprovalGate::new(&mut current_context, &mut prompt);

    let result = super::super::run_goal_turn_with_budget_and_approved_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        3,
        &mut decide,
        &extensions,
        &mut gate,
    )
    .expect("each explicit action has a matching one-shot approval");

    assert_eq!(approvals.get(), 2);
    assert_eq!(
        result
            .observations()
            .iter()
            .filter(|observation| matches!(
                observation,
                WorkspaceObservation::ExtensionCompleted { .. }
            ))
            .count(),
        2
    );
}

#[test]
fn generic_action_prompt_cannot_approve_a_registered_extension() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".to_owned(),
        operation: "read".to_owned(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = extension_registry(&grant, extension_panics);
    let extensions = TurnExtensions {
        registry: &registry,
        context: extension_context(task_id),
    };
    let action = super::super::WorkspaceAction::Extension {
        name: "external_read".to_owned(),
        arguments: json!({}),
    };
    let mut decide = |_: &str, _: &[WorkspaceObservation]| action.clone();
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: Uuid, _: Uuid, _: &super::super::WorkspaceAction| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

    let result = run_goal_turn_with_budget_and_extensions_and_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut decide,
        Some(&extensions),
        Some(&mut gate),
    );

    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
    ));
    assert_eq!(prompt_calls.get(), 0);
}

#[test]
fn unregistered_extension_is_denied_without_prompting_or_dispatching() {
    let (mut session, task_id, turn_id) = running_turn();
    let extensions = TurnExtensions {
        registry: &[],
        context: extension_context(task_id),
    };
    let action = super::super::WorkspaceAction::Extension {
        name: "unknown_tool".to_owned(),
        arguments: json!({}),
    };
    let mut decide = |_: &str, _: &[WorkspaceObservation]| action.clone();
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: Uuid, _: Uuid, _: &super::super::WorkspaceAction| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

    let result = run_goal_turn_with_budget_and_extensions_and_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut decide,
        Some(&extensions),
        Some(&mut gate),
    );

    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
    ));
    assert_eq!(prompt_calls.get(), 0);
}

#[test]
fn unconfigured_fixed_process_is_unavailable_before_prompt_or_b4_dispatch() {
    let (mut session, task_id, turn_id) = running_turn();
    let action = super::super::WorkspaceAction::OfflineCargoTest { test_filter: None };
    let mut decide = |_: &str, _: &[WorkspaceObservation]| action.clone();
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: Uuid, _: Uuid, _: &super::super::WorkspaceAction| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::RiskBased, false, Some(&mut prompt));

    let result = run_goal_turn_with_budget_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut decide,
        &mut gate,
    );

    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionUnavailable {
            action: "offline_cargo_test"
        })
    ));
    assert_eq!(prompt_calls.get(), 0);
}

#[test]
fn unconfirmed_full_access_action_is_unavailable() {
    let (mut session, task_id, turn_id) = running_turn();
    let request = crate::tools::process::HostProcessRequest::new(
        "/bin/true".into(),
        Vec::new(),
        "/tmp".into(),
    )
    .expect("host process request");
    let action = super::super::WorkspaceAction::FullAccessHostProcess { request };
    let mut decide = |_: &str, _: &[WorkspaceObservation]| action.clone();
    let mut gate = ActionPermissionGate::new(PermissionMode::FullAccess, false, None);

    let result = run_goal_turn_with_budget_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut decide,
        &mut gate,
    );

    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionUnavailable {
            action: "full_access_host_process"
        })
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn full_access_cancellation_stops_the_turn_after_reaping_the_host_process() {
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir().expect("host fixture directory");
    let descendant_file = directory.path().join("descendant.pid");
    let script = format!("sleep 30 & echo $! > '{}'; wait", descendant_file.display());
    let request = crate::tools::process::HostProcessRequest::new(
        "/bin/sh".into(),
        vec!["-c".into(), script],
        directory.path().to_string_lossy().into_owned(),
    )
    .expect("direct host request");
    let (mut session, task_id, turn_id) = running_turn();
    session
        .set_task_permission_mode(task_id, PermissionMode::FullAccess, true)
        .expect("confirmed Full Access mode");
    let cancellation = session
        .task_cancellation_flag(task_id)
        .expect("task cancellation flag");
    let trigger_path = descendant_file.clone();
    let trigger = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !trigger_path.exists() {
            thread::sleep(Duration::from_millis(10));
        }
        cancellation.store(true, Ordering::SeqCst);
    });

    let action = crate::harness::execution::WorkspaceAction::FullAccessHostProcess { request };
    let mut decisions = 0;
    let mut decide = |_: &str, _: &[WorkspaceObservation]| {
        decisions += 1;
        action.clone()
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::FullAccess, false, None);
    let result = run_goal_turn_with_budget_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "cancel during host execution",
        3,
        &mut decide,
        &mut gate,
    )
    .expect("cancelled turn result");
    trigger.join().expect("cancellation trigger");

    assert_eq!(
        decisions, 1,
        "the model must not be asked for another action"
    );
    assert_eq!(
        result.stop_reason(),
        crate::contracts::goal_turn::GoalTurnStopReason::Cancelled
    );
    assert_eq!(
        session.get_task(task_id).expect("task").state(),
        TaskState::Cancelled
    );
    let process = result
        .observations()
        .iter()
        .find_map(|observation| match observation {
            WorkspaceObservation::ProcessFinished { result } => Some(result),
            _ => None,
        })
        .expect("host cancellation evidence");
    assert_eq!(process.outcome, crate::ProcessOutcome::Cancelled);
    assert!(process.cleanup_confirmed || process.recovery_hold);
    assert!(fs::read_to_string(descendant_file).is_ok());
}

#[test]
fn impact_checked_correction_cannot_skip_the_permission_gate() {
    let (mut session, task_id, turn_id) = running_turn();
    let plan = WorkspacePlan::try_from_actions(
        vec![super::super::WorkspaceAction::ReadFile {
            path: "Cargo.toml".into(),
        }],
        1,
    )
    .expect("registered plan operation");
    let checked = checked_plan(&session, task_id, turn_id, plan);
    let process_action = super::super::WorkspaceAction::OfflineCargoTest { test_filter: None };
    let correction = checked_plan(
        &session,
        task_id,
        turn_id,
        WorkspacePlan::try_from_actions(vec![process_action.clone()], 1)
            .expect("registered correction operation"),
    );
    let mut control = |_: &str, _: &[WorkspaceObservation]| -> super::super::WorkspaceAction {
        super::super::WorkspaceAction::Complete {
            result: "initial plan completed".to_owned(),
        }
    };
    let required_checks = [crate::harness::verification::RequiredCompletionCheck {
        id: "independent_check".to_owned(),
        description: "the independent check passes".to_owned(),
    }];
    let prompt_calls = Cell::new(0);
    let mut prompt = |seen_task, seen_turn, seen_action: &super::super::WorkspaceAction| {
        prompt_calls.set(prompt_calls.get() + 1);
        assert_eq!(seen_task, task_id);
        assert_ne!(seen_turn, turn_id);
        assert_eq!(seen_action, &process_action);
        Ok(false)
    };
    let mut gate = ActionPermissionGate::new(PermissionMode::Ask, true, Some(&mut prompt));

    let result = run_goal_turn_with_impact_checked_completion_correction_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "goal",
        7,
        Some(Ok(checked)),
        &required_checks,
        |_, _| {
            vec![crate::harness::verification::CompletionCheckEvidence {
                check_id: "independent_check".to_owned(),
                status: crate::harness::verification::CompletionCheckEvidenceStatus::Failed,
                source:
                    crate::harness::verification::CompletionEvidenceSource::DeterministicLocalCheck,
                summary: "the independent check failed".to_owned(),
            }]
        },
        |_, _, _, _, _, _, _, _, _| Ok::<_, WorkspacePlanError>(correction.clone()),
        &mut control,
        &mut gate,
    );

    assert!(matches!(
        result,
        Err(CompletionCorrectionError::Execution(
            GoalTurnError::PermissionDenied {
                action: "offline_cargo_test"
            }
        ))
    ));
    assert_eq!(prompt_calls.get(), 1);
}
