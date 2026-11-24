use super::{
    CloudflareAccountId, CloudflareGatewayId, OpenAiCompatibleCredentials, OpenAiCompatibleParts,
    OpenAiCompatibleProvider,
};

#[test]
fn built_in_provider_selectors_remain_complete_and_case_sensitive() {
    let selectors = [
        "ollama",
        "gemini",
        "openai",
        "xai",
        "anthropic",
        "cloudflare",
        "cloudflare-ai-gateway",
        "groq",
        "openrouter",
        "deepseek",
        "zai",
        "mistral",
    ];

    for selector in selectors {
        assert!(
            super::ModelProvider::parse(selector).is_some(),
            "missing existing provider selector {selector}"
        );
    }
    assert!(super::ModelProvider::parse("OpenAI").is_none());
    assert!(super::ModelProvider::parse("unknown-provider").is_none());
}

#[test]
fn decision_source_registry_rejects_duplicate_adapter_ids() {
    use super::{
        AdapterId, ProviderDecisionSource, ProviderDecisionSourceRegistry, ProviderSourceConfig,
    };

    fn unused_factory(
        _config: ProviderSourceConfig,
    ) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
        Err("duplicate factory must not be called")
    }

    let mut registry = ProviderDecisionSourceRegistry::new();
    registry
        .register(AdapterId::new("fake"), unused_factory)
        .expect("first registration succeeds");

    assert_eq!(
        registry.register(AdapterId::new("fake"), unused_factory),
        Err(super::ProviderDecisionSourceRegistryError::DuplicateAdapter(AdapterId::new("fake")))
    );
}

#[test]
fn decision_source_registry_rejects_missing_adapter_ids() {
    use super::{AdapterId, ProviderDecisionSourceRegistry, ProviderSourceConfig};

    let registry = ProviderDecisionSourceRegistry::new();
    let error = match registry.create_with_adapter(
        AdapterId::new("missing"),
        ProviderSourceConfig::new(
            super::ModelProvider::Ollama,
            "fake-model".to_owned(),
            1,
            "127.0.0.1:11434".parse().expect("socket address"),
            super::ProviderSourceCredentials::None,
        ),
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing adapter is rejected"),
    };

    assert_eq!(
        error,
        super::ProviderDecisionSourceRegistryError::MissingAdapter(AdapterId::new("missing"))
    );
}

#[test]
fn decision_source_registry_builds_and_forwards_a_fake_adapter() {
    use super::{
        AdapterId, ModelDecisionSource, ProviderDecisionSource, ProviderDecisionSourceRegistry,
        ProviderSourceConfig,
    };
    use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction, WorkspaceObservation};

    struct FakeDecisionSource;

    impl BoundedDecisionSource for FakeDecisionSource {
        fn choose_action(
            &mut self,
            _goal: &str,
            _observations: &[WorkspaceObservation],
        ) -> WorkspaceAction {
            WorkspaceAction::Cancel
        }
    }

    impl ProviderDecisionSource for FakeDecisionSource {
        fn supports_token_usage(&self) -> bool {
            true
        }

        fn take_token_usage(&mut self) -> Option<super::ModelTokenUsage> {
            Some(super::ModelTokenUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
                total_tokens: Some(5),
            })
        }
    }

    fn fake_source_factory(
        _config: ProviderSourceConfig,
    ) -> Result<Box<dyn ProviderDecisionSource>, &'static str> {
        Ok(Box::new(FakeDecisionSource))
    }

    let adapter_id = AdapterId::new("fake");
    let mut registry = ProviderDecisionSourceRegistry::new();
    registry
        .register(adapter_id, fake_source_factory)
        .expect("fake adapter registration succeeds");
    let mut source: ModelDecisionSource = registry
        .create_with_adapter(
            adapter_id,
            ProviderSourceConfig::new(
                super::ModelProvider::Ollama,
                "fake-model".to_owned(),
                1,
                "127.0.0.1:11434".parse().expect("socket address"),
                super::ProviderSourceCredentials::None,
            ),
        )
        .expect("fake adapter is built");

    assert_eq!(source.adapter_id(), adapter_id);
    assert_eq!(
        source.choose_action("ignored", &[]),
        WorkspaceAction::Cancel
    );
    assert!(source.supports_token_usage());
    assert_eq!(
        source.take_token_usage(),
        Some(super::ModelTokenUsage {
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            total_tokens: Some(5),
        })
    );
}

#[test]
fn decision_source_registry_builds_each_builtin_protocol_adapter() {
    use super::{
        AdapterId, ModelDecisionSource, ModelProvider, OpenAiCompatibleCredentials,
        OpenAiCompatibleProvider, ProviderDecisionSourceRegistry, ProviderSourceConfig,
        ProviderSourceCredentials,
    };

    let registry = ProviderDecisionSourceRegistry::with_builtins();
    let socket = "127.0.0.1:11434".parse().expect("socket address");
    let cases = [
        (
            ModelProvider::Ollama,
            ProviderSourceCredentials::None,
            AdapterId::OLLAMA,
        ),
        (
            ModelProvider::Gemini,
            ProviderSourceCredentials::HostedApiKey("local-test-key".to_owned()),
            AdapterId::GEMINI,
        ),
        (
            ModelProvider::OpenAi,
            ProviderSourceCredentials::HostedApiKey("local-test-key".to_owned()),
            AdapterId::RESPONSES,
        ),
        (
            ModelProvider::Xai,
            ProviderSourceCredentials::HostedApiKey("local-test-key".to_owned()),
            AdapterId::RESPONSES,
        ),
        (
            ModelProvider::Anthropic,
            ProviderSourceCredentials::HostedApiKey("local-test-key".to_owned()),
            AdapterId::ANTHROPIC,
        ),
        (
            ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Groq),
            ProviderSourceCredentials::OpenAiCompatible(OpenAiCompatibleCredentials::Groq {
                api_key: "local-test-key".to_owned(),
            }),
            AdapterId::OPENAI_COMPATIBLE,
        ),
    ];

    for (provider, credentials, expected_adapter) in cases {
        let source: ModelDecisionSource = registry
            .create(ProviderSourceConfig::new(
                provider,
                "test-model".to_owned(),
                2,
                socket,
                credentials,
            ))
            .expect("built-in factory creates its source");
        assert_eq!(source.adapter_id(), expected_adapter);
    }
}

#[test]
fn decision_source_registry_rejects_credentials_for_a_different_provider() {
    use super::{
        ModelProvider, OpenAiCompatibleCredentials, OpenAiCompatibleProvider,
        ProviderDecisionSourceRegistry, ProviderDecisionSourceRegistryError, ProviderSourceConfig,
        ProviderSourceCredentials,
    };

    let registry = ProviderDecisionSourceRegistry::with_builtins();
    let error = match registry.create(ProviderSourceConfig::new(
        ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Groq),
        "test-model".to_owned(),
        1,
        "127.0.0.1:11434".parse().expect("socket address"),
        ProviderSourceCredentials::OpenAiCompatible(OpenAiCompatibleCredentials::OpenRouter {
            api_key: "local-test-key".to_owned(),
        }),
    )) {
        Err(error) => error,
        Ok(_) => panic!("credentials cannot select a different compatible provider"),
    };

    assert_eq!(
        error,
        ProviderDecisionSourceRegistryError::BuildFailed(
            "compatible provider credentials do not match the selected provider"
        )
    );
}

#[test]
fn cloudflare_ai_gateway_credentials_add_the_gateway_header() {
    let credentials = OpenAiCompatibleCredentials::CloudflareAiGateway {
        account_id: CloudflareAccountId::try_from("account-123").expect("valid account ID"),
        api_token: "local-test-token".to_owned(),
        gateway_id: CloudflareGatewayId::try_from("default").expect("valid gateway ID"),
    };

    let OpenAiCompatibleParts {
        provider,
        endpoint,
        api_key: api_token,
        extra_headers: headers,
    } = credentials.into_parts().expect("valid gateway credentials");

    assert_eq!(provider, OpenAiCompatibleProvider::CloudflareAiGateway);
    assert_eq!(
        endpoint,
        "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1/chat/completions"
    );
    assert_eq!(api_token, "local-test-token");
    assert_eq!(
        headers,
        vec![("cf-aig-gateway-id".to_owned(), "default".to_owned())]
    );
}

#[test]
fn decision_sources_carry_extension_declarations_into_requests() {
    use crate::extensions::{
        authorized_declarations, CapabilityContext, CapabilityGrant, ExtensionAuthority,
        ExtensionDescriptor, ExtensionEffect, ExtensionTool, EXTENSION_CONTRACT_VERSION,
    };
    use uuid::Uuid;

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
        parameters: serde_json::json!({"type":"object","properties":{}}),
        grant: &grant,
    }];
    let declarations = authorized_declarations(&tools, context).expect("authorized");
    let endpoint = "http://127.0.0.1:1".to_owned();

    let ollama = super::OllamaDecisionSource::with_endpoint_and_extensions(
        "test-model",
        1,
        "127.0.0.1:1".parse().unwrap(),
        &declarations,
    );
    let gemini = super::GeminiDecisionSource::with_endpoint_and_extensions(
        "test-model",
        "key".to_owned(),
        1,
        endpoint.clone(),
        &declarations,
    );
    let responses = super::ResponsesDecisionSource::with_endpoint_and_extensions(
        "test-model",
        super::ResponsesProvider::OpenAi,
        "key",
        1,
        endpoint.clone(),
        &declarations,
    );
    let anthropic = super::AnthropicDecisionSource::with_endpoint_and_extensions(
        "test-model",
        "key",
        1,
        endpoint.clone(),
        &declarations,
    );
    let compatible = super::OpenAiCompatibleDecisionSource::with_endpoint_and_extensions(
        "test-model",
        OpenAiCompatibleProvider::Groq,
        "key",
        1,
        endpoint,
        &declarations,
    );

    let requests = [
        crate::brain::build_chat_request_with_extensions(
            "test-model",
            "goal",
            &[],
            1,
            None,
            &ollama.extension_declarations,
        ),
        crate::brain::build_gemini_request_with_extensions(
            "test-model",
            "goal",
            &[],
            1,
            None,
            &gemini.core.extension_declarations,
        ),
        crate::brain::build_responses_request_with_extensions(
            "test-model",
            super::ModelProvider::OpenAi,
            "goal",
            &[],
            1,
            None,
            &responses.core.extension_declarations,
        ),
        crate::brain::build_anthropic_request_with_extensions(
            "test-model",
            "goal",
            &[],
            1,
            None,
            &anthropic.core.extension_declarations,
        ),
        super::build_compatible_chat_request_with_extensions(
            "test-model",
            OpenAiCompatibleProvider::Groq,
            "goal",
            &[],
            1,
            None,
            &compatible.core.extension_declarations,
        ),
    ];
    for request in requests {
        let body = String::from_utf8(request.expect("request")).expect("utf8");
        assert!(body.contains("external_read"), "{body}");
    }
}

/// Exercise the real HTTP/SSE transport, assembly and action validator
/// without contacting a provider or executing the returned workspace action.
fn assert_responses_stream_validates_action(provider: super::ResponsesProvider) {
    use super::ResponsesDecisionSource;
    use crate::agent_loop::WorkspaceAction;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
    let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut request = [0_u8; 16384];
        let count = socket.read(&mut request).expect("read request");
        let received = String::from_utf8_lossy(&request[..count]);
        assert!(received.contains("POST "));
        let frames = [
            (
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
                }),
            ),
            (
                "response.function_call_arguments.delta",
                serde_json::json!({
                    "output_index": 0, "delta": r#"{"path":"README.md"}"#
                }),
            ),
            (
                "response.function_call_arguments.done",
                serde_json::json!({
                    "output_index": 0
                }),
            ),
            (
                "response.output_item.done",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
                }),
            ),
            (
                "response.completed",
                serde_json::json!({
                    "response": {"status": "completed"}
                }),
            ),
        ];
        let mut body = String::new();
        for (event, data) in frames {
            body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        socket.write_all(response.as_bytes()).expect("send SSE");
    });

    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model",
        provider,
        "local-test-key",
        2,
        endpoint,
    );
    let action = source
        .choose_with_streaming_for_test("read README", &[], None, true)
        .expect("completed stream produces validated action");
    assert!(matches!(action, WorkspaceAction::ReadFile { .. }));
    let response = serde_json::json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": "call_a",
            "name": "read_file",
            "arguments": r#"{"path":"README.md"}"#
        }]
    });
    let nonstreamed =
        super::parse_responses_response(response.to_string().as_bytes(), provider.model_provider())
            .expect("equivalent non-streaming decision");
    assert_eq!(format!("{action:?}"), format!("{nonstreamed:?}"));
    server.join().expect("server completed");
}

#[test]
fn anthropic_text_delta_excludes_tool_argument_chunks() {
    use crate::streaming::{provider_text_delta, SseFrame};
    let text = SseFrame {
        event: Some("content_block_delta".into()),
        data: serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello"}
        })
        .to_string(),
    };
    let tool = SseFrame {
        event: Some("content_block_delta".into()),
        data: serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "secret"}
        })
        .to_string(),
    };
    assert_eq!(provider_text_delta(&text).as_deref(), Some("hello"));
    assert_eq!(provider_text_delta(&tool), None);
}

#[test]
fn openai_responses_stream_validates_completed_action() {
    assert_responses_stream_validates_action(super::ResponsesProvider::OpenAi);
}

#[test]
fn xai_responses_stream_validates_completed_action() {
    assert_responses_stream_validates_action(super::ResponsesProvider::Xai);
}

/// A transport that closes before response.completed must never yield an action.
fn assert_responses_stream_rejects_truncated_tool_call(provider: super::ResponsesProvider) {
    use super::ResponsesDecisionSource;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
    let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept request");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        let mut request = [0_u8; 16384];
        let _read = socket.read(&mut request).expect("read request");
        let body = format!(
            "event: response.output_item.added\ndata: {}\n\n",
            serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "read_file"}
            })
        );
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        socket
            .write_all(response.as_bytes())
            .expect("send partial SSE");
    });
    let mut source = ResponsesDecisionSource::with_endpoint(
        "test-model",
        provider,
        "local-test-key",
        2,
        endpoint,
    );
    assert!(source
        .choose_with_streaming_for_test("read README", &[], None, true)
        .is_err());
    server.join().expect("server completed");
}

#[test]
fn openai_responses_stream_rejects_truncated_tool_call() {
    assert_responses_stream_rejects_truncated_tool_call(super::ResponsesProvider::OpenAi);
}

#[test]
fn xai_responses_stream_rejects_truncated_tool_call() {
    assert_responses_stream_rejects_truncated_tool_call(super::ResponsesProvider::Xai);
}

#[test]
fn provider_capability_registry_covers_every_cli_provider() {
    use super::{DecisionProtocol, ProviderDecisionSourceRegistry};
    let source_registry = ProviderDecisionSourceRegistry::with_builtins();
    for descriptor in super::provider_descriptors() {
        let capabilities = descriptor.provider.capabilities();
        assert!(
            source_registry.contains(descriptor.adapter_id),
            "{} has a source factory",
            descriptor.selector
        );
        assert_eq!(
            capabilities.protocol, descriptor.protocol,
            "{} protocol",
            descriptor.selector
        );
        assert_eq!(
            capabilities.native_sse_decisions, descriptor.native_sse_decisions,
            "{} native SSE metadata",
            descriptor.selector
        );
        assert_eq!(
            descriptor.native_sse_decisions,
            matches!(
                descriptor.protocol,
                DecisionProtocol::Responses | DecisionProtocol::AnthropicMessages
            ),
            "{} protocol feature",
            descriptor.selector
        );
    }
    assert!(super::ModelProvider::parse("unknown-provider").is_none());
}

#[test]
fn mistral_credentials_bind_to_the_fixed_endpoint() {
    let key = "local-mistral-test-key";
    let OpenAiCompatibleParts {
        provider,
        endpoint,
        api_key,
        extra_headers: headers,
    } = OpenAiCompatibleCredentials::Mistral {
        api_key: key.to_owned(),
    }
    .into_parts()
    .expect("Mistral credentials use a fixed endpoint");

    assert_eq!(provider, OpenAiCompatibleProvider::Mistral);
    assert_eq!(endpoint, "https://api.mistral.ai/v1/chat/completions");
    assert_eq!(api_key, key);
    assert!(headers.is_empty());
}
