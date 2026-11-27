//! Crash recovery and captured-workspace path replacement on the gated Linux host.

use super::super::{run_for_test as run, ProcessOutcome};
use super::{gate, policy, workspace, RealHost, REAL_SANDBOX_GATE, REGISTRY_ENV, TOOLCHAIN_ENV};
use crate::test_support::TempDir;
use crate::tools::capture_workspace_root_at;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CRASH_WORKSPACE_ENV: &str = "KRYPPER_C4_CRASH_WORKSPACE";
const CRASH_TEST_FILTER: &str = "crash_fixture_payload_waiting_for_release";
const HELPER_TEST_FILTER: &str = "crash_helper_child";
const RECEIPT_NAME: &str = "krypper-offline-cargo.receipt";

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on an isolated provisioned Linux host"]
fn real_sandbox_captured_workspace_path_swap_uses_pinned_root_or_fails_closed() {
    let Some(host) =
        gate("real_sandbox_captured_workspace_path_swap_uses_pinned_root_or_fails_closed")
    else {
        return;
    };
    let captured = workspace();
    let outside = workspace();
    fs::write(captured.join("identity"), "captured-root").unwrap();
    fs::write(outside.join("identity"), "outside-root").unwrap();
    let source = r#"
#[test]
fn captured_root_identity_is_used() {
    assert_eq!(std::fs::read_to_string("identity").unwrap(), "captured-root");
}
"#;
    fs::write(captured.join("src/lib.rs"), source).unwrap();
    fs::write(outside.join("src/lib.rs"), source).unwrap();

    let root = capture_workspace_root_at(&captured).unwrap();
    let replacement = CapturedPathReplacement::install(captured.path(), outside.path());
    let result = run(
        &root,
        &policy(&host),
        Some("captured_root_identity_is_used"),
    );
    let fail_closed = matches!(
        result.outcome,
        ProcessOutcome::Denied
            | ProcessOutcome::SpawnFailed
            | ProcessOutcome::SandboxUnavailable
            | ProcessOutcome::SandboxSetupFailed
    ) && result.cleanup_confirmed
        && !result.recovery_hold
        && !result.tests_ran;
    assert!(
        result.succeeded() || fail_closed,
        "path replacement must run only the captured root or fail before tests: {result:?}"
    );
    if result.succeeded() {
        assert!(
            result.tests_ran,
            "captured-root success must include test evidence: {result:?}"
        );
        assert_eq!(result.executed_tests, Some(1), "{result:?}");
    }
    assert_eq!(
        fs::read_to_string(outside.join("identity")).unwrap(),
        "outside-root"
    );
    drop(replacement);
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on an isolated provisioned Linux host"]
fn real_sandbox_caller_crash_retains_and_recovers_execution_lease() {
    let Some(host) = gate("real_sandbox_caller_crash_retains_and_recovers_execution_lease") else {
        return;
    };
    let receipt = receipt_path();
    assert!(
        !receipt.exists(),
        "isolated crash fixture requires no prior cleanup receipt: {}",
        receipt.display()
    );

    let blocked_workspace = workspace();
    fs::write(
        blocked_workspace.join("src/lib.rs"),
        format!(
            "#[test] fn {CRASH_TEST_FILTER}() {{ loop {{ std::thread::sleep(std::time::Duration::from_secs(1)); }} }}\n"
        ),
    )
    .unwrap();
    let mut helper = HelperGuard::spawn(&host, &blocked_workspace);
    let mut unit = wait_for_fixture_unit(&mut helper, &receipt);
    wait_for_payload(&mut helper, &unit.group);

    let status = helper.kill_and_wait();
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "caller was not killed: {status}"
    );
    wait_for_payload_exit(&unit.unit, &unit.group);
    assert_receipt_matches(&receipt, &unit);
    assert_eq!(unit_state(&unit.unit).as_deref(), Some("active"));
    assert_eq!(
        populated(&unit.group),
        Some(true),
        "the fixture lease must survive its caller"
    );

    let held_root = capture_workspace_root_at(&blocked_workspace).unwrap();
    let held = run(&held_root, &policy(&host), Some(CRASH_TEST_FILTER));
    assert_eq!(held.outcome, ProcessOutcome::OutcomeUnknown, "{held:?}");
    assert!(!held.cleanup_confirmed, "{held:?}");
    assert!(held.recovery_hold, "{held:?}");
    assert!(
        !held.tests_ran,
        "held recovery must not launch tests: {held:?}"
    );
    assert_eq!(unit_state(&unit.unit).as_deref(), Some("active"));
    assert_eq!(
        populated(&unit.group),
        Some(true),
        "recovery hold must leave the uncertain fixture service intact"
    );
    assert!(
        !payload_is_running(&unit.group),
        "the terminated caller's selected payload must stay stopped"
    );
    assert_receipt_matches(&receipt, &unit);

    unit.stop_and_confirm();
    let fresh_workspace = workspace();
    let fresh_root = capture_workspace_root_at(&fresh_workspace).unwrap();
    let recovered = run(&fresh_root, &policy(&host), Some("works"));
    assert!(
        recovered.succeeded(),
        "recovery did not permit a clean run: {recovered:?}"
    );
    assert!(recovered.cleanup_confirmed, "{recovered:?}");
    assert!(!recovered.recovery_hold, "{recovered:?}");
    assert!(
        !receipt.exists(),
        "successful recovery must clear the verified receipt"
    );
}

/// Invoked only by the parent crash fixture through this test binary.
#[test]
#[ignore = "private crash-fixture child"]
fn crash_helper_child() {
    let workspace = PathBuf::from(
        std::env::var_os(CRASH_WORKSPACE_ENV).expect("parent supplies the fixture workspace"),
    );
    let host = gate("crash_helper_child").expect("parent enables the host gate");
    let root = capture_workspace_root_at(&workspace).unwrap();
    let result = run(&root, &policy(&host), Some(CRASH_TEST_FILTER));
    panic!("blocking payload unexpectedly returned: {result:?}");
}

struct CapturedPathReplacement {
    visible: PathBuf,
    captured: PathBuf,
}

impl CapturedPathReplacement {
    fn install(visible: &Path, outside: &Path) -> Self {
        let captured = visible.with_file_name(format!(".krypper-captured-{}", Uuid::new_v4()));
        fs::rename(visible, &captured).unwrap();
        if let Err(error) = symlink(outside, visible) {
            let _ = fs::rename(&captured, visible);
            panic!("replace captured workspace path with a symlink: {error}");
        }
        Self {
            visible: visible.to_path_buf(),
            captured,
        }
    }
}

impl Drop for CapturedPathReplacement {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.visible)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            let _ = fs::remove_file(&self.visible);
        }
        if !self.visible.exists() {
            let _ = fs::rename(&self.captured, &self.visible);
        }
    }
}

struct HelperGuard(Option<Child>);

impl HelperGuard {
    fn spawn(host: &RealHost, workspace: &TempDir) -> Self {
        let executable = std::env::current_exe().expect("current test executable");
        let child = Command::new(executable)
            .args([
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                HELPER_TEST_FILTER,
            ])
            .env(REAL_SANDBOX_GATE, "1")
            .env(TOOLCHAIN_ENV, &host.toolchain)
            .env(REGISTRY_ENV, &host.registry)
            .env(CRASH_WORKSPACE_ENV, workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated caller helper");
        Self(Some(child))
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.as_mut().expect("helper still running").try_wait()
    }

    fn kill_and_wait(&mut self) -> ExitStatus {
        let mut child = self.0.take().expect("helper still running");
        child
            .kill()
            .expect("kill the helper after payload observation");
        child.wait().expect("reap killed helper")
    }
}

impl Drop for HelperGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct FixtureUnit {
    unit: String,
    group: PathBuf,
    armed: bool,
}

impl FixtureUnit {
    fn stop_and_confirm(&mut self) {
        assert!(
            self.stop_until_empty(),
            "fixture unit did not stop cleanly: {}",
            self.unit
        );
        self.armed = false;
    }

    fn stop_until_empty(&self) -> bool {
        let Ok(status) = Command::new("/usr/bin/systemctl")
            .args(["--user", "stop", &self.unit])
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let inactive = matches!(
                unit_state(&self.unit).as_deref(),
                Some("inactive" | "failed" | "not-found")
            );
            if inactive {
                if self.group.as_os_str().is_empty() {
                    return false;
                }
                if populated(&self.group) == Some(false) || !self.group.exists() {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for FixtureUnit {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.stop_until_empty();
        }
    }
}

fn wait_for_fixture_unit(helper: &mut HelperGuard, receipt: &Path) -> FixtureUnit {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut observed_unit = None;
    loop {
        if let Ok(contents) = fs::read_to_string(receipt) {
            let mut lines = contents.lines();
            let unit = lines.next().unwrap_or_default();
            validate_unit(unit);
            if observed_unit.is_none() {
                if let Some(group) = systemd_group_path(unit) {
                    observed_unit = Some(FixtureUnit {
                        unit: unit.to_owned(),
                        group,
                        armed: false,
                    });
                }
            }
            let Some(fixture_unit) = observed_unit.as_mut() else {
                if let Some(status) = helper.try_wait().expect("poll helper") {
                    panic!("crash helper exited before its fixture service was observed: {status}");
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for systemd to report the fixture service"
                );
                thread::sleep(Duration::from_millis(20));
                continue;
            };
            assert_eq!(
                fixture_unit.unit, unit,
                "receipt identity changed during the fixture"
            );
            if let Some(group) = lines.next() {
                // If the receipt cgroup is invalid, disarm teardown before asserting. Cleanup
                // may target only the systemd-verified group after receipt identity matches.
                fixture_unit.armed = false;
                validate_group(unit, group);
                let receipt_group = PathBuf::from(group);
                validate_systemd_group(unit, &receipt_group);
                assert_eq!(
                    receipt_group, fixture_unit.group,
                    "receipt cgroup differs from the systemd-verified fixture group"
                );
                fixture_unit.armed = true;
                if unit_state(unit).as_deref() == Some("active")
                    && populated(&fixture_unit.group) == Some(true)
                {
                    return observed_unit.take().expect("validated fixture unit");
                }
            } else {
                // A unit-only receipt can precede the cgroup update. systemd_group_path above
                // already bound this cleanup target to the validated UUID-named service.
                fixture_unit.armed = true;
            }
        }
        if let Some(status) = helper.try_wait().expect("poll helper") {
            panic!("crash helper exited before its payload was observed: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the fixture payload"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_payload(helper: &mut HelperGuard, group: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if helper.try_wait().expect("poll helper").is_some() {
            panic!("crash helper exited before the selected payload became active");
        }
        if payload_is_running(group) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "selected sandbox payload did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_payload_exit(unit: &str, group: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !payload_is_running(group) {
            return;
        }
        assert_eq!(
            unit_state(unit).as_deref(),
            Some("active"),
            "service must remain active while waiting for the parent-death result"
        );
        assert_eq!(
            populated(group),
            Some(true),
            "lease cgroup must remain populated while the selected payload is still alive"
        );
        assert!(
            Instant::now() < deadline,
            "selected sandbox payload survived its caller"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_receipt_matches(receipt: &Path, fixture: &FixtureUnit) {
    let contents = fs::read_to_string(receipt).expect("caller crash must retain the receipt");
    let mut identity = contents.lines();
    assert_eq!(identity.next(), Some(fixture.unit.as_str()));
    let group = identity
        .next()
        .expect("caller crash must retain the cgroup identity");
    assert!(
        identity.next().is_none(),
        "receipt has unexpected trailing data"
    );
    validate_group(&fixture.unit, group);
    assert_eq!(Path::new(group), fixture.group);
    validate_systemd_group(&fixture.unit, &fixture.group);
}

fn payload_is_running(group: &Path) -> bool {
    let Ok(relative_group) = group.strip_prefix("/sys/fs/cgroup") else {
        return false;
    };
    let expected = format!("/{}", relative_group.display());
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        let proc = entry.path();
        let Ok(cgroup) = fs::read_to_string(proc.join("cgroup")) else {
            return false;
        };
        if !cgroup
            .lines()
            .any(|line| line.strip_prefix("0::") == Some(expected.as_str()))
        {
            return false;
        }
        let executable = fs::read_link(proc.join("exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if !executable.starts_with("real_sandbox_fixture-") {
            return false;
        }
        fs::read(proc.join("cmdline")).is_ok_and(|arguments| {
            arguments
                .windows(CRASH_TEST_FILTER.len())
                .any(|window| window == CRASH_TEST_FILTER.as_bytes())
        })
    })
}

fn receipt_path() -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/run/user/{uid}/{RECEIPT_NAME}"))
}

fn validate_unit(unit: &str) {
    let id = unit
        .strip_prefix("krypper-test-")
        .and_then(|value| value.strip_suffix(".service"))
        .expect("receipt must identify a Krypper fixture service");
    let uuid = Uuid::parse_str(id).expect("receipt service must have a UUID");
    assert_eq!(
        uuid.simple().to_string(),
        id,
        "receipt UUID must be lowercase hex"
    );
}

fn validate_group(unit: &str, group: &str) {
    let path = Path::new(group);
    assert!(path.is_absolute(), "receipt cgroup must be absolute");
    assert!(
        path.starts_with("/sys/fs/cgroup"),
        "receipt cgroup must be under cgroup v2"
    );
    assert_eq!(path.file_name().and_then(|name| name.to_str()), Some(unit));
    assert!(!path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir)));
}

fn validate_systemd_group(unit: &str, group: &Path) {
    assert_eq!(
        group,
        &systemd_group_path(unit).expect("systemd must report the fixture cgroup")
    );
}

fn systemd_group_path(unit: &str) -> Option<PathBuf> {
    let output = Command::new("/usr/bin/systemctl")
        .args(["--user", "show", unit, "--property=ControlGroup", "--value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let control_group = String::from_utf8(output.stdout).ok()?;
    let control_group = control_group.trim();
    if !control_group.starts_with('/') || control_group.split('/').any(|part| part == "..") {
        return None;
    }
    let group = Path::new("/sys/fs/cgroup").join(control_group.trim_start_matches('/'));
    if !group.starts_with("/sys/fs/cgroup")
        || group.file_name().and_then(|name| name.to_str()) != Some(unit)
        || group
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(group)
}

fn unit_state(unit: &str) -> Option<String> {
    let output = Command::new("/usr/bin/systemctl")
        .args(["--user", "show", unit, "--property=ActiveState", "--value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn populated(group: &Path) -> Option<bool> {
    if !group.exists() {
        return Some(false);
    }
    fs::read_to_string(group.join("cgroup.events"))
        .ok()?
        .lines()
        .find_map(|line| match line {
            "populated 0" => Some(false),
            "populated 1" => Some(true),
            _ => None,
        })
}
