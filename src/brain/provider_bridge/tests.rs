use super::*;
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use uuid::Uuid;

#[test]
fn authorized_tool_uses_existing_function_shape() {
    let task_id = Uuid::new_v4();
    let context = CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    };
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let tools = [ExtensionTool {
        descriptor: ExtensionDescriptor {
            version: EXTENSION_CONTRACT_VERSION,
            namespace: "external",
            name: "read",
            effect: ExtensionEffect::ReadWorkspace,
            authority: ExtensionAuthority::Task,
        },
        description: "Read approved data",
        parameters: json!({"type":"object","properties":{}}),
        grant: &grant,
    }];
    let declarations = authorized_declarations(&tools, context).unwrap();
    assert_eq!(declarations[0]["function"]["name"], "external_read");
}

#[test]
fn unauthorized_task_is_not_advertised() {
    let context = CapabilityContext {
        task_id: Uuid::new_v4(),
        generation: 0,
        task_authorized: false,
    };
    assert!(authorized_declarations(&[], context).unwrap().is_empty());
}
