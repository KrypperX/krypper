//! Secret, network and cleanup boundaries for the gated real-sandbox fixtures.

use super::super::supervisor::run;
use super::{gate, policy, workspace};
use crate::tools::capture_workspace_root_at;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use uuid::Uuid;

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_environment_and_network_carry_no_host_secrets() {
    let Some(host) = gate("real_sandbox_environment_and_network_carry_no_host_secrets") else {
        return;
    };
    let path = workspace();
    let secret = format!("secret-canary-{}", Uuid::new_v4());
    std::env::set_var("GITHUB_TOKEN", &secret);
    std::env::set_var("KRYPPER_TEST_SECRET_CANARY", &secret);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let canary_address = listener.local_addr().unwrap();
    assert!(
        TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host loopback canary must be reachable before launch"
    );
    fs::write(
        path.join("src/lib.rs"),
        r#"
#[test] fn secrets_selected_test() {
    for name in ["GITHUB_TOKEN", "KRYPPER_TEST_SECRET_CANARY", "SSH_AUTH_SOCK", "DBUS_SESSION_BUS_ADDRESS"] {
        assert!(std::env::var(name).is_err(), "{name} leaked into the sandbox");
    }
    assert!(std::net::TcpStream::connect_timeout(
        &"__HOST_CANARY__".parse().unwrap(),
        std::time::Duration::from_millis(100)
    ).is_err(), "loopback egress was not blocked");
}
"#
        .replace("__HOST_CANARY__", &canary_address.to_string()),
    )
    .unwrap();
    let root = capture_workspace_root_at(&path).unwrap();
    let result = run(&root, &policy(&host), Some("secrets_selected_test"));
    std::env::remove_var("GITHUB_TOKEN");
    std::env::remove_var("KRYPPER_TEST_SECRET_CANARY");
    assert!(
        result.succeeded(),
        "secret/network fixture failed: {result:?}"
    );
    assert!(result.cleanup_confirmed, "{result:?}");
    assert!(
        TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host loopback must remain reachable after the sandboxed run"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_cleanup_releases_state_for_the_next_run() {
    let Some(host) = gate("real_sandbox_cleanup_releases_state_for_the_next_run") else {
        return;
    };
    let path = workspace();
    let root = capture_workspace_root_at(&path).unwrap();
    let first = run(&root, &policy(&host), Some("works"));
    assert!(first.succeeded(), "first run failed: {first:?}");
    assert!(first.cleanup_confirmed, "{first:?}");
    assert!(
        !first.recovery_hold,
        "clean run must not hold recovery: {first:?}"
    );
    // A held lease or an unconfirmed cleanup would degrade this second attempt.
    let second = run(&root, &policy(&host), Some("works"));
    assert!(second.succeeded(), "second run failed: {second:?}");
    assert!(second.cleanup_confirmed, "{second:?}");
    assert!(!second.recovery_hold, "{second:?}");
    fs::remove_dir_all(path).unwrap();
}
