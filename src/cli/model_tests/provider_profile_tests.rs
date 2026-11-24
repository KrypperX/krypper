use super::super::model_selection::{
    configured_model_selection, prepare_model_source_for_selection, ProviderSelection,
};
use super::super::provider_profiles::{
    add_profile, replace_profile, ProfileProtocol, ProviderProfile,
};
use super::super::settings::{save_credentials, CredentialStore};
use super::super::task::run_task_with_options;
use super::{
    in_workspace, mock_server, read_request, redirecting_mock_server, request_header, request_json,
    task_args, MockResponse, RestoreEnvironmentVariable, TempDir, PROVIDER_ENV_TEST_LOCK,
};
use crate::agent_loop::WorkspaceAction;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::net::SocketAddr;

fn profile_tool_response(name: &str, arguments: Value) -> String {
    json!({
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_profile_test",
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            }
        }]
    })
    .to_string()
}

#[test]
fn task_cli_uses_per_user_profiles_env_precedence_and_no_config_selection() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
    let config_home = TempDir::new();
    let config_home_value = config_home.path().to_string_lossy().into_owned();
    let _restore_config_home =
        RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", &config_home_value);
    let _restore_api_key = RestoreEnvironmentVariable::remove("KRYPPER_PROVIDER_MOCK_API_KEY");
    let _restore_config = RestoreEnvironmentVariable::remove("KRYPPER_CONFIG");

    let (endpoint, server) = mock_server(vec![
        MockResponse {
            status: 200,
            body: profile_tool_response("complete_task", json!({"result":"profile complete"})),
        },
        MockResponse {
            status: 200,
            body: profile_tool_response("complete_task", json!({"result":"profile complete"})),
        },
    ]);
    let profile_path = config_home.path().join("krypper/provider-profiles.json");
    add_profile(
        &profile_path,
        ProviderProfile {
            id: "mock".to_owned(),
            label: "Mock compatible server".to_owned(),
            enabled: true,
            protocol: ProfileProtocol::OpenAiCompatibleChat,
            endpoint: format!("http://{endpoint}/v1/chat/completions"),
            credential_ref: Some("profile:mock:api-key".to_owned()),
        },
    )
    .expect("write trusted per-user profile");
    let mut credentials = CredentialStore::empty();
    credentials.insert(
        "KRYPPER_PROVIDER_MOCK_API_KEY",
        "saved-profile-key".to_owned(),
    );
    save_credentials(
        &config_home.path().join("krypper/credentials.json"),
        &credentials,
    )
    .expect("save profile credential fixture");

    let workspace = TempDir::new();
    let mut args = task_args("1");
    args.extend([OsString::from("--provider"), OsString::from("mock")]);
    std::env::set_var("KRYPPER_PROVIDER_MOCK_API_KEY", "environment-profile-key");
    let (code, output, error) = in_workspace(workspace.path(), || {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut error = Vec::new();
        let code = run_task_with_options(
            &args,
            &mut input,
            &mut output,
            &mut error,
            SocketAddr::from(([127, 0, 0, 1], 0)),
            true,
        )
        .expect("run task through per-user provider profile");
        (code, output, error)
    });
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&error));
    assert!(String::from_utf8_lossy(&output).contains("Task outcome: succeeded"));
    assert!(!String::from_utf8_lossy(&output).contains("environment-profile-key"));
    assert!(!String::from_utf8_lossy(&error).contains("environment-profile-key"));
    let first_request_args = args.clone();
    let mut no_config_args = first_request_args;
    no_config_args.push(OsString::from("--no-config"));
    let (no_config_code, no_config_output, no_config_error) =
        in_workspace(workspace.path(), || {
            let mut input = Cursor::new(Vec::<u8>::new());
            let mut output = Vec::new();
            let mut error = Vec::new();
            let code = run_task_with_options(
                &no_config_args,
                &mut input,
                &mut output,
                &mut error,
                SocketAddr::from(([127, 0, 0, 1], 0)),
                true,
            )
            .expect("explicit profile remains usable with --no-config");
            (code, output, error)
        });
    assert_eq!(
        no_config_code,
        0,
        "{}",
        String::from_utf8_lossy(&no_config_error)
    );
    assert!(String::from_utf8_lossy(&no_config_output).contains("Task outcome: succeeded"));
    let requests = server.join().expect("join profile server");
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(String::from_utf8_lossy(request).starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert_eq!(
            request_header(request, "Authorization").as_deref(),
            Some("Bearer environment-profile-key")
        );
        assert_eq!(request_json(request)["model"], "mock-model");
        assert_eq!(request_json(request)["tool_choice"], "required");
    }

    std::env::remove_var("KRYPPER_PROVIDER_MOCK_API_KEY");
    assert!(prepare_model_source_for_selection(
        Some("mock-model".to_owned()),
        Some(ProviderSelection::Profile("mock".to_owned())),
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        true,
    )
    .is_ok());
    assert!(matches!(
        prepare_model_source_for_selection(
            Some("mock-model".to_owned()),
            Some(ProviderSelection::Profile("mock".to_owned())),
            1,
            SocketAddr::from(([127, 0, 0, 1], 0)),
            false,
        ),
        Err("provider profile API key must be set and nonempty")
    ));
}

#[test]
fn custom_profile_defaults_are_rejected_from_krypper_config_overrides() {
    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
    let config_home = TempDir::new();
    let _restore_config_home =
        RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", config_home.path().to_str().unwrap());
    let _restore_config = RestoreEnvironmentVariable::remove("KRYPPER_CONFIG");

    let user_config_path = config_home.path().join("krypper/config.toml");
    fs::create_dir_all(user_config_path.parent().unwrap()).expect("create user config directory");
    fs::write(
        &user_config_path,
        "provider = \"my-profile\"\nmodel = \"model-x\"\n",
    )
    .expect("write per-user profile default");
    let user_selection = configured_model_selection(&mut Vec::new())
        .expect("allow profile defaults from per-user config");
    assert_eq!(
        user_selection,
        Some(("model-x".to_owned(), Some("my-profile".to_owned())))
    );

    let workspace_config = TempDir::new();
    let workspace_config_path = workspace_config.path().join("config.toml");
    fs::write(
        &workspace_config_path,
        "provider = \"my-profile\"\nmodel = \"model-x\"\n",
    )
    .expect("write workspace override");
    let workspace_config_path = workspace_config_path.to_str().unwrap();
    let _restore_override =
        RestoreEnvironmentVariable::set("KRYPPER_CONFIG", workspace_config_path);
    let mut error = Vec::new();
    let failure =
        configured_model_selection(&mut error).expect_err("reject workspace profile default");
    assert!(failure.contains("not from KRYPPER_CONFIG"));
    assert!(error.is_empty());

    fs::write(
        workspace_config_path,
        "provider = \"openai\"\nmodel = \"gpt-4.1\"\n",
    )
    .expect("write built-in provider override");
    assert_eq!(
        configured_model_selection(&mut Vec::new()).expect("keep built-in overrides working"),
        Some(("gpt-4.1".to_owned(), Some("openai".to_owned())))
    );
}

#[test]
fn replacing_a_profile_does_not_change_an_in_flight_source_snapshot() {
    use crate::agent_loop::BoundedDecisionSource;

    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
    let config_home = TempDir::new();
    let config_home_value = config_home.path().to_string_lossy().into_owned();
    let _restore_config_home =
        RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", &config_home_value);
    let _restore_api_key = RestoreEnvironmentVariable::set(
        "KRYPPER_PROVIDER_SNAPSHOT_API_KEY",
        "snapshot-key-before-reload",
    );
    let (endpoint, server) = mock_server(vec![MockResponse {
        status: 200,
        body: profile_tool_response("complete_task", json!({"result":"snapshot complete"})),
    }]);
    let profile_path = config_home.path().join("krypper/provider-profiles.json");
    let profile = ProviderProfile {
        id: "snapshot".to_owned(),
        label: "Snapshot test".to_owned(),
        enabled: true,
        protocol: ProfileProtocol::OpenAiCompatibleChat,
        endpoint: format!("http://{endpoint}/v1/chat/completions"),
        credential_ref: Some("profile:snapshot:api-key".to_owned()),
    };
    add_profile(&profile_path, profile.clone()).expect("save initial profile");
    let mut source = prepare_model_source_for_selection(
        Some("snapshot-model".to_owned()),
        Some(ProviderSelection::Profile("snapshot".to_owned())),
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        false,
    )
    .expect("resolve source snapshot")
    .expect("profile creates a source");

    let replacement = ProviderProfile {
        endpoint: "https://replacement.invalid/v1/chat/completions".to_owned(),
        ..profile
    };
    replace_profile(&profile_path, replacement).expect("replace stored profile");
    std::env::set_var(
        "KRYPPER_PROVIDER_SNAPSHOT_API_KEY",
        "snapshot-key-after-reload",
    );
    assert_eq!(
        source.choose_action("continue the snapshot test", &[]),
        WorkspaceAction::Complete {
            result: "snapshot complete".to_owned()
        }
    );

    let request = server
        .join()
        .expect("join original profile endpoint")
        .pop()
        .expect("original endpoint received one request");
    assert!(String::from_utf8_lossy(&request).starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert_eq!(
        request_header(&request, "Authorization").as_deref(),
        Some("Bearer snapshot-key-before-reload")
    );
}

#[test]
fn profile_credentials_are_not_forwarded_across_a_redirect() {
    use crate::agent_loop::BoundedDecisionSource;

    let _lock = PROVIDER_ENV_TEST_LOCK
        .lock()
        .expect("lock provider environment tests");
    let config_home = TempDir::new();
    let config_home_value = config_home.path().to_string_lossy().into_owned();
    let _restore_config_home =
        RestoreEnvironmentVariable::set("XDG_CONFIG_HOME", &config_home_value);
    let _restore_api_key =
        RestoreEnvironmentVariable::set("KRYPPER_PROVIDER_REDIRECT_API_KEY", "redirect-key");
    let (endpoint_address, redirect_target, server) = redirecting_mock_server();
    let profile_path = config_home.path().join("krypper/provider-profiles.json");
    add_profile(
        &profile_path,
        ProviderProfile {
            id: "redirect".to_owned(),
            label: "Redirect test".to_owned(),
            enabled: true,
            protocol: ProfileProtocol::OpenAiCompatibleChat,
            endpoint: format!("http://{endpoint_address}/v1/chat/completions"),
            credential_ref: Some("profile:redirect:api-key".to_owned()),
        },
    )
    .expect("save redirect fixture profile");
    let mut source = prepare_model_source_for_selection(
        Some("redirect-model".to_owned()),
        Some(ProviderSelection::Profile("redirect".to_owned())),
        1,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        false,
    )
    .expect("resolve redirect fixture source")
    .expect("profile creates a source");
    source.choose_action("do not leak the credential", &[]);

    let first_request = server.join().expect("join redirect source server");
    assert_eq!(
        request_header(&first_request, "Authorization").as_deref(),
        Some("Bearer redirect-key")
    );
    match redirect_target.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok((mut stream, _)) => {
            let forwarded_request = read_request(&mut stream);
            assert_eq!(request_header(&forwarded_request, "Authorization"), None);
            panic!("credential-bearing profile request followed a redirect");
        }
        Err(error) => panic!("inspect redirect target: {error}"),
    }
}
