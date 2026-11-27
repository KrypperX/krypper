//! Explicitly gated real-sandbox host fixtures (#219 PR 11).
//!
//! Ordinary CI must never claim host-level confinement: every fixture here is `#[ignore]`d and
//! additionally refuses to run unless the operator sets `KRYPPER_REAL_SANDBOX=1`. Opting in is a
//! claim about the host, so a missing prerequisite is a failure, never a silent skip. When the
//! gate is unset the fixture prints one `SKIP` line and returns without asserting anything.
//!
//! Each fixture covers one boundary so host evidence is attributable: supported host, isolation,
//! secrets/network, cleanup and recovery, preserved offline Cargo behavior, path replacement and
//! limits/cancellation. Failed runs never become success; nothing here weakens the fixed profile,
//! grants authority or changes CLI defaults.

use super::policy::OfflineCargoPolicy;
use super::run_for_test as run;
use super::*;
use crate::task_lifecycle::TaskSession;
use crate::test_support::TempDir;
use crate::tools::capture_workspace_root_at;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod cargo;
mod environment;
mod landlock;
mod limits;
mod read_only_exec;
mod recovery;
mod syscalls;

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
    let uid = super::effective_uid();
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
pub(super) fn workspace() -> TempDir {
    let path = TempDir::with_prefix("krypper-real-sandbox-");
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
    let outside_directory = TempDir::with_prefix("krypper-real-canary-");
    let outside = outside_directory.path().join(Uuid::new_v4().to_string());
    let probe_name = format!("krypper-fd-write-{}", Uuid::new_v4());
    let probe_paths = [
        path.join(&probe_name),
        host.toolchain.join(&probe_name),
        host.registry.join(&probe_name),
    ];
    assert!(probe_paths.iter().all(|probe| !probe.exists()));
    for probe in &probe_paths {
        let canary = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(probe)
            .unwrap_or_else(|error| {
                panic!("host fixture root must allow a unique write probe: {error}")
            });
        drop(canary);
        fs::remove_file(probe).unwrap();
    }
    fs::write(&outside, "host private canary").unwrap();
    std::os::unix::fs::symlink(&outside, path.join("escape")).unwrap();
    let source = r#"
use std::os::unix::fs::MetadataExt;

fn assert_create_denied(path: &str, expected: std::io::ErrorKind, context: &str) {
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error) => assert_eq!(error.kind(), expected, "{context}: {error}"),
        Ok(mut file) => {
            let write = std::io::Write::write_all(&mut file, b"read-only mount probe");
            drop(file);
            let cleanup = std::fs::remove_file(path);
            panic!("{context}: create succeeded (write={write:?}); cleanup={cleanup:?}");
        }
    }
}

#[test] fn isolation_selected_test() {
    assert!(std::fs::read_to_string("escape").is_err(), "workspace symlink escaped");
    assert!(std::fs::write("escape", "changed").is_err(), "workspace symlink was writable");
    assert!(std::fs::read_dir("/home").is_err(), "host /home is visible");
    assert!(std::fs::metadata("/run/user").is_err(), "host runtime dir is visible");
    assert_eq!(std::env::current_dir().unwrap().to_str().unwrap(), "/workspace");
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let nspid = status.lines().find(|line| line.starts_with("NSpid:")).expect("NSpid");
    assert_eq!(nspid.split_whitespace().count(), 2, "pid namespace not isolated: {nspid}");

    let visible_fds = std::fs::read_dir("/proc/self/fd").unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    for fd in 200..=202 {
        let fd_name = fd.to_string();
        let fd_path = format!("/proc/self/fd/{fd}");
        let mountpoint = match fd {
            200 => "/workspace",
            201 => "/toolchain",
            202 => "/cargo/registry",
            _ => unreachable!(),
        };
        let is_read_only = mountinfo.lines().any(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            fields.get(4) == Some(&mountpoint)
                && fields
                    .get(5)
                    .is_some_and(|options| options.split(',').any(|option| option == "ro"))
        });
        assert!(is_read_only, "{mountpoint} is not mounted read-only");
        assert_create_denied(
            &format!("{mountpoint}/__FD_PROBE__"),
            std::io::ErrorKind::ReadOnlyFilesystem,
            &format!("read-only mount {mountpoint}"),
        );

        let descriptor_is_visible = visible_fds.contains(&fd_name);
        if descriptor_is_visible {
            let descriptor = std::fs::metadata(&fd_path).expect("visible mount descriptor");
            let mounted_root = std::fs::metadata(mountpoint).expect("mount source destination");
            assert!(descriptor.is_dir(), "FD {fd} is not a directory");
            assert_eq!((descriptor.dev(), descriptor.ino()), (mounted_root.dev(), mounted_root.ino()), "FD {fd} is not the expected mount source");
            eprintln!("mount source FD {fd} remains visible for {mountpoint}");
            assert_create_denied(
                &format!("{fd_path}/__FD_PROBE__"),
                std::io::ErrorKind::ReadOnlyFilesystem,
                &format!("visible mount source FD {fd}"),
            );
        } else {
            eprintln!("mount source FD {fd} is closed in the payload");
            assert_create_denied(
                &format!("{fd_path}/__FD_PROBE__"),
                std::io::ErrorKind::NotFound,
                &format!("closed mount source FD {fd}"),
            );
        }
    }
}
#[test] fn unrelated_test_is_filtered() { panic!("unrelated test ran"); }
"#
        .replace("__FD_PROBE__", &probe_name);
    fs::write(path.join("src/lib.rs"), source).unwrap();
    let root = capture_workspace_root_at(&path).unwrap();
    let result = run(&root, &policy(&host), Some("isolation_selected_test"));

    let leaked_probe_files: Vec<_> = probe_paths
        .iter()
        .filter(|probe| probe.exists())
        .cloned()
        .collect();
    let outside_contents = fs::read_to_string(&outside).ok();
    for probe in &leaked_probe_files {
        fs::remove_file(probe).expect("remove host-side FD probe artifact");
    }

    assert!(
        leaked_probe_files.is_empty(),
        "read-only FD probe created host files: {leaked_probe_files:?}"
    );
    assert!(result.succeeded(), "isolation fixture failed: {result:?}");
    assert_eq!(result.executed_tests, Some(1), "{result:?}");
    assert!(result.cleanup_confirmed, "{result:?}");
    assert_eq!(outside_contents.as_deref(), Some("host private canary"));
}
