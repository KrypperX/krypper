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
    let mut current_context = || Ok(extensions.context);
    let mut prompt = |request: &super::super::ExtensionApprovalRequest| {
        assert_eq!(request.qualified_name, "external_read");
        assert_eq!(request.arguments, json!({}));
        Ok(true)
    };
    let mut approval = super::super::ExtensionApprovalGate::new(&mut current_context, &mut prompt);
    let result = run_goal_turn_with_approved_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        &mut choose,
        &extensions,
        &mut approval,
    )
    .expect("authorized extension");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionCompleted);
    assert_eq!(
        result.observations(),
        &[WorkspaceObservation::ExtensionCompleted {
            name: "external_read".to_owned(),
            result: "host-executed".to_owned()
        }]
    );
}

#[test]
fn existing_ungated_turn_api_cannot_invoke_registered_extension() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn handler(_: &serde_json::Value) -> String {
        HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        "unexpected".to_owned()
    }

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
        execute: handler,
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
        arguments: json!({}),
    };
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    assert!(matches!(
        run_goal_turn_with_extensions(
            &mut session,
            task_id,
            turn_id,
            "goal",
            &mut choose,
            Some(&extensions),
        ),
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
    ));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

#[test]
fn bounded_approved_turn_prompts_for_each_registered_effect() {
    use std::cell::Cell;

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
    let mut choose = |_: &str, observations: &[WorkspaceObservation]| {
        if observations.is_empty() {
            super::super::WorkspaceAction::Extension {
                name: "external_read".to_owned(),
                arguments: json!({}),
            }
        } else {
            super::super::WorkspaceAction::Continue
        }
    };
    let mut current_context = || Ok(extensions.context);
    let prompt_calls = Cell::new(0);
    let mut prompt = |request: &super::super::ExtensionApprovalRequest| {
        assert_eq!(request.qualified_name, "external_read");
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut approval = super::super::ExtensionApprovalGate::new(&mut current_context, &mut prompt);

    let result = run_goal_turn_with_budget_and_approved_extensions(
        &mut session,
        task_id,
        turn_id,
        "goal",
        3,
        &mut choose,
        &extensions,
        &mut approval,
    )
    .expect("bounded extension turn");
    assert_eq!(result.stop_reason(), GoalTurnStopReason::Continued);
    assert_eq!(result.actions_taken(), 2);
    assert_eq!(prompt_calls.get(), 1);
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
    );
    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
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
    );
    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
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
    );
    assert!(matches!(
        result,
        Err(GoalTurnError::PermissionPromptUnavailable {
            action: "registered extension"
        })
    ));
}
