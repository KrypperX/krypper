//! Provider endpoint validation before credentials are attached.

use super::*;
use crate::agent_loop::{BoundedDecisionSource, WorkspaceAction};
use std::io::ErrorKind;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[test]
fn endpoint_validation_accepts_https_and_loopback_http() {
    for endpoint in [
        "https://api.example.com/v1/chat",
        "https://api.example.com:443/models/x",
        "https://127.0.0.1/x",
        "https://[::1]:8443/x",
        "http://127.0.0.1:1234/x",
        "http://127.1.2.3/x",
        "http://[::1]:1234/x",
        "http://localhost:1234/x",
        "http://LOCALHOST/x",
        "http://localhost/x?key=value",
    ] {
        assert!(
            validate_endpoint(endpoint).is_ok(),
            "rejected loopback or https endpoint {endpoint:?}"
        );
    }
}

#[test]
fn endpoint_validation_rejects_plaintext_remote_and_malformed_urls() {
    for endpoint in [
        "http://example.com/x",
        "http://10.0.0.1/x",
        "http://192.168.1.1:8080/x",
        "http://127.0.0.1.evil.test/x",
        "https://user:pw@host/x",
        "http://user:pw@127.0.0.1/x",
        "https://@api.example.com/x",
        "https:///no-host",
        "http://",
        "http://:1234/x",
        "http://localhost:/x",
        "http://localhost:99999/x",
        "http://[::1:1234/x",
        "http://[::2]:80/x",
        "file:///etc/passwd",
        "ftp://example.com/x",
        "api.example.com/x",
        "",
        "   ",
        "https://api.example.com/a b",
        "https://api.example.com/x\n",
    ] {
        assert!(
            validate_endpoint(endpoint).is_err(),
            "accepted unsafe endpoint {endpoint:?}"
        );
    }
}

#[test]
fn rejected_endpoint_produces_no_request_and_no_credential() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock provider server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking mock server");
    let address = listener.local_addr().expect("mock server address");
    let endpoint = format!("http://user:secret@{address}/v1beta/models/test:generateContent");
    let mut source =
        GeminiDecisionSource::with_endpoint("test-model", "loopback-test-key", 1, endpoint);

    match source.choose_action("Inspect the workspace", &[]) {
        WorkspaceAction::Unsupported { action } => {
            assert!(
                action.contains("endpoint"),
                "unexpected rejection reason {action:?}"
            );
        }
        other => panic!("expected a rejected endpoint action, got {other:?}"),
    }

    thread::sleep(Duration::from_millis(150));
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("rejected endpoint produced a network request"),
        Err(error) => panic!("unexpected mock server error: {error}"),
    }
}
