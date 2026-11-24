//! Actions, parsers and errors of the compatible profiles.

use super::super::*;
use super::compatible::*;
use crate::agent_loop::{
    run_goal_turn_with_budget, BoundedDecisionSource, GoalTurnStopReason, WorkspaceAction,
};
use crate::task_lifecycle::TurnState;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
#[test]
fn mistral_chat_profile_uses_auto_tool_choice_and_accepts_one_supported_call() {
    let key = "local-mistral-test-key";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "create_file",
        json!({"path":"mistral.txt","contents":"from Mistral"}),
    )]);
    let (action, request) = run_compatible_profile(mistral_provider(), key, response);
    assert_compatible_chat_request_with_tool_choice(&request, key, "auto");
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("mistral.txt"),
            contents: "from Mistral".to_owned(),
        }
    );
}

#[test]
fn cloudflare_compatible_response_exposes_token_usage_to_the_caller() {
    let key = "local-cloudflare-usage-token";
    let mut response: Value = serde_json::from_str(&compatible_chat_response(vec![
        compatible_chat_function_call("complete_task", json!({"result": "done"})),
    ]))
    .expect("build mock Cloudflare response");
    response["usage"] = json!({
        "prompt_tokens": 123,
        "completion_tokens": 17,
        "total_tokens": 140,
    });

    let (address, server) = mock_server(vec![ok(response.to_string())]);
    let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
        "test-model",
        OpenAiCompatibleProvider::Cloudflare,
        key,
        1,
        format!("http://{address}/v1/chat/completions"),
    );
    let action = source.choose_action("finish", &[]);

    assert_eq!(
        action,
        WorkspaceAction::Complete {
            result: "done".to_owned()
        }
    );
    assert_eq!(
        source.take_token_usage(),
        Some(crate::brain::ModelTokenUsage {
            prompt_tokens: Some(123),
            completion_tokens: Some(17),
            total_tokens: Some(140),
        })
    );
    assert_eq!(server.join().expect("join Cloudflare mock").len(), 1);
}

#[test]
fn mistral_action_uses_the_existing_workspace_and_action_budget_path() {
    let key = "local-mistral-test-key";
    let (address, server) = mock_server(vec![
        ok(compatible_chat_response(vec![
            compatible_chat_function_call(
                "create_file",
                json!({"path":"mistral result.txt","contents":"created through task safety"}),
            ),
        ])),
        ok(compatible_chat_response(vec![
            compatible_chat_function_call(
                "complete_task",
                json!({"result":"Created mistral result.txt."}),
            ),
        ])),
    ]);
    let workspace = TempDir::new();
    let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
        let (mut session, task_id, turn_id) = running_turn();
        let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
            "test-model",
            mistral_provider(),
            key,
            2,
            format!("http://{address}/v1/chat/completions"),
        );
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            "Create a Mistral result file",
            2,
            &mut source,
        );
        (result, session, task_id, turn_id)
    });
    let result = result.expect("run Mistral actions through the bounded task loop");

    assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        fs::read_to_string(workspace.path().join("mistral result.txt"))
            .expect("read result created under the workspace"),
        "created through task safety"
    );
    assert_eq!(
        session
            .get_task(task_id)
            .expect("task exists")
            .get_turn(turn_id)
            .expect("turn exists")
            .state(),
        TurnState::Succeeded
    );
    let requests = server.join().expect("join Mistral task mock");
    assert_eq!(requests.len(), 2);
    let body = request_body(&requests[0]);
    assert_eq!(
        request_header(&requests[0], "authorization"),
        Some(format!("Bearer {key}"))
    );
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["n"], 1);
    assert_eq!(body["stream"], false);
    assert!(body["messages"][1]["content"]
        .as_str()
        .is_some_and(|prompt| {
            prompt.contains("Create a Mistral result file")
                && prompt.contains("including this decision: 2")
        }));
    assert!(!body.to_string().contains(key));
}

#[test]
fn deepseek_and_zai_parsers_accept_object_and_json_text_arguments() {
    for provider in [
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
    ] {
        for arguments in [
            Value::String(json!({"result":"done"}).to_string()),
            json!({"result":"done"}),
        ] {
            let response =
                compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
                    "complete_task",
                    arguments,
                )]);
            assert_eq!(
                parse_compatible_chat_response(
                    response.as_bytes(),
                    ModelProvider::OpenAiCompatible(provider),
                    &all_advertised_tools()
                )
                .expect("valid provider arguments"),
                WorkspaceAction::Complete {
                    result: "done".to_owned()
                }
            );
        }
    }
}

#[test]
fn compatible_chat_parser_rejects_missing_malformed_unsupported_and_multiple_calls() {
    let one_call = compatible_chat_function_call("complete_task", json!({"result":"done"}));
    let invalid_responses = [
        b"not json".to_vec(),
        json!({"choices":[]}).to_string().into_bytes(),
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","tool_calls":[]}}]}).to_string().into_bytes(),
        compatible_chat_response(vec![one_call.clone(), one_call.clone()]).into_bytes(),
        json!({"choices":[
            {"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[one_call.clone()]}},
            {"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[one_call.clone()]}}
        ]}).to_string().into_bytes(),
        compatible_chat_response(vec![compatible_chat_function_call("run_shell", json!({}))]).into_bytes(),
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{
            "id":"call_bad","type":"function","function":{"name":"complete_task","arguments":"{invalid"}
        }]}}]}).to_string().into_bytes(),
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "create_file",
            json!({"path":"missing-contents.txt"}),
        )])
        .into_bytes(),
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"user","tool_calls":[one_call]}}]}).to_string().into_bytes(),
    ];

    for provider in [
        OpenAiCompatibleProvider::Groq,
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
        mistral_provider(),
    ] {
        for response in &invalid_responses {
            assert!(parse_compatible_chat_response(
                response,
                ModelProvider::OpenAiCompatible(provider),
                &all_advertised_tools()
            )
            .is_err());
        }
    }

    let object_arguments =
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "complete_task",
            json!({"result":"done"}),
        )]);
    assert!(parse_compatible_chat_response(
        object_arguments.as_bytes(),
        ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Groq),
        &all_advertised_tools()
    )
    .is_err());
}

#[test]
fn compatible_chat_multiple_calls_fail_without_executing_either_action() {
    for provider in [
        OpenAiCompatibleProvider::Groq,
        OpenAiCompatibleProvider::DeepSeek,
        OpenAiCompatibleProvider::Zai,
        mistral_provider(),
    ] {
        let response = compatible_chat_response(vec![
            compatible_chat_function_call(
                "create_file",
                json!({"path":"must-not-exist.txt","contents":"blocked"}),
            ),
            compatible_chat_function_call("complete_task", json!({"result":"done"})),
        ]);
        let (address, server) = mock_server(vec![ok(response)]);
        let workspace = TempDir::new();
        let (result, session, task_id, turn_id) = in_workspace(workspace.path(), || {
            let (mut session, task_id, turn_id) = running_turn();
            let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
                "test-model",
                provider,
                "local-test-key",
                1,
                format!("http://{address}/v1/chat/completions"),
            );
            let result = run_goal_turn_with_budget(
                &mut session,
                task_id,
                turn_id,
                "Reject multiple action calls",
                1,
                &mut source,
            );
            (result, session, task_id, turn_id)
        });
        let result = result.expect("invalid action response is recorded as a failed turn");
        assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
        assert_failed_turn(&result, &session, task_id, turn_id);
        assert!(!workspace.path().join("must-not-exist.txt").exists());
        assert_eq!(
            server
                .join()
                .expect("join compatible multiple-call mock")
                .len(),
            1
        );
    }
}

#[test]
fn compatible_chat_errors_never_echo_keys_or_response_bodies() {
    for (provider, key) in [
        (OpenAiCompatibleProvider::Cloudflare, "local-cf-error-token"),
        (OpenAiCompatibleProvider::Groq, "local-groq-error-key"),
        (
            OpenAiCompatibleProvider::OpenRouter,
            "local-openrouter-error-key",
        ),
        (
            OpenAiCompatibleProvider::DeepSeek,
            "local-deepseek-error-key",
        ),
        (OpenAiCompatibleProvider::Zai, "local-zai-error-key"),
        (mistral_provider(), "local-mistral-error-key"),
    ] {
        let (address, server) = mock_server(vec![MockResponse {
            status: 401,
            body: json!({"error":{"message":key}}).to_string(),
        }]);
        let endpoint = format!("http://{address}/v1/chat/completions");
        let mut source =
            OpenAiCompatibleDecisionSource::with_endpoint("test-model", provider, key, 1, endpoint);
        let action = source.choose_action("Test a failed request", &[]);
        let requests = server.join().expect("join compatible provider error mock");
        let recorded = String::from_utf8_lossy(&requests[0]);
        assert!(recorded.contains(key));
        assert!(!request_body(&requests[0]).to_string().contains(key));
        let WorkspaceAction::Unsupported { action } = action else {
            panic!("provider error should fail as an unsupported bounded action");
        };
        assert!(action.contains("401"));
        assert!(!action.contains(key));
        assert!(!action.contains("error\":{\"message"));
    }
}
