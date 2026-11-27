//! Behavioral evidence for the versioned direct-argv offline/read-only profile.

use super::{gate, policy};
use crate::harness::execution::{
    run_goal_turn_with_budget_and_permission_gate, ActionPermissionGate, BoundedDecisionSource,
    WorkspaceAction, WorkspaceObservation,
};
use crate::task_lifecycle::{TaskSession, TaskState, TurnState};
use crate::test_support::TempDir;
use crate::tools::process::{OfflineReadOnlyExecRequest, ProcessOutcome, SandboxExecutable};
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const LIBC_LOCK: &str = "version = 4\n\n[[package]]\nname = \"c3_fixture\"\nversion = \"0.1.0\"\ndependencies = [\n \"libc\",\n]\n\n[[package]]\nname = \"libc\"\nversion = \"0.2.191\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"65cd87c9a6d60536580b316ad985726104730a5151c99845f2490428ef91240a\"\n";

struct OneAction(Option<WorkspaceAction>);

impl BoundedDecisionSource for OneAction {
    fn choose_action(
        &mut self,
        _goal: &str,
        _observations: &[crate::harness::execution::WorkspaceObservation],
    ) -> WorkspaceAction {
        self.0.take().unwrap_or_else(|| WorkspaceAction::Complete {
            result: "C3 profile fixture complete".into(),
        })
    }
}

fn direct_cargo_request(arguments: &[&str]) -> OfflineReadOnlyExecRequest {
    direct_cargo_request_in(arguments, "crates/member")
}

fn direct_cargo_request_in(
    arguments: &[&str],
    working_directory: &str,
) -> OfflineReadOnlyExecRequest {
    OfflineReadOnlyExecRequest::new(
        SandboxExecutable::Cargo,
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory,
    )
    .expect("fixture request matches the bounded direct-argv contract")
}

fn run_approved_request(
    path: &Path,
    host: &super::RealHost,
    request: OfflineReadOnlyExecRequest,
) -> crate::ProcessResult {
    let mut session = TaskSession::new_in_workspace(path).expect("bind fixture workspace");
    let task_id = session.create_task("C3 real sandbox fixture").id();
    let owner = session.get_task_mut(task_id).expect("fixture task exists");
    owner.transition(TaskState::Running).expect("start task");
    let turn_id = owner.create_turn().expect("create fixture turn");
    owner
        .transition_turn(turn_id, TurnState::Running)
        .expect("start fixture turn");

    let process_policy = policy(host);
    session
        .authorize_offline_cargo_tests(task_id, process_policy)
        .expect("install trusted host process policy");

    let prompted_for_exact_request = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt_observed = Arc::clone(&prompted_for_exact_request);
    let expected_request = request.clone();
    let mut prompt = move |prompt_task, prompt_turn, action: &WorkspaceAction| {
        assert_eq!(prompt_task, task_id);
        assert_eq!(prompt_turn, turn_id);
        assert!(matches!(
            action,
            WorkspaceAction::OfflineReadOnlyExec { request } if request == &expected_request
        ));
        prompt_observed.store(true, Ordering::SeqCst);
        Ok(true)
    };
    let mut permission_gate = ActionPermissionGate::new(
        crate::tools::permission_policy::PermissionMode::Ask,
        true,
        Some(&mut prompt),
    );
    let mut decision = OneAction(Some(WorkspaceAction::OfflineReadOnlyExec {
        request: request.clone(),
    }));
    let execution = run_goal_turn_with_budget_and_permission_gate(
        &mut session,
        task_id,
        turn_id,
        "verify the offline read-only profile",
        2,
        &mut decision,
        &mut permission_gate,
    )
    .expect("the approved request dispatches through TaskSession");
    assert!(
        prompted_for_exact_request.load(Ordering::SeqCst),
        "exact process approval was requested"
    );
    let [WorkspaceObservation::ProcessFinished { result }] = execution.observations() else {
        panic!("one direct-process observation is required: {execution:?}");
    };
    assert_eq!(
        result.profile,
        crate::ProcessExecutionProfile::OfflineReadOnlyExecV1
    );
    result.clone()
}

fn run_cancelled_request(
    path: &Path,
    host: &super::RealHost,
    request: OfflineReadOnlyExecRequest,
    delay: Duration,
) -> crate::ProcessResult {
    let mut session = TaskSession::new_in_workspace(path).expect("bind fixture workspace");
    let task_id = session.create_task("C3 cancellation fixture").id();
    let owner = session.get_task_mut(task_id).expect("fixture task exists");
    owner.transition(TaskState::Running).expect("start task");
    let turn_id = owner.create_turn().expect("create fixture turn");
    owner
        .transition_turn(turn_id, TurnState::Running)
        .expect("start fixture turn");

    let process_policy = policy(host);
    let cancellation_flag = process_policy.cancellation();
    session
        .authorize_offline_read_only_exec_once(
            task_id,
            process_policy,
            request.clone(),
            Duration::from_secs(60),
        )
        .expect("install exact request grant");
    let trigger_flag = Arc::clone(&cancellation_flag);
    let trigger = thread::spawn(move || {
        thread::sleep(delay);
        trigger_flag.store(true, Ordering::SeqCst);
    });
    let result = session
        .run_offline_read_only_exec(task_id, turn_id, &request)
        .expect("dispatch exact request");
    trigger.join().expect("cancellation trigger finishes");
    assert!(cancellation_flag.load(Ordering::SeqCst));
    assert_eq!(
        result.profile,
        crate::ProcessExecutionProfile::OfflineReadOnlyExecV1
    );
    result
}

fn fixture_workspace(source: &str) -> TempDir {
    let path = TempDir::with_prefix("krypper-real-read-only-exec-");
    let member = path.join("crates/member");
    fs::create_dir_all(member.join("tests")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/member\"]\ndefault-members = [\"crates/member\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(path.join("Cargo.lock"), LIBC_LOCK).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"c3_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nlibc = \"=0.2.191\"\n",
    )
    .unwrap();
    fs::write(member.join("tests/c3_profile.rs"), source).unwrap();
    path
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_offline_read_only_exec_preserves_nested_cwd_and_confinement() {
    let Some(host) = gate("real_offline_read_only_exec_preserves_nested_cwd_and_confinement")
    else {
        return;
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let canary_address = listener.local_addr().unwrap();
    assert!(
        TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host loopback canary must be reachable before launch"
    );
    let ptrace = unsafe { libc::syscall(libc::SYS_ptrace, -1 as libc::c_long, 0, 0, 0) };
    assert!(
        !(ptrace == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)),
        "host ptrace baseline already returns EPERM; this host cannot verify seccomp"
    );

    let source = r#"
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const HOST_WORKSPACE: &str = __HOST_WORKSPACE__;
const HOST_CANARY: &str = "__HOST_CANARY__";

fn assert_ptrace_denied() {
    let result = unsafe { libc::syscall(libc::SYS_ptrace, -1 as libc::c_long, 0, 0, 0) };
    assert_eq!(result, -1, "ptrace unexpectedly succeeded");
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
}

fn assert_no_host_workspace_descriptor() {
    let host_root = Path::new(HOST_WORKSPACE);
    for entry in fs::read_dir("/proc/self/fd").unwrap() {
        let entry = entry.unwrap();
        let Ok(target) = fs::read_link(entry.path()) else { continue };
        assert!(
            target != host_root && !target.starts_with(host_root),
            "host workspace descriptor leaked into payload: fd {} -> {}",
            entry.file_name().to_string_lossy(), target.display()
        );
    }
}

fn assert_denies_workspace_write(path: &str) {
    assert!(!Path::new(path).exists(), "stale write probe: {path}");
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Err(error) => assert!(
            matches!(error.kind(), ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem),
            "unexpected write error for {path}: {error}"
        ),
        Ok(file) => {
            drop(file);
            let cleanup = fs::remove_file(path);
            panic!("workspace write unexpectedly succeeded for {path}; cleanup={cleanup:?}");
        }
    }
}

fn assert_profile_status() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let value = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .unwrap_or_else(|| panic!("{name} is missing from /proc/self/status"))
            .trim()
            .to_owned()
    };
    assert_eq!(value("NoNewPrivs"), "1");
    assert!(value("CapEff").trim_start_matches('0').is_empty(), "effective capabilities must be empty");
    assert_eq!(value("NSpid").split_whitespace().count(), 1, "payload must be in its own PID namespace");
    assert!(fs::read_dir("/home").is_err(), "host /home is not mounted");
    assert!(fs::metadata("/run/user").is_err(), "host user runtime tree is not mounted");
    assert!(!Path::new(HOST_WORKSPACE).exists(), "host workspace path is visible");
}

#[test]
fn c3_nested_workspace_and_seccomp_descendant() {
    assert_eq!(std::env::current_dir().unwrap(), Path::new("/workspace/crates/member"));
    assert!(Path::new("../../Cargo.toml").is_file(), "workspace parents must retain their location");
    assert_profile_status();
    assert_no_host_workspace_descriptor();
    assert_ptrace_denied();
    assert!(
        TcpStream::connect_timeout(&HOST_CANARY.parse().unwrap(), Duration::from_millis(100)).is_err(),
        "host loopback listener was reachable from the offline network namespace"
    );
    assert_denies_workspace_write("/workspace/c3-read-only-probe");
    assert_denies_workspace_write("../../c3-read-only-probe-relative");

    let descendant = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "c3_seccomp_descendant"])
        .env("KRYPPER_C3_DESCENDANT", "1")
        .status()
        .unwrap();
    assert!(descendant.success(), "seccomp/FD policy did not reach an exec descendant: {descendant}");
}

#[test]
fn c3_root_workspace_and_fd_hygiene() {
    assert!(
        std::env::current_dir()
            .unwrap()
            .starts_with("/workspace"),
        "a workspace-root C3 request must keep the workspace mounted"
    );
    assert!(Path::new("Cargo.toml").is_file());
    assert_profile_status();
    assert_no_host_workspace_descriptor();
    assert_ptrace_denied();
}

#[test]
fn c3_seccomp_descendant() {
    if std::env::var_os("KRYPPER_C3_DESCENDANT").is_some() {
        assert_ptrace_denied();
        assert_no_host_workspace_descriptor();
    }
}

#[test]
fn c3_write_output_limit() {
    println!("{}", "x".repeat(400_000));
}

#[test]
fn c3_wait_for_cancellation() {
    std::thread::sleep(Duration::from_secs(90));
}
"#
        .replace("__HOST_WORKSPACE__", &format!("{:?}", host_workspace_placeholder()))
        .replace("__HOST_CANARY__", &canary_address.to_string());
    let path = fixture_workspace(&source);
    let host_workspace = path.path().to_string_lossy().to_string();
    let source = source.replace(
        &format!("{:?}", host_workspace_placeholder()),
        &format!("{:?}", host_workspace),
    );
    fs::write(path.join("crates/member/tests/c3_profile.rs"), source).unwrap();

    let result = run_approved_request(
        path.path(),
        &host,
        direct_cargo_request(&[
            "test",
            "--offline",
            "--locked",
            "--test",
            "c3_profile",
            "--",
            "--exact",
            "c3_nested_workspace_and_seccomp_descendant",
        ]),
    );
    assert_eq!(result.outcome, ProcessOutcome::Exited(0), "{result:?}");
    assert!(result.cleanup_confirmed, "{result:?}");
    let root_cwd = run_approved_request(
        path.path(),
        &host,
        direct_cargo_request_in(
            &[
                "test",
                "--offline",
                "--locked",
                "--test",
                "c3_profile",
                "--",
                "--exact",
                "c3_root_workspace_and_fd_hygiene",
            ],
            ".",
        ),
    );
    assert_eq!(root_cwd.outcome, ProcessOutcome::Exited(0), "{root_cwd:?}");
    assert!(root_cwd.cleanup_confirmed, "{root_cwd:?}");
    assert!(
        TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host loopback must remain reachable outside the sandbox"
    );
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_offline_read_only_exec_limits_cancellation_and_cleanup() {
    let Some(host) = gate("real_offline_read_only_exec_limits_cancellation_and_cleanup") else {
        return;
    };
    let source = r#"
use std::time::Duration;
#[test]
fn c3_write_output_limit() { println!("{}", "x".repeat(400_000)); }
#[test]
fn c3_wait_for_cancellation() { std::thread::sleep(Duration::from_secs(90)); }
"#;
    let path = fixture_workspace(source);
    let output_limited = run_approved_request(
        path.path(),
        &host,
        direct_cargo_request(&[
            "test",
            "--offline",
            "--locked",
            "--test",
            "c3_profile",
            "--",
            "--nocapture",
            "c3_write_output_limit",
        ]),
    );
    assert_eq!(
        output_limited.outcome,
        ProcessOutcome::OutputLimitExceeded,
        "{output_limited:?}"
    );
    assert!(output_limited.cleanup_confirmed, "{output_limited:?}");
    assert!(
        output_limited.truncated,
        "the bounded output must be marked truncated"
    );

    let cancelled = run_cancelled_request(
        path.path(),
        &host,
        direct_cargo_request(&[
            "test",
            "--offline",
            "--locked",
            "--test",
            "c3_profile",
            "--",
            "--exact",
            "c3_wait_for_cancellation",
        ]),
        Duration::from_secs(5),
    );
    assert_eq!(
        cancelled.outcome,
        ProcessOutcome::Cancelled,
        "{cancelled:?}"
    );
    assert!(cancelled.cleanup_confirmed, "{cancelled:?}");
    assert!(
        !cancelled.recovery_hold,
        "confirmed cleanup must not hold recovery"
    );
}

fn host_workspace_placeholder() -> &'static str {
    "__C3_HOST_WORKSPACE_PATH__"
}
