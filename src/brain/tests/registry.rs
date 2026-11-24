//! Workspace registry, schema metadata and module compatibility.

use super::*;
use crate::agent_loop::WorkspaceAction;
use crate::ProcessOutcome;
use serde_json::{json, Value};
#[test]
fn workspace_registry_contains_only_existing_operations_and_schemas() {
    let actual = crate::tools::registry::workspace_operations()
        .iter()
        .map(|operation| {
            json!({
                "name": operation.name(),
                "description": operation.description(),
                "parameters": operation.parameters()
            })
        })
        .collect::<Vec<_>>();

    assert_eq!(
        Value::Array(actual),
        json!([
            {
                "name":"read_file",
                "description":"Read one safe UTF-8 workspace file.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}
            },
            {
                "name":"read_range",
                "description":"Read a bounded one-based line range from one safe UTF-8 file.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":200}},"required":["max_lines","path","start_line"],"additionalProperties":false}
            },
            {
                "name":"list_directory",
                "description":"List immediate entries in a safe workspace directory.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}
            },
            {
                "name":"create_directory",
                "description":"Create one directory inside the workspace.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}
            },
            {
                "name":"search_text",
                "description":"Find a literal query in immediate regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation.",
                "parameters":{"type":"object","properties":{"directory":{"type":"string"},"query":{"type":"string","minLength":1,"maxLength":4096}},"required":["directory","query"],"additionalProperties":false}
            },
            {
                "name":"search_tree_text",
                "description":"Find a literal query recursively in regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation, and skipping .git metadata directories.",
                "parameters":{"type":"object","properties":{"directory":{"type":"string"},"query":{"type":"string","minLength":1,"maxLength":4096}},"required":["directory","query"],"additionalProperties":false}
            },
            {
                "name":"create_file",
                "description":"Create one new UTF-8 file inside the workspace without replacing an existing path.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"},"contents":{"type":"string"}},"required":["contents","path"],"additionalProperties":false}
            },
            {
                "name":"replace_text",
                "description":"Replace one unique exact match in a safe UTF-8 workspace file.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"},"match_text":{"type":"string","minLength":1},"replacement_text":{"type":"string"}},"required":["match_text","path","replacement_text"],"additionalProperties":false}
            },
            {
                "name":"apply_patch",
                "description":"Apply one bounded single-file unified diff to an existing safe UTF-8 workspace file with exact hunk context.",
                "parameters":{"type":"object","properties":{"path":{"type":"string"},"patch":{"type":"string","minLength":1,"maxLength":32768}},"required":["patch","path"],"additionalProperties":false}
            },
            {
                "name":"offline_cargo_test",
                "description":"Run the fixed offline locked Cargo test profile with an optional literal test-name filter.",
                "parameters":{"type":"object","properties":{"test_filter":{"type":"string","minLength":1,"maxLength":256}},"required":[],"additionalProperties":false}
            }
        ])
    );
}

#[test]
fn provider_request_builders_keep_their_formats_with_registry_tools() {
    let observations = [];
    let ollama = serde_json::from_slice::<Value>(
        &build_chat_request("test-model", "goal", &observations, 3, None)
            .expect("build Ollama request"),
    )
    .expect("decode Ollama request");
    let responses = serde_json::from_slice::<Value>(
        &build_responses_request(
            "test-model",
            ModelProvider::OpenAi,
            "goal",
            &observations,
            3,
            None,
        )
        .expect("build Responses request"),
    )
    .expect("decode Responses request");
    let anthropic = serde_json::from_slice::<Value>(
        &build_anthropic_request("test-model", "goal", &observations, 3, None)
            .expect("build Anthropic request"),
    )
    .expect("decode Anthropic request");
    let gemini = serde_json::from_slice::<Value>(
        &build_gemini_request("gemini-2.5-flash", "goal", &observations, 3, None)
            .expect("build Gemini request"),
    )
    .expect("decode Gemini request");
    let compatible = serde_json::from_slice::<Value>(
        &crate::brain::providers::build_compatible_chat_request(
            "test-model",
            OpenAiCompatibleProvider::Groq,
            "goal",
            &observations,
            3,
            None,
        )
        .expect("build compatible chat request"),
    )
    .expect("decode compatible chat request");

    let function_tools = tool_definitions();
    let names = function_tools
        .as_array()
        .expect("function tool array")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "read_file",
            "read_range",
            "list_directory",
            "create_directory",
            "search_text",
            "search_tree_text",
            "create_file",
            "replace_text",
            "apply_patch",
            "offline_cargo_test",
            "complete_task"
        ]
    );
    assert_eq!(ollama["tools"], function_tools);
    assert_eq!(responses["tools"], openai_tool_definitions());
    assert_eq!(anthropic["tools"], anthropic_tool_definitions());
    assert_eq!(
        gemini["tools"][0]["functionDeclarations"],
        Value::Array(gemini_function_declarations())
    );
    assert_eq!(compatible["tools"], function_tools);
    assert_eq!(responses["tools"][0]["type"], "function");
    assert_eq!(anthropic["tools"][0]["strict"], Value::Bool(true));
    assert_eq!(
        gemini["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );

    assert_provider_operation_metadata(
        &ollama["tools"],
        &["function", "name"],
        &["function", "description"],
        &["function", "parameters"],
        clone_schema,
    );
    assert_provider_operation_metadata(
        &responses["tools"],
        &["name"],
        &["description"],
        &["parameters"],
        clone_schema,
    );
    assert_provider_operation_metadata(
        &anthropic["tools"],
        &["name"],
        &["description"],
        &["input_schema"],
        anthropic_strict_schema,
    );
    assert_provider_operation_metadata(
        &gemini["tools"][0]["functionDeclarations"],
        &["name"],
        &["description"],
        &["parameters"],
        gemini_supported_schema,
    );
    assert_provider_operation_metadata(
        &compatible["tools"],
        &["function", "name"],
        &["function", "description"],
        &["function", "parameters"],
        clone_schema,
    );
}

fn clone_schema(schema: &Value) -> Value {
    schema.clone()
}

fn nested_value<'a>(mut value: &'a Value, path: &[&str]) -> &'a Value {
    for segment in path {
        value = value
            .get(*segment)
            .unwrap_or_else(|| panic!("missing provider field at {segment}"));
    }
    value
}

fn assert_provider_operation_metadata(
    tools: &Value,
    name_path: &[&str],
    description_path: &[&str],
    schema_path: &[&str],
    schema_adapter: fn(&Value) -> Value,
) {
    let tools = tools.as_array().expect("provider tool array");

    for operation in crate::tools::registry::workspace_operations() {
        let matching = tools
            .iter()
            .filter(|tool| nested_value(tool, name_path).as_str() == Some(operation.name()))
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "provider tool {} must be unique",
            operation.name()
        );

        let expected_description = operation.provider_description();
        assert_eq!(
            nested_value(matching[0], description_path).as_str(),
            Some(expected_description.as_str()),
            "provider description for {}",
            operation.name()
        );
        assert_eq!(
            nested_value(matching[0], schema_path),
            &schema_adapter(&operation.parameters()),
            "provider schema for {}",
            operation.name()
        );
    }
}

#[test]
fn descriptive_metadata_does_not_register_unknown_tools_or_grant_cargo_authority() {
    let cargo_description = crate::tools::registry::workspace_operations()
        .iter()
        .find(|operation| operation.name() == "offline_cargo_test")
        .expect("registered Cargo operation")
        .provider_description();
    assert!(cargo_description.contains("this metadata does not grant access"));

    let unsupported = serde_json::to_vec(&json!({
        "message": {
            "role": "assistant",
            "tool_calls": [
                {"function":{"name":"shell_exec","arguments":{"command":"echo unsafe"}}}
            ]
        }
    }))
    .expect("encode unregistered tool fixture");
    assert_eq!(
        parse_model_plan_response(&unsupported, 1, &all_advertised_tools()),
        Err(WorkspacePlanError::UnsupportedOperation)
    );

    let (session, task_id, turn_id) = running_turn();
    let result = session
        .run_offline_cargo_tests(task_id, turn_id, None)
        .expect("record denied Cargo operation");
    assert_eq!(result.outcome, ProcessOutcome::Denied);
}

#[test]
fn public_model_source_can_be_constructed_without_a_request() {
    let _source = crate::OllamaDecisionSource::new("mock-model", 1);
}

#[test]
fn brain_module_source_path_preserves_root_compatibility() {
    let brain_source = crate::brain::OllamaDecisionSource::new("mock-model", 1);
    let _: crate::OllamaDecisionSource = brain_source;
}

#[test]
fn offline_profile_accepts_only_an_optional_literal_filter() {
    assert_eq!(
        action_from_call("offline_cargo_test", &json!({}), &all_advertised_tools()).unwrap(),
        WorkspaceAction::OfflineCargoTest { test_filter: None }
    );
    assert_eq!(
        action_from_call(
            "offline_cargo_test",
            &json!({"test_filter":"module::selected_test"}),
            &all_advertised_tools()
        )
        .unwrap(),
        WorkspaceAction::OfflineCargoTest {
            test_filter: Some("module::selected_test".into())
        }
    );
    for arguments in [
        json!({"command":"sh"}),
        json!({"cwd":"/"}),
        json!({"env":{"TOKEN":"x"}}),
        json!({"memory":0}),
        json!({"test_filter":""}),
        json!({"test_filter":"--list"}),
        json!({"test_filter":"line\nbreak"}),
        json!({"test_filter":"x".repeat(257)}),
        json!([]),
    ] {
        assert!(
            action_from_call("offline_cargo_test", &arguments, &all_advertised_tools()).is_err()
        );
    }
}

#[test]
fn advertised_tool_names_follow_each_request_schema_and_gemini_allow_list() {
    let gemini_request = build_gemini_request("gemini-2.5-flash", "goal", &[], 1, None).unwrap();
    let requests = [
        build_chat_request("fixture-model", "goal", &[], 1, None).unwrap(),
        build_responses_request("fixture-model", ModelProvider::OpenAi, "goal", &[], 1, None)
            .unwrap(),
        build_anthropic_request("fixture-model", "goal", &[], 1, None).unwrap(),
        gemini_request.clone(),
    ];
    for request in requests {
        let names = AdvertisedToolNames::from_request(&request).unwrap();
        assert!(names.contains("read_file"));
        assert!(names.contains("complete_task"));
    }

    let plan_request =
        build_plan_request_with_project_guidance("fixture-model", "goal", &[], 4, None, None)
            .unwrap();
    let plan_tools = AdvertisedToolNames::from_request(&plan_request).unwrap();
    assert!(plan_tools.contains("read_file"));
    assert!(!plan_tools.contains("complete_task"));

    let mut gemini_request: Value =
        serde_json::from_slice(&gemini_request).expect("decode Gemini request fixture");
    gemini_request["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] =
        json!(["read_file"]);
    let restricted = serde_json::to_vec(&gemini_request).expect("encode restricted Gemini request");
    let restricted_tools = AdvertisedToolNames::from_request(&restricted).unwrap();
    assert!(restricted_tools.contains("read_file"));
    assert!(!restricted_tools.contains("create_directory"));
}

#[test]
fn action_parser_rejects_every_unadvertised_builtin_and_extension_with_a_typed_error() {
    let advertised = AdvertisedToolNames::from_names(["read_file"]);
    for name in [
        "offline_cargo_test",
        "search_tree_text",
        "create_directory",
        "complete_task",
        "custom_extension",
    ] {
        assert!(matches!(
            action_from_call(name, &json!({}), &advertised),
            Err(ModelError::InvalidToolCall("tool was not advertised"))
        ));
    }
    assert!(matches!(
        action_from_provider_call(
            ModelProvider::Anthropic,
            "complete_task",
            &json!({"result":"done"}),
            &advertised,
        ),
        Err(ModelError::ProviderInvalidToolCall(
            ModelProvider::Anthropic,
            "tool was not advertised"
        ))
    ));
}

#[test]
fn an_advertised_extension_remains_dispatchable_after_schema_extraction() {
    let declaration = json!({
        "type":"function",
        "function":{
            "name":"custom_extension",
            "description":"host-registered extension fixture",
            "parameters":{
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            }
        }
    });
    let request =
        build_chat_request_with_extensions("fixture-model", "goal", &[], 1, None, &[declaration])
            .unwrap();
    let advertised = AdvertisedToolNames::from_request(&request).unwrap();
    let error = action_from_call("custom_extension", &json!({}), &advertised).unwrap_err();

    assert!(matches!(
        action_from_error(error),
        WorkspaceAction::Extension { name, arguments }
            if name == "custom_extension" && arguments == json!({})
    ));
}
