//! Request builders keep their prompt sections and size limits.

use super::*;
use crate::agent_loop::WorkspaceObservation;
use crate::extensions::{
    authorized_declarations, CapabilityContext, CapabilityGrant, ExtensionAuthority,
    ExtensionDescriptor, ExtensionEffect, ExtensionTool, EXTENSION_CONTRACT_VERSION,
};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;
#[test]
fn model_name_validation_rejects_cloud_models() {
    assert!(validate_model_name_for_provider("qwen3", ModelProvider::Ollama).is_ok());
    assert!(validate_model_name_for_provider("gemma4:cloud", ModelProvider::Ollama).is_err());
    assert!(validate_model_name_for_provider("gemma4:cloud ", ModelProvider::Ollama).is_err());
    assert!(
        validate_model_name_for_provider("namespace/model:cloud", ModelProvider::Ollama).is_err()
    );
    assert!(
        validate_model_name_for_provider("qwen3-coder:480b-cloud", ModelProvider::Ollama).is_err()
    );
    assert!(validate_model_name_for_provider("gpt-oss:20b-cloud", ModelProvider::Ollama).is_err());
    assert!(
        validate_model_name_for_provider("qwen3-coder:480b-Cloud", ModelProvider::Ollama).is_err()
    );
}

#[test]
fn all_request_builders_share_prompt_and_keep_existing_size_limits() {
    let build_requests = |goal: &str, observations: &[crate::agent_loop::WorkspaceObservation]| {
        [
            build_chat_request("test-model", goal, observations, 3, None),
            build_responses_request(
                "test-model",
                ModelProvider::OpenAi,
                goal,
                observations,
                3,
                None,
            ),
            build_anthropic_request("test-model", goal, observations, 3, None),
            build_gemini_request("gemini-2.5-flash", goal, observations, 3, None),
            crate::brain::providers::build_compatible_chat_request(
                "test-model",
                OpenAiCompatibleProvider::Groq,
                goal,
                observations,
                3,
                None,
            ),
        ]
    };
    let observations = [crate::agent_loop::WorkspaceObservation::FileRead {
        path: PathBuf::from("note.txt"),
        contents: "local fixture".to_owned(),
    }];
    let requests = build_requests("Summarize the note", &observations).map(|request| {
        serde_json::from_slice::<Value>(&request.expect("build request"))
            .expect("decode offline request")
    });
    let expected_prompt = concat!(
        "Task goal:\nSummarize the note\n\nAction slots remaining, including this decision: 3. The task fails if the action budget runs out before completion.\n\nPrior workspace observations (JSON):\n",
        r#"[{"contents":"local fixture","path":"note.txt","type":"file_read"}]"#
    );
    let prompts = [
        requests[0]["messages"][1]["content"].as_str(),
        requests[1]["input"].as_str(),
        requests[2]["messages"][0]["content"].as_str(),
        requests[3]["contents"][0]["parts"][0]["text"].as_str(),
        requests[4]["messages"][1]["content"].as_str(),
    ];
    assert_eq!(prompts, [Some(expected_prompt); 5]);

    let oversized_observation = [crate::agent_loop::WorkspaceObservation::FileRead {
        path: PathBuf::from("note.txt"),
        contents: "x".repeat(2 * 1024 * 1024 + 1),
    }];
    for result in build_requests("goal", &oversized_observation) {
        assert!(matches!(result, Err(ModelError::ObservationsTooLarge)));
    }

    let oversized_goal = "g".repeat(MAX_MODEL_REQUEST_BYTES + 1);
    for result in build_requests(&oversized_goal, &[]) {
        assert!(matches!(result, Err(ModelError::RequestTooLarge)));
    }
}

#[test]
fn compatible_request_rejects_oversized_retrieval_context() {
    let context = serde_json::json!({
        "bytes": 0,
        "modules": [],
        "padding": "x".repeat(
            crate::harness::context::MAX_REPOSITORY_CONTEXT_JSON_BYTES + 1
        )
    });
    let result = crate::brain::providers::build_compatible_chat_request_with_retrieval_context(
        "openai/gpt-4.1",
        OpenAiCompatibleProvider::CloudflareAiGateway,
        "same task goal",
        &[],
        3,
        crate::brain::providers::CompatibleRequestContext {
            project_guidance: None,
            extension_declarations: &[],
            repository_context: Some(&context),
        },
    );
    assert!(matches!(result, Err(ModelError::RepositoryContextTooLarge)));
}

#[test]
fn every_model_provider_keeps_root_guidance_in_the_same_separate_section() {
    let guidance = "Use the project's existing error style.";
    let observations = [WorkspaceObservation::FileRead {
        path: PathBuf::from("note.txt"),
        contents: "observed text".to_owned(),
    }];
    let requests = [
        serde_json::from_slice::<Value>(
            &build_chat_request("fixture", "Read the note", &observations, 4, Some(guidance))
                .expect("build Ollama request"),
        )
        .expect("decode Ollama request"),
        serde_json::from_slice::<Value>(
            &build_responses_request(
                "fixture",
                ModelProvider::OpenAi,
                "Read the note",
                &observations,
                4,
                Some(guidance),
            )
            .expect("build Responses request"),
        )
        .expect("decode Responses request"),
        serde_json::from_slice::<Value>(
            &build_anthropic_request("fixture", "Read the note", &observations, 4, Some(guidance))
                .expect("build Anthropic request"),
        )
        .expect("decode Anthropic request"),
        serde_json::from_slice::<Value>(
            &build_gemini_request(
                "gemini-2.5-flash",
                "Read the note",
                &observations,
                4,
                Some(guidance),
            )
            .expect("build Gemini request"),
        )
        .expect("decode Gemini request"),
        serde_json::from_slice::<Value>(
            &crate::brain::providers::build_compatible_chat_request(
                "fixture",
                OpenAiCompatibleProvider::Groq,
                "Read the note",
                &observations,
                4,
                Some(guidance),
            )
            .expect("build compatible request"),
        )
        .expect("decode compatible request"),
    ];
    let prompts = [
        requests[0]["messages"][1]["content"].as_str().unwrap(),
        requests[1]["input"].as_str().unwrap(),
        requests[2]["messages"][0]["content"].as_str().unwrap(),
        requests[3]["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap(),
        requests[4]["messages"][1]["content"].as_str().unwrap(),
    ];

    for prompt in prompts {
        assert!(prompt.contains("Task goal:\nRead the note"));
        assert!(prompt.contains("Action slots remaining, including this decision: 4"));
        assert!(prompt.contains("observed text"));
        assert!(prompt.contains(
            "Root AGENTS.md project guidance (lower priority):\nUse the project's existing error style."
        ));
        assert_eq!(prompt.matches(guidance).count(), 1);
    }
}

#[test]
fn extension_request_builders_advertise_authorized_declarations() {
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
        description: "Read an external path",
        parameters: serde_json::json!({"type":"object","properties":{}}),
        grant: &grant,
    }];
    let declarations = authorized_declarations(
        &tools,
        CapabilityContext {
            task_id,
            generation: 1,
            task_authorized: true,
        },
    )
    .expect("authorized declaration");
    let observations = [];
    let requests = [
        build_chat_request_with_extensions(
            "test-model",
            "goal",
            &observations,
            3,
            None,
            &declarations,
        )
        .expect("chat request"),
        build_responses_request_with_extensions(
            "test-model",
            ModelProvider::OpenAi,
            "goal",
            &observations,
            3,
            None,
            &declarations,
        )
        .expect("responses request"),
        build_anthropic_request_with_extensions(
            "test-model",
            "goal",
            &observations,
            3,
            None,
            &declarations,
        )
        .expect("anthropic request"),
        build_gemini_request_with_extensions(
            "gemini-2.5-flash",
            "goal",
            &observations,
            3,
            None,
            &declarations,
        )
        .expect("gemini request"),
        build_compatible_chat_request_with_extensions(
            "test-model",
            OpenAiCompatibleProvider::Groq,
            "goal",
            &observations,
            3,
            None,
            &declarations,
        )
        .expect("compatible request"),
    ];
    let parsed = requests.map(|request| {
        serde_json::from_slice::<Value>(&request).expect("decode extension request")
    });
    let names = [
        parsed[0]["tools"]
            .as_array()
            .expect("chat tools")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>(),
        parsed[1]["tools"]
            .as_array()
            .expect("responses tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        parsed[2]["tools"]
            .as_array()
            .expect("anthropic tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        parsed[3]["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("gemini declarations")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        parsed[4]["tools"]
            .as_array()
            .expect("compatible tools")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>(),
    ];
    for names in names {
        assert!(names.contains(&"external_read"));
    }

    let chat_tool = parsed[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "external_read")
        .unwrap();
    let responses_tool = parsed[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "external_read")
        .unwrap();
    let anthropic_tool = parsed[2]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "external_read")
        .unwrap();
    let gemini_tool = parsed[3]["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "external_read")
        .unwrap();
    let compatible_tool = parsed[4]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "external_read")
        .unwrap();
    assert_eq!(
        chat_tool["function"]["parameters"],
        serde_json::json!({"type":"object","properties":{}})
    );
    assert_eq!(
        responses_tool["parameters"],
        serde_json::json!({"type":"object","properties":{}})
    );
    assert_eq!(
        anthropic_tool["input_schema"],
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
    );
    assert_eq!(
        gemini_tool["parameters"],
        serde_json::json!({"type":"OBJECT","properties":{}})
    );
    assert_eq!(
        compatible_tool["function"]["parameters"],
        serde_json::json!({"type":"object","properties":{}})
    );
}
