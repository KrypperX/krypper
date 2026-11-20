use super::*;
use crate::brain::provider_bridge::ExtensionTool;
use crate::task_lifecycle::{TaskState, TurnState};
use crate::tools::capability_policy::{CapabilityContext, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use serde_json::json;

fn running_turn() -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new();
    let task_id = session.create_task("extension integration").id();
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

#[test]
fn authorized_extension_runs_through_existing_goal_turn() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [super::super::RegisteredExtensionWithArguments {
        tool: ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({"type":"object","properties":{}}),
            grant: &grant,
        },
        execute: |_: &serde_json::Value| "host-executed".to_owned(),
    }];
    let extensions = TurnExtensions {
        registry: &registry,
        context: CapabilityContext {
            task_id,
            generation: 1,
            task_authorized: true,
        },
    };
    let mut choose = |_: &str| super::super::WorkspaceAction::Extension {
        name: "external_read".to_owned(),
        arguments: serde_json::json!({}),
    };
    let result = run_goal_turn_with_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        &mut choose,
        Some(&extensions),
    )
    .expect("authorized extension");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        result.outcome(),
        Some(&TurnOutcome::Success {
            result: "host-executed".to_owned()
        })
    );
}

#[test]
fn required_extension_arguments_fail_closed_in_bounded_execution_loop() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [super::super::RegisteredExtensionWithArguments {
        tool: ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            grant: &grant,
        },
        execute: |_: &serde_json::Value| panic!("required arguments were bypassed"),
    }];
    let extensions = TurnExtensions {
        registry: &registry,
        context: CapabilityContext {
            task_id,
            generation: 1,
            task_authorized: true,
        },
    };
    let mut choose =
        |_: &str, _: &[WorkspaceObservation]| super::super::WorkspaceAction::Extension {
            name: "external_read".to_owned(),
            arguments: serde_json::json!({}),
        };
    let result = run_goal_turn_with_budget_and_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut choose,
        Some(&extensions),
    )
    .expect("required arguments must fail closed");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}

#[test]
fn revoked_extension_is_denied_in_bounded_execution_loop() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [super::super::RegisteredExtensionWithArguments {
        tool: ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({"type":"object","properties":{}}),
            grant: &grant,
        },
        execute: |_: &serde_json::Value| panic!("revoked extension executed"),
    }];
    let extensions = TurnExtensions {
        registry: &registry,
        context: CapabilityContext {
            task_id,
            generation: 2,
            task_authorized: true,
        },
    };
    let mut choose =
        |_: &str, _: &[WorkspaceObservation]| super::super::WorkspaceAction::Extension {
            name: "external_read".to_owned(),
            arguments: serde_json::json!({}),
        };
    let result = run_goal_turn_with_budget_and_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        2,
        &mut choose,
        Some(&extensions),
    )
    .expect("denial recorded as failed turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
}

#[test]
fn built_in_continue_does_not_invoke_registered_extension() {
    let (mut session, task_id, turn_id) = running_turn();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [super::super::RegisteredExtensionWithArguments {
        tool: ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({"type":"object","properties":{}}),
            grant: &grant,
        },
        execute: |_: &serde_json::Value| panic!("built-in action must not invoke extension"),
    }];
    let extensions = TurnExtensions {
        registry: &registry,
        context: CapabilityContext {
            task_id,
            generation: 1,
            task_authorized: true,
        },
    };
    let mut choose = |_: &str| super::super::WorkspaceAction::Continue;
    let result = run_goal_turn_with_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        &mut choose,
        Some(&extensions),
    )
    .expect("built-in action remains available");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Continued);
    assert_eq!(
        result.outcome(),
        Some(&TurnOutcome::Success {
            result: "continued".to_owned()
        })
    );
}

#[test]
fn wrong_task_context_never_invokes_extension() {
    let (mut session, task_id, turn_id) = running_turn();
    let extensions = TurnExtensions {
        registry: &[],
        context: CapabilityContext {
            task_id: Uuid::new_v4(),
            generation: 1,
            task_authorized: true,
        },
    };
    let mut choose =
        |_: &str, _: &[WorkspaceObservation]| super::super::WorkspaceAction::Extension {
            name: "external_read".to_owned(),
            arguments: serde_json::json!({}),
        };
    let result = run_goal_turn_with_budget_and_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        1,
        &mut choose,
        Some(&extensions),
    )
    .expect("wrong task fails closed");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
}
