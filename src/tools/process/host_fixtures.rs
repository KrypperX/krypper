//! Explicitly gated real-sandbox host fixtures (#219 PR 11).
//!
//! Ordinary CI must never claim host-level confinement: every fixture here is `#[ignore]`d and
//! additionally refuses to run unless the operator sets `KRYPPER_REAL_SANDBOX=1`. Opting in is a
//! claim about the host, so a missing prerequisite is a failure, never a silent skip. When the
//! gate is unset the fixture prints one `SKIP` line and returns without asserting anything.
//!
//! Each fixture covers one boundary so host evidence is attributable: supported host, isolation,
//! secrets/network, cleanup, preserved offline Cargo behavior and limits/cancellation. Failed
//! runs never become success; nothing here weakens the fixed profile, grants authority or
//! changes CLI defaults.

use super::policy::OfflineCargoPolicy;
use super::supervisor::run;
use super::*;
use crate::task_lifecycle::TaskSession;
use crate::tools::capture_workspace_root_at;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod cargo;
mod environment;
mod limits;

/// The explicit opt-in. Presence is not enough; only the exact value `1` opens the gate.
pub(super) const REAL_SANDBOX_GATE: &str = "KRYPPER_REAL_SANDBOX";
pub(super) const TOOLCHAIN_ENV: &str = "KRYPPER_TEST_TOOLCHAIN";
pub(super) const REGISTRY_ENV: &str = "KRYPPER_TEST_REGISTRY";

/// A host that passed the prerequisite probe for the fixed offline Cargo profile.
pub(super) struct RealHost {
    pub(super) toolchain: PathBuf,
    pub(super) registry: PathBuf,
}

/// Every prerequisite the fixed profile needs before workspace code may launch. Values are the
/// evidence, so a failure names what the operator has to provision rather than a bare assert.
pub(super) fn host_prerequisites() -> Vec<(&'static str, bool)> {
    let uid = unsafe { libc::geteuid() };
    vec![
        ("linux", cfg!(target_os = "linux")),
        ("non_root", uid != 0),
        (
            "bwrap",
            fs::metadata("/usr/bin/bwrap")
                .map(|meta| !meta.is_dir())
                .unwrap_or(false),
        ),
        (
            "cgroup_v2_unified",
            fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").is_ok(),
        ),
        (
            "user_systemd_manager",
            fs::metadata(format!("/run/user/{uid}/systemd/private")).is_ok(),
        ),
    ]
}

/// Returns `None` only when the operator did not opt in. Once opted in, every prerequisite must
/// hold: a claimed host that cannot be verified is an error, not a skip.
pub(super) fn gate(fixture: &str) -> Option<RealHost> {
    if std::env::var_os(REAL_SANDBOX_GATE).as_deref() != Some(OsStr::new("1")) {
        eprintln!(
            "SKIP {fixture}: real-sandbox evidence is opt-in; set {REAL_SANDBOX_GATE}=1, \
             {TOOLCHAIN_ENV} and {REGISTRY_ENV} on a provisioned Linux host. \
             Ordinary CI makes no host-confinement claim."
        );
        return None;
    }
    let missing: Vec<_> = host_prerequisites()
        .into_iter()
        .filter(|(_, available)| !available)
        .map(|(name, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "{REAL_SANDBOX_GATE}=1 claims a provisioned host, but these prerequisites are \
         unavailable: {missing:?}. Provision them or unset the gate; a partial host must not \
         report confinement."
    );
    let toolchain = std::env::var_os(TOOLCHAIN_ENV)
        .unwrap_or_else(|| panic!("{TOOLCHAIN_ENV} is required with {REAL_SANDBOX_GATE}=1"));
    let registry = std::env::var_os(REGISTRY_ENV)
        .unwrap_or_else(|| panic!("{REGISTRY_ENV} is required with {REAL_SANDBOX_GATE}=1"));
    Some(RealHost {
        toolchain: PathBuf::from(toolchain),
        registry: PathBuf::from(registry),
    })
}

/// A trusted host-selected policy. Invalid toolchain/registry identities fail the fixture
/// instead of degrading to a weaker profile.
pub(super) fn policy(host: &RealHost) -> OfflineCargoPolicy {
    OfflineCargoPolicy::new(&host.toolchain, &host.registry)
        .unwrap_or_else(|error| panic!("trusted toolchain/registry rejected: {error}"))
}

/// Smallest offline Cargo workspace that can compile one test binary without a network.
pub(super) fn workspace() -> PathBuf {
    let path = std::env::temp_dir().join(format!("krypper-real-sandbox-{}", Uuid::new_v4()));
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname=\"real_sandbox_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(
        path.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"real_sandbox_fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn works() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    path
}

/// Creates a task with a running turn and an explicit host grant for the fixed profile.
pub(super) fn grant_policy(path: &Path, policy: OfflineCargoPolicy) -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new_in_workspace(path).unwrap();
    let task = session.create_task("real sandbox fixture").id();
    let owner = session.get_task_mut(task).unwrap();
    owner
        .transition(crate::task_lifecycle::TaskState::Running)
        .unwrap();
    let turn = owner.create_turn().unwrap();
    owner
        .transition_turn(turn, crate::task_lifecycle::TurnState::Running)
        .unwrap();
    session.authorize_offline_cargo_tests(task, policy).unwrap();
    (session, task, turn)
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_host_reports_supported_prerequisites() {
    let Some(_host) = gate("real_sandbox_host_reports_supported_prerequisites") else {
        return;
    };
    // The gate already asserted every prerequisite; keep the evidence visible in test output.
    for (name, available) in host_prerequisites() {
        println!("prerequisite {name}: {available}");
        assert!(
            available,
            "{name} must be available after a successful gate"
        );
    }
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_isolation_blocks_host_paths_and_namespaces() {
    let Some(host) = gate("real_sandbox_isolation_blocks_host_paths_and_namespaces") else {
        return;
    };
    let path = workspace();
    let outside = Path::new("/var/tmp").join(format!("krypper-real-canary-{}", Uuid::new_v4()));
    fs::write(&outside, "host private canary").unwrap();
    std::os::unix::fs::symlink(&outside, path.join("escape")).unwrap();
    fs::write(
        path.join("src/lib.rs"),
        r#"
#[test] fn isolation_selected_test() {
    assert!(std::fs::read_to_string("escape").is_err(), "workspace symlink escaped");
    assert!(std::fs::write("escape", "changed").is_err(), "workspace symlink was writable");
    assert!(std::fs::read_dir("/home").is_err(), "host /home is visible");
    assert!(std::fs::metadata("/run/user").is_err(), "host runtime dir is visible");
    assert_eq!(std::env::current_dir().unwrap().to_str().unwrap(), "/workspace");
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let nspid = status.lines().find(|line| line.starts_with("NSpid:")).expect("NSpid");
    assert_eq!(nspid.split_whitespace().count(), 2, "pid namespace not isolated: {nspid}");
}
#[test] fn unrelated_test_is_filtered() { panic!("unrelated test ran"); }
"#,
    )
    .unwrap();
    let root = capture_workspace_root_at(&path).unwrap();
    let result = run(&root, &policy(&host), Some("isolation_selected_test"));
    assert!(result.succeeded(), "isolation fixture failed: {result:?}");
    assert_eq!(result.executed_tests, Some(1), "{result:?}");
    assert!(result.cleanup_confirmed, "{result:?}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "host private canary");
    fs::remove_dir_all(path).unwrap();
    fs::remove_file(outside).unwrap();
}
