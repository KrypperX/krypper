use krypper::extensions::{
    authorize_dispatch, authorized_declarations, BridgeError, CapabilityContext, CapabilityDenial,
    CapabilityGrant, ExtensionAuthority, ExtensionContractError, ExtensionDescriptor,
    ExtensionEffect, ExtensionTool, EXTENSION_CONTRACT_VERSION,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn extension_facade_keeps_contract_types_and_authorization_functions_available() {
    let task_id = Uuid::new_v4();
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let context = CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    };
    let tool = ExtensionTool {
        descriptor: ExtensionDescriptor {
            version: EXTENSION_CONTRACT_VERSION,
            namespace: "external",
            name: "read",
            effect: ExtensionEffect::ReadWorkspace,
            authority: ExtensionAuthority::Task,
        },
        description: "Read a path",
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        grant: &grant,
    };

    assert_eq!(
        authorized_declarations(&[tool], context).expect("authorized declarations")[0]["function"]
            ["name"],
        "external_read"
    );
    let denied: Result<(), BridgeError> = authorize_dispatch(&[], context, "external_missing");
    assert_eq!(denied, Err(BridgeError::UnknownTool));

    let _contract_error: Option<ExtensionContractError> = None;
    let _capability_denial: Option<CapabilityDenial> = None;
}
