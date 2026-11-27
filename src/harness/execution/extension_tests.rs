use super::{
    dispatch_registered_extension_with_approval_for_turn,
    dispatch_registered_extension_with_arguments_for_turn, ExtensionApprovalGate,
    ExtensionApprovalRequest, ExtensionDispatchError, RegisteredExtensionWithArguments,
};
use crate::brain::provider_bridge::{BridgeError, ExtensionTool};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use crate::tools::capability_policy::{CapabilityContext, CapabilityDenial, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

#[test]
fn ungated_registered_dispatch_fails_closed_without_running_handler() {
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
        Err(ExtensionDispatchError::ApprovalRequired)
    ));
    assert!(matches!(
        dispatch(context, &json!({"path": 42})),
        Err(ExtensionDispatchError::ApprovalRequired)
    ));
    assert!(matches!(
        dispatch(
            CapabilityContext {
                generation: 2,
                ..context
            },
            &json!({"path": "notes.txt"})
        ),
        Err(ExtensionDispatchError::ApprovalRequired)
    ));
    assert!(matches!(
        dispatch(
            CapabilityContext {
                task_id: Uuid::new_v4(),
                ..context
            },
            &json!({"path": "notes.txt"})
        ),
        Err(ExtensionDispatchError::ApprovalRequired)
    ));
    assert!(matches!(
        dispatch(context, &json!({})),
        Err(ExtensionDispatchError::ApprovalRequired)
    ));
}

#[test]
fn approved_registered_dispatch_revalidates_and_invokes_once_per_approval() {
    static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn handler(arguments: &serde_json::Value) -> String {
        HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        arguments["path"]
            .as_str()
            .expect("validated path")
            .to_owned()
    }

    let mut session = TaskSession::new();
    let task_id = session.create_task("approved extension").id();
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
    let mut context_source = || Ok(context);
    let prompt_requests = RefCell::new(Vec::<ExtensionApprovalRequest>::new());
    let mut prompt = |request: &ExtensionApprovalRequest| {
        prompt_requests.borrow_mut().push(request.clone());
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut context_source, &mut prompt);
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    for expected_calls in 1..=2 {
        let result = dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        )
        .expect("host-approved extension");
        assert_eq!(result, "notes.txt");
        assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), expected_calls);
    }
    let requests = prompt_requests.into_inner();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].qualified_name, "external_read");
    assert_eq!(requests[0].arguments, json!({"path": "notes.txt"}));
    assert_eq!(requests[0].workspace_revision, 0);
    assert_ne!(requests[0].intent_digest, [0; 32]);
}

#[test]
fn denial_schema_failure_and_revocation_during_prompt_never_invoke_handler() {
    static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn handler(_: &serde_json::Value) -> String {
        HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        "unexpected".to_owned()
    }

    let mut session = TaskSession::new();
    let task_id = session.create_task("denied extension").id();
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
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    let mut context_source = || Ok(context);
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: &ExtensionApprovalRequest| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(false)
    };
    let mut gate = ExtensionApprovalGate::new(&mut context_source, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        ),
        Err(ExtensionDispatchError::ApprovalDenied)
    ));
    assert_eq!(prompt_calls.get(), 1);
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);

    let mut context_source = || Ok(context);
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: &ExtensionApprovalRequest| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut context_source, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": 42}),
        ),
        Err(ExtensionDispatchError::Bridge(
            BridgeError::InvalidArguments
        ))
    ));
    assert_eq!(prompt_calls.get(), 0);
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);

    let revoked = Cell::new(false);
    let mut current_context = || {
        Ok(CapabilityContext {
            generation: if revoked.get() { 2 } else { 1 },
            ..context
        })
    };
    let mut prompt = |_: &ExtensionApprovalRequest| {
        revoked.set(true);
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut current_context, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        ),
        Err(ExtensionDispatchError::Bridge(BridgeError::Denied(
            CapabilityDenial::StaleGrant
        )))
    ));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);

    let mut unavailable_context = || Err(());
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: &ExtensionApprovalRequest| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut unavailable_context, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        ),
        Err(ExtensionDispatchError::AuthorizationUnavailable)
    ));
    assert_eq!(prompt_calls.get(), 0);

    let mut current_context = || Ok(context);
    let mut prompt = |_: &ExtensionApprovalRequest| Err(());
    let mut gate = ExtensionApprovalGate::new(&mut current_context, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        ),
        Err(ExtensionDispatchError::ApprovalPromptFailed)
    ));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);

    let mut current_context = || Ok(context);
    let prompt_calls = Cell::new(0);
    let mut prompt = |_: &ExtensionApprovalRequest| {
        prompt_calls.set(prompt_calls.get() + 1);
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut current_context, &mut prompt);
    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            Uuid::new_v4(),
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({"path": "notes.txt"}),
        ),
        Err(ExtensionDispatchError::Turn(_))
    ));
    assert_eq!(prompt_calls.get(), 0);
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

#[test]
fn managed_workspace_change_during_prompt_denies_the_effect() {
    static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn handler(_: &serde_json::Value) -> String {
        HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
        "unexpected".to_owned()
    }

    let workspace = crate::test_support::TempDir::with_prefix("krypper-extension-revision-");
    let mut session = TaskSession::new_in_workspace(workspace.path()).expect("workspace session");
    let task_id = session.create_task("approved extension").id();
    let turn_id = {
        let task = session.get_task_mut(task_id).expect("task");
        task.transition(TaskState::Running).expect("running");
        let turn = task.create_turn().expect("turn");
        task.transition_turn(turn, TurnState::Running)
            .expect("running turn");
        turn
    };
    let mut writer_session =
        TaskSession::new_in_workspace(workspace.path()).expect("writer session");
    let writer_task_id = writer_session.create_task("managed writer").id();
    let writer_turn_id = {
        let task = writer_session
            .get_task_mut(writer_task_id)
            .expect("writer task");
        task.transition(TaskState::Running).expect("writer running");
        let turn = task.create_turn().expect("writer turn");
        task.transition_turn(turn, TurnState::Running)
            .expect("writer turn running");
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
            parameters: json!({"type":"object","properties":{}}),
            grant: &grant,
        },
        execute: handler,
    }];
    let context = CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    };
    let mut context_source = || Ok(context);
    let mut prompt = |_: &ExtensionApprovalRequest| {
        writer_session
            .create_workspace_file(
                writer_task_id,
                writer_turn_id,
                std::path::Path::new("change.txt"),
                "changed",
            )
            .expect("managed mutation during prompt");
        Ok(true)
    };
    let mut gate = ExtensionApprovalGate::new(&mut context_source, &mut prompt);
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    assert!(matches!(
        dispatch_registered_extension_with_approval_for_turn(
            &session,
            task_id,
            turn_id,
            &registry,
            context,
            &mut gate,
            "external_read",
            &json!({}),
        ),
        Err(ExtensionDispatchError::StaleWorkspace)
    ));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}
