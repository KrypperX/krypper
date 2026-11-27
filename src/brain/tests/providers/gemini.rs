//! Gemini generateContent provider contract.

use super::super::*;
use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
};
use serde_json::{json, Value};
use std::path::PathBuf;
pub(super) fn gemini_function_call(name: &str, arguments: Value) -> Value {
    json!({"functionCall":{"name":name,"args":arguments}})
}

pub(super) fn gemini_response(parts: Vec<Value>) -> String {
    json!({
        "candidates":[{
            "finishReason":"STOP",
            "content":{"role":"model","parts":parts}
        }]
    })
    .to_string()
}

#[test]
fn gemini_model_name_builds_only_a_safe_fixed_generate_content_path() {
    assert_eq!(
        gemini_generate_content_endpoint("gemini-2.5-flash").expect("valid model endpoint"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
    );
    for invalid in [
        "",
        ".",
        "..",
        "gemini/other",
        "gemini\\other",
        "gemini?query",
        "gemini#fragment",
        "gemini%2fother",
        "https://example.test/model",
    ] {
        assert!(
            validate_model_name_for_provider(invalid, ModelProvider::Gemini).is_err(),
            "accepted unsafe Gemini model name {invalid:?}"
        );
        assert!(gemini_generate_content_endpoint(invalid).is_err());
    }
}

#[test]
fn gemini_generate_content_uses_native_schema_api_key_and_one_bounded_action() {
    let key = "local-gemini-test-key";
    let response = gemini_response(vec![gemini_function_call(
        "read_file",
        json!({"path":"plan.md"}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let endpoint = format!("http://{address}/v1beta/models/test-model:generateContent");
    let mut source = GeminiDecisionSource::with_endpoint("test-model", key, 3, endpoint);
    let action = source.choose_action(
        "Inspect the plan and create a result",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("brief.txt"),
            contents: "local-only fixture contents".to_owned(),
        }],
    );
    let requests = server.join().expect("join Gemini mock server");
    let request = requests.first().expect("one Gemini request");
    let request_line = String::from_utf8_lossy(request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();

    assert!(request_line.starts_with("POST /v1beta/models/test-model:generateContent "));
    assert_eq!(
        request_header(request, "x-goog-api-key").as_deref(),
        Some(key)
    );
    assert_eq!(request_header(request, "authorization"), None);
    let body = request_body(request);
    assert!(body["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("Select exactly one tool call")));
    assert_eq!(body["contents"][0]["role"], "user");
    let prompt = body["contents"][0]["parts"][0]["text"]
        .as_str()
        .expect("Gemini user prompt");
    assert!(prompt.contains("Inspect the plan and create a result"));
    assert!(prompt.contains("local-only fixture contents"));
    assert!(prompt.contains("including this decision: 3"));
    assert_eq!(body["generationConfig"]["candidateCount"], 1);
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    let declarations = body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("Gemini function declarations");
    let names = declarations
        .iter()
        .filter_map(|declaration| declaration["name"].as_str())
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
            "offline_read_only_exec",
            "complete_task"
        ]
    );
    let allowed = body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"]
        .as_array()
        .expect("allowed Gemini function names")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(allowed, names);
    let read_range = declarations
        .iter()
        .find(|declaration| declaration["name"] == "read_range")
        .expect("read_range declaration");
    assert_eq!(read_range["parameters"]["type"], "OBJECT");
    assert_eq!(
        read_range["parameters"]["properties"]["path"]["type"],
        "STRING"
    );
    assert_eq!(
        read_range["parameters"]["properties"]["max_lines"]["type"],
        "INTEGER"
    );
    assert!(read_range["parameters"]
        .get("additionalProperties")
        .is_none());
    assert!(read_range["parameters"]["properties"]["max_lines"]
        .get("maximum")
        .is_none());
    assert!(read_range["parameters"]["properties"]["max_lines"]
        .get("minimum")
        .is_none());
    assert!(!body.to_string().contains(key));
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn gemini_response_parser_requires_exactly_one_well_formed_supported_call() {
    let one_call = gemini_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"candidates":[]}).to_string().into_bytes(),
        gemini_response(vec![json!({"text":"no function call"})]).into_bytes(),
        gemini_response(vec![one_call.clone(), one_call.clone()]).into_bytes(),
        json!({"candidates":[
            {"content":{"parts":[one_call.clone()]}},
            {"content":{"parts":[one_call.clone()]}}
        ]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"MALFORMED_FUNCTION_CALL",
            "content":{"parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"SAFETY",
            "content":{"role":"model","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "finishReason":"MAX_TOKENS",
            "content":{"role":"model","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[{
            "content":{"role":"user","parts":[one_call.clone()]}
        }]})
        .to_string()
        .into_bytes(),
        json!({"candidates":[
            {"content":{"role":"model","parts":[one_call.clone()]}},
            {"content":{"role":"model"}}
        ]})
        .to_string()
        .into_bytes(),
        gemini_response(vec![json!({"functionCall":{"args":{"result":"done"}}})]).into_bytes(),
        gemini_response(vec![
            json!({"functionCall":{"name":"complete_task","args":"not an object"}}),
        ])
        .into_bytes(),
        gemini_response(vec![gemini_function_call(
            "run_shell",
            json!({"command":"unsafe"}),
        )])
        .into_bytes(),
    ];

    for response in invalid_responses {
        assert!(parse_gemini_response(&response, &all_advertised_tools()).is_err());
    }
}

#[test]
fn gemini_parser_keeps_local_argument_bounds_authoritative() {
    for (name, args) in [
        (
            "read_range",
            json!({"path":"plan.md","start_line":1,"max_lines":201}),
        ),
        (
            "search_text",
            json!({"directory":".","query":"x".repeat(4097)}),
        ),
    ] {
        assert!(parse_gemini_response(
            gemini_response(vec![gemini_function_call(name, args)]).as_bytes(),
            &all_advertised_tools()
        )
        .is_err());
    }
}

#[test]
fn gemini_invalid_call_responses_fail_without_executing_workspace_actions() {
    let invalid_responses = [
        gemini_response(vec![json!({"text":"no function call"})]),
        gemini_response(vec![
            gemini_function_call(
                "create_file",
                json!({"path":"must-not-exist.txt","contents":"blocked"}),
            ),
            gemini_function_call("complete_task", json!({"result":"done"})),
        ]),
        gemini_response(vec![gemini_function_call(
            "create_file",
            json!({"path":"must-not-exist.txt"}),
        )]),
        gemini_response(vec![gemini_function_call(
            "run_shell",
            json!({"command":"touch must-not-exist.txt"}),
        )]),
    ];

    for response in invalid_responses {
        let (address, server) = mock_server(vec![ok(response)]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = GeminiDecisionSource::with_endpoint(
                "test-model",
                "local-gemini-key",
                1,
                format!("http://{address}/v1beta/models/test-model:generateContent"),
            );
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject an invalid function call response",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("invalid call failure is recorded in the turn");

        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(!workspace.path().join("must-not-exist.txt").exists());
        assert_eq!(server.join().expect("join Gemini mock server").len(), 1);
    }
}

#[test]
fn gemini_provider_errors_never_echo_api_keys_or_response_bodies() {
    let key = "local-gemini-error-key";
    let (address, server) = mock_server(vec![MockResponse {
        status: 401,
        body: json!({"error":{"message":key}}).to_string(),
    }]);
    let mut source = GeminiDecisionSource::with_endpoint(
        "test-model",
        key,
        1,
        format!("http://{address}/v1beta/models/test-model:generateContent"),
    );
    let action = source.choose_action("Test a failed Gemini request", &[]);
    let requests = server.join().expect("join Gemini error mock server");
    let request = requests.first().expect("one failed Gemini request");
    assert_eq!(
        request_header(request, "x-goog-api-key").as_deref(),
        Some(key)
    );
    assert!(!request_body(request).to_string().contains(key));

    let WorkspaceAction::Unsupported { action } = action else {
        panic!("provider error should fail as an unsupported bounded action");
    };
    assert!(action.contains("401"));
    assert!(!action.contains(key));
    assert!(!action.contains("error\":{\"message"));
}
