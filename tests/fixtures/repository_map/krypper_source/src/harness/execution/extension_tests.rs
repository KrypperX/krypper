use super::{
    dispatch_registered_extension_with_arguments_for_turn, ExtensionDispatchError,
    RegisteredExtensionWithArguments,
};
use crate::brain::provider_bridge::{BridgeError, ExtensionTool};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use crate::tools::capability_policy::{CapabilityContext, CapabilityDenial, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn registered_argument_handler_validates_schema_and_turn_ownership() {
    fn handler(arguments: &serde_json::Value) -> String {
        arguments["path"]
            .as_str()
            .expect("validated path")
            .to_owned()
    }

    let mut session = TaskSession::new();
    let task_id = session.create_task("argument handler").id();
    let turn_id = {
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("running");
        let turn = task.create_turn().expect("turn");
        task.transition_turn(turn, TurnState::Running)
            .expect("running turn");
        turn
    };
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [RegisteredExtensionWithArguments {
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
                "required": ["path"],
                "additionalProperties": false
            }),
            grant: &grant,
        },
        execute: handler,
    }];
    let context = CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    };
    let dispatch = |context, arguments: &serde_json::Value| {
        dispatch_registered_extension_with_arguments_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            "external_read",
            arguments,
        )
    };
    assert!(matches!(
        dispatch(context, &json!({"path": "notes.txt"})),
        Ok(value) if value == "notes.txt"
    ));
    assert!(matches!(
        dispatch(context, &json!({"path": 42})),
        Err(ExtensionDispatchError::Bridge(
            BridgeError::InvalidArguments
        ))
    ));
    assert!(matches!(
        dispatch(
            CapabilityContext {
                generation: 2,
                ..context
            },
            &json!({"path": "notes.txt"})
        ),
        Err(ExtensionDispatchError::Bridge(BridgeError::Denied(
            CapabilityDenial::StaleGrant
        )))
    ));
    assert!(matches!(
        dispatch(
            CapabilityContext {
                task_id: Uuid::new_v4(),
                ..context
            },
            &json!({"path": "notes.txt"})
        ),
        Err(ExtensionDispatchError::TaskMismatch)
    ));
    assert!(matches!(
        dispatch(context, &json!({})),
        Err(ExtensionDispatchError::Bridge(
            BridgeError::InvalidArguments
        ))
    ));
}
