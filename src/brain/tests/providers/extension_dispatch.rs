//! Provider tool calls reach the bounded Harness dispatcher with validated arguments.

use super::super::*;
use super::anthropic::anthropic_tool_use;
use super::compatible::{compatible_chat_function_call, compatible_chat_response};
use super::gemini::{gemini_function_call, gemini_response};
use super::openai::openai_function_call;
use crate::harness::execution::{
    run_goal_turn_with_budget_and_extensions, BoundedDecisionSource, GoalTurnStopReason,
    RegisteredExtensionWithArguments, TurnExtensions,
};
use crate::task_lifecycle::TaskSession;
use crate::tools::capability_policy::{CapabilityContext, CapabilityGrant};
use crate::tools::extension_contract::{
    ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};
use serde_json::Value;
use uuid::Uuid;

enum Expected {
    Success(&'static str),
    Failure,
}

fn assert_extension_call<S: BoundedDecisionSource>(
    session: &mut TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    source: &mut S,
    expected: Expected,
) {
    let grant = CapabilityGrant {
        task_id,
        namespace: "external".into(),
        operation: "read".into(),
        effect: ExtensionEffect::ReadWorkspace,
        generation: 1,
        host_approved: false,
    };
    let registry = [RegisteredExtensionWithArguments {
        tool: crate::brain::provider_bridge::ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read an external path",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            grant: &grant,
        },
        execute: |arguments: &Value| {
            arguments["path"]
                .as_str()
                .expect("validated path")
                .to_owned()
        },
    }];
    let extensions = TurnExtensions {
        registry: &registry,
        context: CapabilityContext {
            task_id,
            generation: 1,
            task_authorized: true,
        },
    };
    let result = run_goal_turn_with_budget_and_extensions(
        session,
        task_id,
        turn_id,
        "Use an extension",
        2,
        source,
        Some(&extensions),
    )
    .expect("extension result is recorded in the turn");
    match expected {
        Expected::Success(result_text) => {
            assert_eq!(result.stop_reason(), GoalTurnStopReason::Completed);
            assert_eq!(
                result.outcome(),
                Some(&TurnOutcome::Success {
                    result: "done".to_owned()
                })
            );
            assert!(result.observations().iter().any(|observation| {
                matches!(
                    observation,
                    crate::agent_loop::WorkspaceObservation::ExtensionCompleted { name, result }
                        if name == "external_read" && result == result_text
                )
            }));
        }
        Expected::Failure => {
            assert_eq!(result.stop_reason(), GoalTurnStopReason::ActionFailed);
            assert!(matches!(
                result.outcome(),
                Some(TurnOutcome::Failure { .. })
            ));
        }
    }
}

#[test]
fn ollama_extension_arguments_reach_harness_dispatch() {
    let (address, server) = mock_server(vec![
        ok(tool_response(
            "external_read",
            json!({"path": "ollama.txt"}),
        )),
        ok(tool_response("complete_task", json!({"result": "done"}))),
    ]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OllamaDecisionSource::with_endpoint("test-model", 2, address);
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Success("ollama.txt"),
    );
    server.join().expect("join Ollama mock server");
}

#[test]
fn ollama_extension_without_required_arguments_fails_closed() {
    let (address, server) = mock_server(vec![ok(tool_response("external_read", json!({})))]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OllamaDecisionSource::with_endpoint("test-model", 2, address);
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Ollama mock server");
}

#[test]
fn openai_compatible_extension_arguments_reach_harness_dispatch() {
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "external_read",
        json!({"path": "compatible.txt"}),
    )]);
    let completion = compatible_chat_response(vec![compatible_chat_function_call(
        "complete_task",
        json!({"result": "done"}),
    )]);
    let (address, server) = mock_server(vec![ok(response), ok(completion)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
        "test-model",
        OpenAiCompatibleProvider::DeepSeek,
        "local-key",
        2,
        format!("http://{address}/v1/chat/completions"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Success("compatible.txt"),
    );
    server.join().expect("join OpenAI-compatible mock server");
}

#[test]
fn openai_compatible_extension_without_required_arguments_fails_closed() {
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "external_read",
        json!({}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
        "test-model",
        OpenAiCompatibleProvider::DeepSeek,
        "local-key",
        2,
        format!("http://{address}/v1/chat/completions"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join OpenAI-compatible mock server");
}

#[test]
fn responses_extension_arguments_reach_harness_dispatch() {
    let response = json!({
        "status": "completed",
        "output": [openai_function_call("external_read", json!({"path": "responses.txt"}))]
    })
    .to_string();
    let completion = json!({
        "status": "completed",
        "output": [openai_function_call("complete_task", json!({"result": "done"}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response), ok(completion)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model",
        ResponsesProvider::OpenAi,
        "local-key",
        2,
        format!("http://{address}/v1/responses"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Success("responses.txt"),
    );
    server.join().expect("join Responses mock server");
}

#[test]
fn responses_extension_without_required_arguments_fails_closed() {
    let response = json!({
        "status": "completed",
        "output": [openai_function_call("external_read", json!({}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model",
        ResponsesProvider::OpenAi,
        "local-key",
        2,
        format!("http://{address}/v1/responses"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Responses mock server");
}

#[test]
fn anthropic_extension_arguments_reach_harness_dispatch() {
    let response = json!({
        "stop_reason": "tool_use",
        "content": [anthropic_tool_use("external_read", json!({"path": "anthropic.txt"}))]
    })
    .to_string();
    let completion = json!({
        "stop_reason": "tool_use",
        "content": [anthropic_tool_use("complete_task", json!({"result": "done"}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response), ok(completion)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = AnthropicDecisionSource::with_endpoint(
        "test-model:cloud",
        "local-key",
        2,
        format!("http://{address}/v1/messages"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Success("anthropic.txt"),
    );
    server.join().expect("join Anthropic mock server");
}

#[test]
fn anthropic_extension_without_required_arguments_fails_closed() {
    let response = json!({
        "stop_reason": "tool_use",
        "content": [anthropic_tool_use("external_read", json!({}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = AnthropicDecisionSource::with_endpoint(
        "test-model:cloud",
        "local-key",
        2,
        format!("http://{address}/v1/messages"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Anthropic mock server");
}

#[test]
fn gemini_extension_arguments_reach_harness_dispatch() {
    let response = gemini_response(vec![gemini_function_call(
        "external_read",
        json!({"path": "gemini.txt"}),
    )]);
    let completion = gemini_response(vec![gemini_function_call(
        "complete_task",
        json!({"result": "done"}),
    )]);
    let (address, server) = mock_server(vec![ok(response), ok(completion)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = GeminiDecisionSource::with_endpoint(
        "test-model",
        "local-key",
        2,
        format!("http://{address}/v1beta/models/test-model:generateContent"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Success("gemini.txt"),
    );
    server.join().expect("join Gemini mock server");
}

#[test]
fn gemini_extension_without_required_arguments_fails_closed() {
    let response = gemini_response(vec![gemini_function_call("external_read", json!({}))]);
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = GeminiDecisionSource::with_endpoint(
        "test-model",
        "local-key",
        2,
        format!("http://{address}/v1beta/models/test-model:generateContent"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Gemini mock server");
}

#[test]
fn ollama_unregistered_extension_name_fails_closed() {
    let response = tool_response("external_missing", json!({"path": "missing.txt"}));
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OllamaDecisionSource::with_endpoint("test-model", 2, address);
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Ollama mock server");
}

#[test]
fn openai_compatible_unregistered_extension_name_fails_closed() {
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "external_missing",
        json!({"path": "missing.txt"}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = OpenAiCompatibleDecisionSource::with_endpoint(
        "test-model",
        OpenAiCompatibleProvider::DeepSeek,
        "local-key",
        2,
        format!("http://{address}/v1/chat/completions"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join OpenAI-compatible mock server");
}

#[test]
fn responses_unregistered_extension_name_fails_closed() {
    let response = json!({
        "status": "completed",
        "output": [openai_function_call("external_missing", json!({"path": "missing.txt"}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model",
        ResponsesProvider::OpenAi,
        "local-key",
        2,
        format!("http://{address}/v1/responses"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Responses mock server");
}

#[test]
fn anthropic_unregistered_extension_name_fails_closed() {
    let response = json!({
        "stop_reason": "tool_use",
        "content": [anthropic_tool_use("external_missing", json!({"path": "missing.txt"}))]
    })
    .to_string();
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = AnthropicDecisionSource::with_endpoint(
        "test-model:cloud",
        "local-key",
        2,
        format!("http://{address}/v1/messages"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Anthropic mock server");
}

#[test]
fn gemini_unregistered_extension_name_fails_closed() {
    let response = gemini_response(vec![gemini_function_call(
        "external_missing",
        json!({"path": "missing.txt"}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let (mut session, task_id, turn_id) = running_turn();
    let mut source = GeminiDecisionSource::with_endpoint(
        "test-model",
        "local-key",
        2,
        format!("http://{address}/v1beta/models/test-model:generateContent"),
    );
    assert_extension_call(
        &mut session,
        task_id,
        turn_id,
        &mut source,
        Expected::Failure,
    );
    server.join().expect("join Gemini mock server");
}
