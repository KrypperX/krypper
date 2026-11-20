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

#[test]
fn rejects_stale_grant_and_wrong_task() {
    let task_id = Uuid::new_v4();
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
    let stale = CapabilityContext {
        task_id,
        generation: 2,
        task_authorized: true,
    };
    assert_eq!(
        authorized_declarations(&tools, stale),
        Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
    );
    let wrong_task = CapabilityContext {
        task_id: Uuid::new_v4(),
        generation: 1,
        task_authorized: true,
    };
    assert_eq!(
        authorized_declarations(&tools, wrong_task),
        Err(BridgeError::Denied(CapabilityDenial::WrongTask))
    );
}

#[test]
fn rejects_unsupported_schema() {
    let task_id = Uuid::new_v4();
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
        parameters: json!({"type":"array"}),
        grant: &grant,
    }];
    let context = CapabilityContext {
        task_id,
        generation: 1,
        task_authorized: true,
    };
    assert_eq!(
        authorized_declarations(&tools, context),
        Err(BridgeError::InvalidSchema)
    );
}

#[test]
fn dispatch_rejects_invalid_schema_even_with_valid_grant() {
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
        description: "Read",
        parameters: json!({"type":"array"}),
        grant: &grant,
    }];
    assert_eq!(
        authorize_dispatch(&tools, context, "external_read"),
        Err(BridgeError::InvalidSchema)
    );
}

#[test]
fn unknown_extension_cannot_dispatch() {
    let context = CapabilityContext {
        task_id: Uuid::new_v4(),
        generation: 1,
        task_authorized: true,
    };
    assert_eq!(
        authorize_dispatch(&[], context, "external_unknown"),
        Err(BridgeError::UnknownTool)
    );
}

#[test]
fn authorized_declarations_adapt_to_all_provider_formats() {
    let task_id = Uuid::new_v4();
    let context = CapabilityContext {
        task_id,
        generation: 3,
        task_authorized: true,
    };
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 3,
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
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        grant: &grant,
    }];
    let declarations = authorized_declarations(&tools, context).unwrap();
    // Ollama and OpenAI-compatible chat use the common function envelope.
    assert_eq!(declarations[0]["type"], "function");
    assert_eq!(declarations[0]["function"]["name"], "external_read");
    let responses = responses_declarations(&declarations);
    assert_eq!(responses[0]["name"], "external_read");
    assert_eq!(responses[0]["type"], "function");
    let anthropic = anthropic_declarations(&declarations);
    assert_eq!(anthropic[0]["name"], "external_read");
    assert_eq!(anthropic[0]["input_schema"]["additionalProperties"], false);
    let gemini = gemini_declarations(&declarations);
    assert_eq!(gemini[0]["name"], "external_read");
    assert_eq!(gemini[0]["parameters"]["type"], "OBJECT");
    assert_eq!(
        gemini[0]["parameters"]["properties"]["path"]["type"],
        "STRING"
    );
    assert_eq!(authorize_dispatch(&tools, context, "external_read"), Ok(()));
    assert_eq!(
        authorize_dispatch(&tools, context, "external_write"),
        Err(BridgeError::UnknownTool)
    );
}

#[test]
fn revoked_extension_is_not_exposed_to_any_provider_format() {
    let task_id = Uuid::new_v4();
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
        description: "Read",
        parameters: json!({"type":"object","properties":{}}),
        grant: &grant,
    }];
    let revoked = CapabilityContext {
        task_id,
        generation: 2,
        task_authorized: true,
    };
    assert_eq!(
        authorized_declarations(&tools, revoked),
        Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
    );
    assert_eq!(
        authorize_dispatch(&tools, revoked, "external_read"),
        Err(BridgeError::Denied(CapabilityDenial::StaleGrant))
    );
    let empty = Vec::new();
    assert!(responses_declarations(&empty).is_empty());
    assert!(anthropic_declarations(&empty).is_empty());
    assert!(gemini_declarations(&empty).is_empty());
}

#[test]
fn dispatch_arguments_reject_missing_extra_and_wrong_types() {
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
        description: "Read",
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
        grant: &grant,
    }];
    for invalid in [
        json!({}),
        json!({"path": 42}),
        json!({"path": "a", "unexpected": true}),
        json!(null),
    ] {
        assert_eq!(
            authorize_dispatch_with_arguments(&tools, context, "external_read", &invalid),
            Err(BridgeError::InvalidArguments)
        );
    }
    assert_eq!(
        authorize_dispatch_with_arguments(
            &tools,
            context,
            "external_read",
            &json!({"path": "src/main.rs"})
        ),
        Ok(())
    );
    assert_eq!(
        authorize_dispatch_with_arguments(
            &tools,
            context,
            "external_unknown",
            &json!({"path": "src/main.rs"})
        ),
        Err(BridgeError::UnknownTool)
    );
}

#[test]
fn nested_arguments_are_checked_recursively() {
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
        description: "Read",
        parameters: json!({
            "type": "object",
            "properties": {
                "filters": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["filters"],
            "additionalProperties": false
        }),
        grant: &grant,
    }];
    assert!(authorized_declarations(&tools, context).is_ok());
    assert_eq!(
        authorize_dispatch_with_arguments(
            &tools, context, "external_read", &json!({"filters": [{"path": "src"}]})
        ),
        Ok(())
    );
    for invalid in [
        json!({"filters": [{"path": 42}]}),
        json!({"filters": [{}]}),
        json!({"filters": [{"path": "src", "extra": true}]}),
        json!({"filters": "not an array"}),
    ] {
        assert_eq!(
            authorize_dispatch_with_arguments(&tools, context, "external_read", &invalid),
            Err(BridgeError::InvalidArguments)
        );
    }
}

#[test]
fn unsupported_schema_keywords_are_rejected_before_advertisement() {
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
    for schema in [
        json!({"type": "object", "properties": {"path": {"type": "string", "minLength": 2}}}),
        json!({"type": "object", "properties": {}, "additionalProperties": true}),
        json!({"type": "object", "properties": {"values": {"type": "array"}}}),
    ] {
        let tools = [ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: schema,
            grant: &grant,
        }];
        assert_eq!(
            authorized_declarations(&tools, context),
            Err(BridgeError::InvalidSchema)
        );
    }
}
