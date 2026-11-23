//! OpenAI-compatible provider profiles and request shapes.

use super::super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use serde_json::{json, Value};
use std::path::PathBuf;
pub(super) fn compatible_chat_function_call(name: &str, arguments: Value) -> Value {
    compatible_chat_function_call_with_raw_arguments(name, Value::String(arguments.to_string()))
}

pub(super) fn compatible_chat_function_call_with_raw_arguments(
    name: &str,
    arguments: Value,
) -> Value {
    json!({
        "id":"call_test",
        "type":"function",
        "function":{"name":name,"arguments":arguments}
    })
}

pub(super) fn compatible_chat_response(calls: Vec<Value>) -> String {
    json!({
        "choices":[{
            "finish_reason":"tool_calls",
            "message":{"role":"assistant","tool_calls":calls}
        }]
    })
    .to_string()
}

pub(super) fn run_compatible_profile(
    provider: OpenAiCompatibleProvider,
    key: &str,
    response: String,
) -> (WorkspaceAction, Vec<u8>) {
    let (address, server) = mock_server(vec![ok(response)]);
    let endpoint = format!("http://{address}/v1/chat/completions");
    let mut source =
        OpenAiCompatibleDecisionSource::with_endpoint("test-model", provider, key, 3, endpoint);
    let action = source.choose_action(
        "Create a result from the observation",
        &[crate::agent_loop::WorkspaceObservation::FileRead {
            path: PathBuf::from("fixture.txt"),
            contents: "mock-only workspace text".to_owned(),
        }],
    );
    let requests = server.join().expect("join compatible chat mock");
    (action, requests.into_iter().next().expect("one request"))
}

pub(super) fn mistral_provider() -> OpenAiCompatibleProvider {
    match ModelProvider::parse("mistral") {
        Some(ModelProvider::OpenAiCompatible(provider)) => provider,
        _ => panic!("Mistral must select the OpenAI-compatible profile"),
    }
}

pub(super) fn assert_compatible_chat_request_with_tool_choice(
    request: &[u8],
    key: &str,
    expected_tool_choice: &str,
) -> Value {
    let first_line = String::from_utf8_lossy(request)
        .lines()
        .next()
        .expect("request line")
        .to_owned();
    assert!(first_line.starts_with("POST /v1/chat/completions "));
    assert_eq!(
        request_header(request, "authorization"),
        Some(format!("Bearer {key}"))
    );
    let body = request_body(request);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["tool_choice"], expected_tool_choice);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["n"], 1);
    assert_eq!(body["stream"], false);
    assert!(body["messages"][0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("exactly one tool call")));
    assert!(body["messages"][1]["content"].as_str().is_some_and(|text| {
        text.contains("Create a result from the observation")
            && text.contains("mock-only workspace text")
            && text.contains("including this decision: 3")
    }));
    let tools = body["tools"].as_array().expect("compatible tools array");
    assert!(tools
        .iter()
        .any(|tool| tool["function"]["name"] == "read_file"));
    assert!(tools
        .iter()
        .any(|tool| tool["function"]["name"] == "complete_task"));
    assert!(tools.iter().all(|tool| {
        tool["function"]["parameters"]["additionalProperties"] == false
            && tool["function"]["parameters"]["required"].is_array()
    }));
    assert!(!body.to_string().contains(key));
    body
}

pub(super) fn assert_compatible_chat_request(request: &[u8], key: &str) -> Value {
    assert_compatible_chat_request_with_tool_choice(request, key, "required")
}

#[test]
fn compatible_chat_profiles_have_fixed_endpoints_and_cloudflare_path_validation() {
    let account_id = CloudflareAccountId::try_from("account-123").expect("safe account ID");
    assert_eq!(
        OpenAiCompatibleProvider::Cloudflare
            .endpoint(Some(&account_id))
            .expect("Cloudflare endpoint"),
        "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::CloudflareAiGateway
            .endpoint(Some(&account_id))
            .expect("Cloudflare AI Gateway endpoint"),
        "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::Groq
            .endpoint(None)
            .expect("Groq endpoint"),
        "https://api.groq.com/openai/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::OpenRouter
            .endpoint(None)
            .expect("OpenRouter endpoint"),
        "https://openrouter.ai/api/v1/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::DeepSeek
            .endpoint(None)
            .expect("DeepSeek endpoint"),
        "https://api.deepseek.com/chat/completions"
    );
    assert_eq!(
        OpenAiCompatibleProvider::Zai
            .endpoint(None)
            .expect("Z.AI endpoint"),
        "https://api.z.ai/api/paas/v4/chat/completions"
    );
    assert_eq!(
        mistral_provider().endpoint(None).expect("Mistral endpoint"),
        "https://api.mistral.ai/v1/chat/completions"
    );
    for invalid in [
        "",
        ".",
        "..",
        "account/other",
        "account\\other",
        "account?query",
        "%2f",
    ] {
        assert!(
            CloudflareAccountId::try_from(invalid).is_err(),
            "{invalid:?}"
        );
    }
    assert!(OpenAiCompatibleProvider::Cloudflare.endpoint(None).is_err());
    assert!(OpenAiCompatibleProvider::CloudflareAiGateway
        .endpoint(None)
        .is_err());
    for valid in ["default", "account-gateway_1"] {
        assert!(CloudflareGatewayId::try_from(valid).is_ok());
    }
    for invalid in ["", "gateway id", "gateway\nid", &"a".repeat(65)] {
        assert!(
            CloudflareGatewayId::try_from(invalid).is_err(),
            "{invalid:?}"
        );
    }
    assert!(mistral_provider().endpoint(Some(&account_id)).is_err());
}

#[test]
fn cloudflare_ai_gateway_sends_gateway_id_header() {
    let key = "local-cloudflare-gateway-token";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "complete_task",
        json!({"result":"done through AI Gateway"}),
    )]);
    let (address, server) = mock_server(vec![ok(response)]);
    let endpoint = format!("http://{address}/v1/chat/completions");
    let headers = vec![("cf-aig-gateway-id".to_owned(), "default".to_owned())];
    let mut source = OpenAiCompatibleDecisionSource::with_endpoint_and_headers(
        "openai/gpt-4.1",
        OpenAiCompatibleProvider::CloudflareAiGateway,
        key,
        3,
        endpoint,
        &headers,
    );

    assert_eq!(
        source.choose_action("complete the task", &[]),
        WorkspaceAction::Complete {
            result: "done through AI Gateway".to_owned()
        }
    );
    let request = server
        .join()
        .expect("join Cloudflare AI Gateway mock")
        .into_iter()
        .next()
        .expect("one gateway request");
    assert_eq!(
        request_header(&request, "authorization"),
        Some(format!("Bearer {key}"))
    );
    assert_eq!(
        request_header(&request, "cf-aig-gateway-id").as_deref(),
        Some("default")
    );
    assert_eq!(request_body(&request)["model"], "openai/gpt-4.1");
}

#[test]
fn cloudflare_chat_profile_uses_compatible_request_and_normalizes_one_action() {
    let key = "local-cloudflare-test-token";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "create_file",
        json!({"path":"summary.txt","contents":"from Cloudflare"}),
    )]);
    let (action, request) =
        run_compatible_profile(OpenAiCompatibleProvider::Cloudflare, key, response);
    assert_compatible_chat_request(&request, key);
    assert_eq!(request_header(&request, "cf-aig-gateway-id"), None);
    assert_eq!(
        action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: "from Cloudflare".to_owned(),
        }
    );
}

#[test]
fn groq_chat_profile_uses_compatible_request_and_normalizes_one_action() {
    let key = "local-groq-test-key";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "read_file",
        json!({"path":"plan.md"}),
    )]);
    let (action, request) = run_compatible_profile(OpenAiCompatibleProvider::Groq, key, response);
    assert_compatible_chat_request(&request, key);
    assert_eq!(
        action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("plan.md")
        }
    );
}

#[test]
fn openrouter_chat_profile_disables_provider_fallback_and_normalizes_one_action() {
    let key = "local-openrouter-test-key";
    let response = compatible_chat_response(vec![compatible_chat_function_call(
        "complete_task",
        json!({"result":"done"}),
    )]);
    let (action, request) =
        run_compatible_profile(OpenAiCompatibleProvider::OpenRouter, key, response);
    let body = assert_compatible_chat_request(&request, key);
    assert_eq!(body["provider"]["allow_fallbacks"], false);
    assert_eq!(
        action,
        WorkspaceAction::Complete {
            result: "done".to_owned()
        }
    );
}

#[test]
fn deepseek_and_zai_chat_profiles_use_auto_tool_choice_and_accept_supported_calls() {
    let deepseek_key = "local-deepseek-test-key";
    let deepseek_response = compatible_chat_response(vec![compatible_chat_function_call(
        "create_file",
        json!({"path":"deepseek.txt","contents":"from DeepSeek"}),
    )]);
    let (deepseek_action, deepseek_request) = run_compatible_profile(
        OpenAiCompatibleProvider::DeepSeek,
        deepseek_key,
        deepseek_response,
    );
    assert_compatible_chat_request_with_tool_choice(&deepseek_request, deepseek_key, "auto");
    assert_eq!(
        deepseek_action,
        WorkspaceAction::CreateFile {
            path: PathBuf::from("deepseek.txt"),
            contents: "from DeepSeek".to_owned(),
        }
    );

    let zai_key = "local-zai-test-key";
    let zai_response =
        compatible_chat_response(vec![compatible_chat_function_call_with_raw_arguments(
            "read_file",
            json!({"path":"zai.md"}),
        )]);
    let (zai_action, zai_request) =
        run_compatible_profile(OpenAiCompatibleProvider::Zai, zai_key, zai_response);
    assert_compatible_chat_request_with_tool_choice(&zai_request, zai_key, "auto");
    assert_eq!(
        zai_action,
        WorkspaceAction::ReadFile {
            path: PathBuf::from("zai.md")
        }
    );
}
