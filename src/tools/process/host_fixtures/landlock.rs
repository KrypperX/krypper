//! Real Landlock write-confinement evidence through Cargo and an exec descendant.

use super::{gate, policy, workspace};
use crate::tools::capture_workspace_root_at;
use std::fs;

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_landlock_limits_writes_and_reaches_descendants() {
    let Some(host) = gate("real_sandbox_landlock_limits_writes_and_reaches_descendants") else {
        return;
    };

    let path = workspace();
    fs::write(
        path.join("src/lib.rs"),
        r#"
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

fn assert_landlock_denies_write(path: &str) {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{path}: {error}");
        }
        Ok(file) => {
            drop(file);
            let cleanup = std::fs::remove_file(path);
            panic!("Landlock allowed unlisted write {path}; cleanup={cleanup:?}");
        }
    }
}

#[test]
fn landlock_descendant_probe() {
    let Some(path) = std::env::var_os("KRYPPER_LANDLOCK_DESCENDANT_PATH") else {
        return;
    };
    assert_landlock_denies_write(path.to_str().expect("UTF-8 probe path"));
}

#[test]
fn landlock_allows_profile_scratch_and_denies_unlisted_tmp() {
    assert_eq!(std::env::var("TMPDIR").as_deref(), Ok("/tmp/tmp"));

    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let tmp_mount = mountinfo
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some("/tmp"))
        .expect("Bubblewrap /tmp tmpfs mount");
    let fields = tmp_mount.split_whitespace().collect::<Vec<_>>();
    assert!(fields[5].split(',').any(|option| option == "rw"), "/tmp must be writable in Bubblewrap before Landlock");
    let separator = fields.iter().position(|field| *field == "-").expect("mountinfo separator");
    assert_eq!(fields.get(separator + 1), Some(&"tmpfs"), "/tmp must be Bubblewrap's tmpfs");
    let effective_uid = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok())
        .expect("effective uid in proc status");
    let tmp_metadata = std::fs::metadata("/tmp").unwrap();
    assert_eq!(tmp_metadata.uid(), effective_uid, "/tmp must be owned by the sandbox uid");
    assert_ne!(tmp_metadata.mode() & 0o200, 0, "/tmp must grant its owner write access before Landlock");

    for directory in ["/tmp/home", "/tmp/target", "/tmp/tmp"] {
        let probe = format!("{directory}/landlock-allowed-{}", std::process::id());
        std::fs::write(&probe, b"allowed scratch").unwrap_or_else(|error| {
            panic!("Landlock denied required scratch write {probe}: {error}")
        });
        std::fs::remove_file(probe).unwrap();
    }

    let process_id = std::process::id();
    let denied_path = format!("/tmp/krypper-landlock-denied-{process_id}");
    assert_landlock_denies_write(&denied_path);
    let descendant_path = format!("/tmp/krypper-landlock-descendant-denied-{process_id}");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "landlock_descendant_probe"])
        .env("KRYPPER_LANDLOCK_DESCENDANT_PATH", &descendant_path)
        .status()
        .unwrap();
    assert!(child.success(), "Landlock restriction did not reach the exec descendant: {child}");
}
"#,
    )
    .unwrap();

    let root = capture_workspace_root_at(&path).unwrap();
    let result = crate::tools::process::run_for_test(
        &root,
        &policy(&host),
        Some("landlock_allows_profile_scratch_and_denies_unlisted_tmp"),
    );
    assert!(
        result.succeeded(),
        "Landlock Cargo fixture failed: {result:?}"
    );
    assert_eq!(result.executed_tests, Some(1), "{result:?}");
    assert!(result.cleanup_confirmed, "{result:?}");
    assert!(!result.recovery_hold, "{result:?}");
    let status = result
        .sandbox_evidence
        .as_ref()
        .and_then(|evidence| {
            evidence.controls().iter().find(|record| {
                record.control == krypper_sandbox::SandboxControl::LandlockFilesystemWrites
            })
        })
        .map(|record| record.status);
    assert_eq!(
        status,
        Some(krypper_sandbox::SandboxEvidenceStatus::Applied),
        "the fixture must observe the Landlock runner handshake before attributing denials to Landlock"
    );
}
