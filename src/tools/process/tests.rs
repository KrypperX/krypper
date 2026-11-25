use super::outcome::{record_event, resolve_outcome, StopReason};
use super::*;
use crate::brain::OllamaDecisionSource;
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use crate::tools::process_profile::ProcessProfile;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::thread::{self, JoinHandle};

const TASK_FIXTURE_PATCH: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,14 @@\n-pub fn fixture_value() -> usize { 1 }\n+pub fn fixture_value() -> usize { 2 }\n+\n+#[cfg(test)]\n+mod task_fixture_tests {\n+    #[test]\n+    fn task_fixture_selected_test() {\n+        assert_eq!(super::fixture_value(), 2);\n+        assert!(std::fs::read_to_string(\"escape\").is_err());\n+        assert!(std::fs::write(\"escape\", \"changed\").is_err());\n+    }\n+\n+    #[test]\n+    fn task_fixture_unrelated_test() { panic!(\"unrelated test ran\"); }\n+}\n";

fn mock_model_server(responses: Vec<String>) -> (SocketAddr, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        responses
            .into_iter()
            .map(|body| {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_model_request(&mut stream);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
                request
            })
            .collect()
    });
    (address, handle)
}

fn read_model_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap();
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn model_tool_response(name: &str, arguments: Value) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{"function": {"name": name, "arguments": arguments}}]
        }
    })
    .to_string()
}

fn model_request_json(request: &[u8]) -> Value {
    let body_start = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    serde_json::from_slice(&request[body_start..]).unwrap()
}

fn prepare_task_fixture(path: &Path) -> PathBuf {
    fs::write(
        path.join("src/lib.rs"),
        "pub fn fixture_value() -> usize { 1 }\n",
    )
    .unwrap();
    let outside = Path::new("/var/tmp").join(format!("krypper-process-canary-{}", Uuid::new_v4()));
    fs::write(&outside, "host private canary").unwrap();
    symlink(&outside, path.join("escape")).unwrap();
    outside
}

fn task_with_process_grant(
    path: &Path,
    policy: OfflineCargoPolicy,
    goal: &str,
) -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new_in_workspace(path).unwrap();
    let task = session.create_task(goal).id();
    let owner = session.get_task_mut(task).unwrap();
    owner.transition(TaskState::Running).unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    session.authorize_offline_cargo_tests(task, policy).unwrap();
    (session, task, turn)
}

fn fixture() -> PathBuf {
    let path = std::env::temp_dir().join(format!("krypper-process-{}", Uuid::new_v4()));
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname=\"offline_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(
        path.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"offline_fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn works() { assert_eq!(2+2,4); }\n",
    )
    .unwrap();
    path
}

#[test]
fn profile_is_denied_without_explicit_task_authority() {
    let path = fixture();
    let mut session = TaskSession::new_in_workspace(&path).unwrap();
    let task = session.create_task("offline test").id();
    let owner = session.get_task_mut(task).unwrap();
    owner
        .transition(crate::task_lifecycle::TaskState::Running)
        .unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    let result = session
        .run_offline_cargo_tests(task, turn, Some("works"))
        .unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Denied);
    assert_eq!(result.test_filter.as_deref(), Some("works"));
    assert_eq!(result.elapsed_ms, 0);
    assert_eq!(result.output_bytes, 0);
    assert!(result.resource_events.is_empty());
    assert!(!result.recovery_hold);
    assert_eq!(
        session
            .get_task(task)
            .unwrap()
            .get_turn(turn)
            .unwrap()
            .state(),
        TurnState::Running
    );
    assert!(session
        .run_offline_cargo_tests(Uuid::new_v4(), turn, None)
        .is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn registered_plan_reaches_process_authorization_through_harness() {
    use crate::agent_loop::{
        run_goal_turn_with_plan, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    let path = fixture();
    let mut session = TaskSession::new_in_workspace(&path).unwrap();
    let task = session.create_task("denied process profile").id();
    let owner = session.get_task_mut(task).unwrap();
    owner
        .transition(crate::task_lifecycle::TaskState::Running)
        .unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    let plan = crate::WorkspacePlan::try_from_actions(
        vec![WorkspaceAction::OfflineCargoTest {
            test_filter: Some("works".into()),
        }],
        1,
    );
    let mut control =
        |_: &str, _: &[WorkspaceObservation]| panic!("denied process must stop before completion");
    let execution = run_goal_turn_with_plan(
        &mut session,
        task,
        turn,
        "denied process profile",
        2,
        plan,
        &mut control,
    )
    .unwrap();
    assert_eq!(execution.stop_reason(), GoalTurnStopReason::ActionFailed);
    let [WorkspaceObservation::ProcessFinished { result }] = execution.observations() else {
        panic!("registered process must reach authorization, not unsupported-action handling")
    };
    assert_eq!(result.outcome, ProcessOutcome::Denied);
    assert!(matches!(
        execution.outcome(),
        Some(crate::task_lifecycle::TurnOutcome::Failure { .. })
    ));
    assert_eq!(
        session
            .get_task(task)
            .unwrap()
            .get_turn(turn)
            .unwrap()
            .state(),
        TurnState::Failed
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn approved_profile_reaches_process_dispatch_through_task_session_and_harness() {
    use crate::agent_loop::{
        run_goal_turn_with_plan, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    let path = fixture();
    let runtime = path.with_extension("runtime");
    fs::create_dir_all(runtime.join("bin")).unwrap();
    for binary in ["cargo", "rustc"] {
        fs::copy("/usr/bin/true", runtime.join("bin").join(binary)).unwrap();
    }
    let policy = OfflineCargoPolicy::new(&runtime, &runtime).unwrap();
    assert_eq!(
        policy.profile(),
        &ProcessProfile::offline_cargo_test().unwrap()
    );

    let mut session = TaskSession::new_in_workspace(&path).unwrap();
    let task = session.create_task("approved process profile").id();
    let owner = session.get_task_mut(task).unwrap();
    owner.transition(TaskState::Running).unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    session.authorize_offline_cargo_tests(task, policy).unwrap();

    let mut passed = ProcessResult::empty(ProcessOutcome::Exited(0));
    passed.test_filter = Some("works".into());
    passed.stdout = "test works ... ok\n".into();
    passed.tests_ran = true;
    passed.executed_tests = Some(1);
    session.queue_offline_cargo_results_for_test(task, [passed]);
    let plan = crate::WorkspacePlan::try_from_actions(
        vec![WorkspaceAction::OfflineCargoTest {
            test_filter: Some("works".into()),
        }],
        2,
    );
    let mut control = |_: &str, observations: &[WorkspaceObservation]| {
        assert!(matches!(
            observations,
            [WorkspaceObservation::ProcessFinished { result }]
                if result.succeeded()
                    && result.test_filter.as_deref() == Some("works")
                    && result.executed_tests == Some(1)
        ));
        WorkspaceAction::Complete {
            result: "profile dispatched".into(),
        }
    };
    let execution = run_goal_turn_with_plan(
        &mut session,
        task,
        turn,
        "approved process profile",
        2,
        plan,
        &mut control,
    )
    .unwrap();
    assert_eq!(execution.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(
        session.offline_cargo_filters_for_test(task),
        vec![Some("works".to_owned())]
    );
    fs::remove_dir_all(path).unwrap();
    fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn missing_containment_never_launches_workspace_code() {
    let path = fixture();
    let marker = path.with_extension("launched");
    fs::write(
        path.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, b\"launched\").unwrap(); }}",
            marker
        ),
    )
    .unwrap();
    let runtime = path.with_extension("runtime");
    fs::create_dir_all(runtime.join("bin")).unwrap();
    for binary in ["cargo", "rustc"] {
        fs::copy("/usr/bin/true", runtime.join("bin").join(binary)).unwrap();
    }
    let policy = OfflineCargoPolicy::new(&runtime, &runtime).unwrap();
    let root = capture_workspace_root_at(&path).unwrap();

    let result = run_without_containment(
        &root,
        &policy,
        None,
        Duration::from_secs(2),
        8192,
        10_000_000,
    );

    assert_eq!(result.outcome, ProcessOutcome::SandboxUnavailable);
    assert!(result.cleanup_confirmed);
    assert!(!marker.exists(), "no workspace code may launch");
    fs::remove_dir_all(path).unwrap();
    fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn zero_exit_alone_is_not_success() {
    let mut result = ProcessResult::empty(ProcessOutcome::Exited(0));
    assert!(result.succeeded());
    result.cleanup_confirmed = false;
    assert!(!result.succeeded());
    result.cleanup_confirmed = true;
    result.truncated = true;
    assert!(!result.succeeded());
}

#[test]
fn test_filter_validation_rejects_unbounded_or_option_like_values() {
    assert!(validate_test_filter(None).is_ok());
    assert!(validate_test_filter(Some("module::selected_test")).is_ok());
    for filter in ["", "--list", "  -h", "line\nbreak", "nul\0byte"] {
        assert!(validate_test_filter(Some(filter)).is_err(), "{filter:?}");
    }
    let oversized = "x".repeat(MAX_TEST_FILTER_BYTES + 1);
    assert!(validate_test_filter(Some(&oversized)).is_err());
    let multibyte_oversized = "é".repeat(MAX_TEST_FILTER_BYTES / 2 + 1);
    assert!(validate_test_filter(Some(&multibyte_oversized)).is_err());
}

#[test]
fn invalid_filter_returns_before_workspace_process_launch() {
    let path = fixture();
    let marker = path.with_extension("launched");
    fs::write(
        path.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, b\"launched\").unwrap(); }}",
            marker
        ),
    )
    .unwrap();
    let runtime = path.with_extension("runtime");
    fs::create_dir_all(runtime.join("bin")).unwrap();
    for binary in ["cargo", "rustc"] {
        fs::copy("/usr/bin/true", runtime.join("bin").join(binary)).unwrap();
    }
    let policy = OfflineCargoPolicy::new(&runtime, &runtime).unwrap();
    let root = capture_workspace_root_at(&path).unwrap();

    let result = run(&root, &policy, Some("--list"));

    assert_eq!(result.outcome, ProcessOutcome::InvalidTestFilter);
    assert!(
        !marker.exists(),
        "invalid filters must not launch workspace code"
    );
    fs::remove_dir_all(path).unwrap();
    fs::remove_dir_all(runtime).unwrap();
}

#[test]
fn cargo_filter_is_one_literal_argument_after_the_fixed_separator() {
    let profile = ProcessProfile::offline_cargo_test().unwrap();
    let fixed = cargo_command_args(&profile, None);
    assert_eq!(
        fixed,
        [
            "--",
            "/toolchain/bin/cargo",
            "test",
            "--offline",
            "--locked",
            "--all-targets"
        ]
        .map(OsString::from)
    );

    let filtered = cargo_command_args(&profile, Some("module::selected test"));
    assert_eq!(
        filtered,
        [
            "--",
            "/toolchain/bin/cargo",
            "test",
            "--offline",
            "--locked",
            "--all-targets",
            "--",
            "module::selected test"
        ]
        .map(OsString::from)
    );
}

#[test]
fn test_harness_summaries_report_executed_tests_and_reject_zero_matches() {
    let mut result = ProcessResult::empty(ProcessOutcome::Exited(0));
    result.test_filter = Some("selected_test".into());
    result.stdout = "test selected_test ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.00s\n".into();
    record_test_evidence(&mut result);
    assert_eq!(result.executed_tests, Some(1));
    assert!(result.tests_ran);
    assert!(result.succeeded());

    let mut no_matches = ProcessResult::empty(ProcessOutcome::Exited(0));
    no_matches.test_filter = Some("absent_test".into());
    no_matches.stdout = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n".into();
    record_test_evidence(&mut no_matches);
    assert_eq!(no_matches.executed_tests, Some(0));
    assert!(!no_matches.tests_ran);
    assert_eq!(no_matches.outcome, ProcessOutcome::NoTestsMatched);
    assert!(!no_matches.succeeded());

    let mut summary_only = ProcessResult::empty(ProcessOutcome::Exited(0));
    summary_only.test_filter = Some("absent_test".into());
    summary_only.stdout = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n".into();
    record_test_evidence(&mut summary_only);
    assert_eq!(summary_only.executed_tests, Some(0));
    assert!(!summary_only.tests_ran);
    assert_eq!(summary_only.outcome, ProcessOutcome::NoTestsMatched);
    assert!(!summary_only.succeeded());

    let mut warning_only = ProcessResult::empty(ProcessOutcome::Exited(0));
    warning_only.test_filter = Some("absent_test".into());
    warning_only.stdout = "warning: workspace build script: test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n".into();
    record_test_evidence(&mut warning_only);
    assert_eq!(warning_only.executed_tests, Some(0));
    assert!(!warning_only.tests_ran);
    assert_eq!(warning_only.outcome, ProcessOutcome::NoTestsMatched);
    assert!(!warning_only.succeeded());
}

#[test]
fn cargo_build_script_warning_cannot_fake_a_filtered_test_result() {
    let path = fixture();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname=\"offline_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\nbuild=\"build.rs\"\n",
    )
    .unwrap();
    fs::write(
        path.join("build.rs"),
        r#"fn main() {
    println!("cargo:warning=test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s");
}
"#,
    )
    .unwrap();
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn selected_fixture_test() {}\n#[test] fn unrelated_fixture_test() {}\n",
    )
    .unwrap();
    let run_filter = |filter: &str, target: &str| {
        std::process::Command::new("cargo")
            .args(["test", "--offline", "--all-targets", "--manifest-path"])
            .arg(path.join("Cargo.toml"))
            .arg("--")
            .arg(filter)
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TARGET_DIR", path.join(target))
            .output()
            .unwrap()
    };

    let selected = run_filter("selected_fixture_test", "selected-target");
    assert!(
        selected.status.success(),
        "fixture cargo test failed: {}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let mut selected_result = ProcessResult::empty(ProcessOutcome::Exited(0));
    selected_result.test_filter = Some("selected_fixture_test".into());
    selected_result.stdout = String::from_utf8_lossy(&selected.stdout).into_owned();
    selected_result.stderr = String::from_utf8_lossy(&selected.stderr).into_owned();
    let selected_output = format!("{}{}", selected_result.stdout, selected_result.stderr);
    assert!(selected_output.contains("warning: offline_fixture@0.1.0: test result: ok. 1 passed;"));
    assert!(selected_output.contains("test selected_fixture_test ... ok"));
    assert!(selected_output.contains("1 filtered out"));
    record_test_evidence(&mut selected_result);
    assert_eq!(selected_result.executed_tests, Some(1));
    assert!(selected_result.tests_ran);
    assert!(selected_result.succeeded());

    let absent = run_filter("absent_fixture_test", "absent-target");
    assert!(
        absent.status.success(),
        "fixture cargo test failed: {}",
        String::from_utf8_lossy(&absent.stderr)
    );
    let mut absent_result = ProcessResult::empty(ProcessOutcome::Exited(0));
    absent_result.test_filter = Some("absent_fixture_test".into());
    absent_result.stdout = String::from_utf8_lossy(&absent.stdout).into_owned();
    absent_result.stderr = String::from_utf8_lossy(&absent.stderr).into_owned();
    let absent_output = format!("{}{}", absent_result.stdout, absent_result.stderr);
    assert!(absent_output.contains("warning: offline_fixture@0.1.0: test result: ok. 1 passed;"));
    assert!(absent_output.contains("test result: ok. 0 passed;"));
    record_test_evidence(&mut absent_result);
    assert_eq!(absent_result.executed_tests, Some(0));
    assert!(!absent_result.tests_ran);
    assert_eq!(absent_result.outcome, ProcessOutcome::NoTestsMatched);
    assert!(!absent_result.succeeded());

    fs::remove_dir_all(path).unwrap();
}

#[test]
fn resource_readback_rejects_missing_and_unlimited_controls() {
    let path = fixture();
    let limits = ProcessProfile::offline_cargo_test().unwrap().limits();
    let supervisor = std::mem::ManuallyDrop::new(Supervisor {
        unit: "unused".into(),
        group: path.clone(),
        cleaned: true,
        limits,
    });
    assert!(supervisor.verify().is_err());
    for (name, value) in [
        ("memory.max", limits.memory_bytes.to_string()),
        ("memory.swap.max", "0".into()),
        ("pids.max", limits.processes.to_string()),
        ("cpu.max", "100000 100000".into()),
        ("cgroup.procs", "".into()),
        ("cgroup.kill", "".into()),
    ] {
        fs::write(path.join(name), value).unwrap();
    }
    assert!(supervisor.verify().is_ok());
    for name in ["memory.max", "pids.max", "cpu.max"] {
        let original = fs::read(path.join(name)).unwrap();
        fs::write(path.join(name), "max").unwrap();
        assert!(supervisor.verify().is_err(), "{name} must fail closed");
        fs::write(path.join(name), original).unwrap();
    }
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn outcome_resolution_is_deterministic_for_limits_cancellation_and_cleanup() {
    use ProcessOutcome::*;
    let cases = [
        (StopReason::OutputLimit, vec![], true, OutputLimitExceeded),
        (StopReason::Cancelled, vec![], true, Cancelled),
        (StopReason::WallTimeout, vec![], true, TimedOut),
        (
            StopReason::CpuBudget,
            vec![],
            true,
            ResourceLimitExceeded {
                resource: ProcessResource::Cpu,
            },
        ),
        (StopReason::Exited(0), vec![], true, Exited(0)),
        (StopReason::Exited(101), vec![], true, Exited(101)),
        (StopReason::Signaled(9), vec![], true, Signaled(9)),
        (StopReason::Unknown, vec![], true, OutcomeUnknown),
        (
            StopReason::WallTimeout,
            vec![ProcessResource::Memory],
            true,
            ResourceLimitExceeded {
                resource: ProcessResource::Memory,
            },
        ),
        (
            StopReason::Cancelled,
            vec![ProcessResource::Processes],
            true,
            ResourceLimitExceeded {
                resource: ProcessResource::Processes,
            },
        ),
        (
            StopReason::Exited(0),
            vec![ProcessResource::FileSize],
            true,
            ResourceLimitExceeded {
                resource: ProcessResource::FileSize,
            },
        ),
        (StopReason::Exited(0), vec![], false, OutcomeUnknown),
        (
            StopReason::Cancelled,
            vec![ProcessResource::Cpu],
            false,
            OutcomeUnknown,
        ),
    ];
    for (stop, events, cleanup_confirmed, expected) in cases {
        assert_eq!(
            resolve_outcome(stop, &events, cleanup_confirmed),
            expected,
            "{stop:?} {events:?} cleanup={cleanup_confirmed}"
        );
    }
}

#[test]
fn kernel_limit_events_are_deduplicated_and_ordered() {
    let mut events = Vec::new();
    for resource in [
        ProcessResource::FileSize,
        ProcessResource::Processes,
        ProcessResource::Memory,
        ProcessResource::Cpu,
        ProcessResource::Memory,
    ] {
        record_event(&mut events, resource);
    }
    assert_eq!(
        events,
        [
            ProcessResource::Processes,
            ProcessResource::Memory,
            ProcessResource::Cpu,
            ProcessResource::FileSize,
        ]
    );
    assert_eq!(
        resolve_outcome(StopReason::Exited(0), &events, true),
        ProcessOutcome::ResourceLimitExceeded {
            resource: ProcessResource::Processes,
        }
    );
}

#[test]
fn audit_metadata_is_bounded_and_a_recovery_hold_blocks_success() {
    let mut result = ProcessResult::empty(ProcessOutcome::Exited(0));
    result.stdout = "x".repeat(64);
    result.stderr = "y".repeat(36);
    result.elapsed_ms = 2_000;
    result.output_bytes = 100;
    result.output_limit = 8_192;
    result.resource_events = vec![ProcessResource::Cpu, ProcessResource::Memory];
    assert!(result.succeeded());
    result.recovery_hold = true;
    assert!(!result.succeeded(), "a recovery hold must fail closed");
    assert!(result.output_bytes <= result.output_limit);
    assert_eq!(result.resource_events.len(), 2);
}

#[test]
fn process_result_serializes_bounded_audit_metadata() {
    let mut result = ProcessResult::empty(ProcessOutcome::Cancelled);
    result.elapsed_ms = 1_234;
    result.output_bytes = 16;
    result.output_limit = 8_192;
    result.resource_events = vec![ProcessResource::Memory];
    result.recovery_hold = true;
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["outcome"], "cancelled");
    assert_eq!(value["elapsed_ms"], 1_234);
    assert_eq!(value["output_bytes"], 16);
    assert_eq!(value["output_limit"], 8_192);
    assert_eq!(value["resource_events"], json!(["memory"]));
    assert_eq!(value["recovery_hold"], true);
}

#[test]
fn cancellation_after_probe_resolves_to_cancelled() {
    let path = fixture();
    let limits = ProcessProfile::offline_cargo_test().unwrap().limits();
    let mut supervisor = Supervisor {
        unit: "unused".into(),
        group: path.clone(),
        cleaned: true,
        limits,
    };
    let mut result = ProcessResult::empty_with_filter(ProcessOutcome::SandboxUnavailable, None);
    settle_before_launch(&mut result, &mut supervisor, StopReason::Cancelled);
    assert_eq!(result.outcome, ProcessOutcome::Cancelled);
    assert!(result.cleanup_confirmed);
    assert_eq!(result.output_bytes, 0);
    assert!(result.resource_events.is_empty());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn audit_metadata_counts_raw_output_bytes_not_lossy_text() {
    let mut result = ProcessResult::empty(ProcessOutcome::Exited(0));
    result.output_limit = 2;
    record_raw_output(&mut result, vec![0xff], vec![0xfe]);
    assert_eq!(result.output_bytes, 2);
    assert_eq!(result.stdout, "\u{fffd}");
    assert_eq!(result.stderr, "\u{fffd}");
    assert_eq!(result.stdout.len() + result.stderr.len(), 6);
    assert!(result.output_bytes <= result.output_limit);
    assert!(result.stdout.len() + result.stderr.len() > result.output_limit);
}

#[test]
fn process_grants_do_not_survive_a_checkpoint_restore() {
    let path = fixture();
    let runtime = fixture();
    fs::create_dir(runtime.join("bin")).unwrap();
    // Only construction is tested; these inert ELF placeholders are never executed.
    for name in ["cargo", "rustc"] {
        fs::copy("/usr/bin/true", runtime.join("bin").join(name)).unwrap();
    }
    let policy = OfflineCargoPolicy::new(&runtime, &runtime).unwrap();
    let mut session = TaskSession::new_in_workspace(&path).unwrap();
    let task = session.create_task("checkpoint fixture").id();
    session.authorize_offline_cargo_tests(task, policy).unwrap();
    let owner = session.get_task_mut(task).unwrap();
    owner
        .transition(crate::task_lifecycle::TaskState::Running)
        .unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    let mut source = |_: &str, _: &[crate::agent_loop::WorkspaceObservation]| {
        crate::agent_loop::WorkspaceAction::Continue
    };
    crate::agent_loop::run_goal_turn_with_budget(
        &mut session,
        task,
        turn,
        "checkpoint fixture",
        1,
        &mut source,
    )
    .unwrap();
    let bytes = session.export_checkpoint().unwrap();
    let mut restored = TaskSession::restore_checkpoint_in_workspace(&bytes, &path).unwrap();
    let owner = restored.get_task_mut(task).unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    assert_eq!(
        restored
            .run_offline_cargo_tests(task, turn, None)
            .unwrap()
            .outcome,
        ProcessOutcome::Denied
    );
    fs::remove_dir_all(path).unwrap();
    fs::remove_dir_all(runtime).unwrap();
}

#[test]
#[ignore = "requires KRYPPER_TEST_TOOLCHAIN and KRYPPER_TEST_REGISTRY from a delegated Linux user service with verified namespaces and cgroup v2"]
fn local_model_task_patches_then_runs_a_filtered_offline_cargo_test() {
    use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceObservation};

    let toolchain = std::env::var_os("KRYPPER_TEST_TOOLCHAIN").expect("toolchain required");
    let registry = std::env::var_os("KRYPPER_TEST_REGISTRY").expect("registry required");
    let path = fixture();
    let outside = prepare_task_fixture(&path);
    let goal = "Patch the fixture and verify its selected test";
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let (mut session, task, turn) = task_with_process_grant(&path, policy, goal);
    let (address, server) = mock_model_server(vec![
        model_tool_response(
            "apply_patch",
            json!({"path":"src/lib.rs","patch":TASK_FIXTURE_PATCH}),
        ),
        model_tool_response(
            "offline_cargo_test",
            json!({"test_filter":"task_fixture_selected_test"}),
        ),
        model_tool_response(
            "complete_task",
            json!({"result":"filtered offline Cargo test passed"}),
        ),
    ]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);

    let execution =
        run_goal_turn_with_budget(&mut session, task, turn, goal, 3, &mut source).unwrap();

    assert_eq!(execution.stop_reason(), GoalTurnStopReason::Completed);
    assert_eq!(execution.actions_taken(), 3);
    assert!(matches!(
        execution.outcome(),
        Some(TurnOutcome::Success { result }) if result == "filtered offline Cargo test passed"
    ));
    let [WorkspaceObservation::PatchApplied {
        path: patched_path,
        after_diff,
        ..
    }, WorkspaceObservation::ProcessFinished { result }] = execution.observations()
    else {
        panic!("patch and process observations must precede task completion")
    };
    assert_eq!(patched_path, Path::new("src/lib.rs"));
    assert!(after_diff.contains("task_fixture_selected_test"));
    assert!(after_diff.contains("task_fixture_unrelated_test"));
    assert_eq!(
        result.test_filter.as_deref(),
        Some("task_fixture_selected_test")
    );
    assert!(result.succeeded(), "{result:?}");
    assert!(result.tests_ran);
    assert_eq!(result.executed_tests, Some(1));
    let process_output = format!("{}{}", result.stdout, result.stderr);
    assert!(process_output.contains("task_fixture_selected_test ... ok"));
    assert!(process_output.contains("1 filtered out"));
    assert!(!process_output.contains("task_fixture_unrelated_test ..."));

    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    let completion_request = model_request_json(&requests[2]);
    let completion_prompt = completion_request["messages"][1]["content"]
        .as_str()
        .unwrap();
    assert!(completion_prompt.contains(r#""test_filter":"task_fixture_selected_test""#));
    assert!(completion_prompt.contains(r#""tests_ran":true"#));
    assert!(completion_prompt.contains(r#""executed_tests":1"#));
    assert!(completion_prompt.contains("1 filtered out"));

    let recorded_turn = session.get_task(task).unwrap().get_turn(turn).unwrap();
    assert_eq!(recorded_turn.state(), TurnState::Succeeded);
    assert!(recorded_turn
        .activity_history()
        .iter()
        .any(|summary| matches!(
            summary,
            crate::task_lifecycle::TurnActivitySummary::ProcessFinished {
                succeeded: true,
                ..
            }
        )));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "host private canary");
    fs::remove_dir_all(path).unwrap();
    fs::remove_file(outside).unwrap();
}

#[test]
#[ignore = "requires KRYPPER_TEST_TOOLCHAIN and KRYPPER_TEST_REGISTRY from a delegated Linux user service with verified namespaces and cgroup v2"]
fn local_model_task_cannot_complete_after_a_zero_match_offline_cargo_test() {
    use crate::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceObservation};

    let toolchain = std::env::var_os("KRYPPER_TEST_TOOLCHAIN").expect("toolchain required");
    let registry = std::env::var_os("KRYPPER_TEST_REGISTRY").expect("registry required");
    let path = fixture();
    let outside = prepare_task_fixture(&path);
    let goal = "Patch the fixture and verify a selected test";
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let (mut session, task, turn) = task_with_process_grant(&path, policy, goal);
    let (address, server) = mock_model_server(vec![
        model_tool_response(
            "apply_patch",
            json!({"path":"src/lib.rs","patch":TASK_FIXTURE_PATCH}),
        ),
        model_tool_response(
            "offline_cargo_test",
            json!({"test_filter":"task_fixture_filter_with_no_matches"}),
        ),
    ]);
    let mut source = OllamaDecisionSource::with_endpoint("fixture-model", 3, address);

    let execution =
        run_goal_turn_with_budget(&mut session, task, turn, goal, 3, &mut source).unwrap();

    assert_eq!(execution.stop_reason(), GoalTurnStopReason::ActionFailed);
    assert_eq!(execution.actions_taken(), 2);
    assert!(matches!(
        execution.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    let [WorkspaceObservation::PatchApplied { .. }, WorkspaceObservation::ProcessFinished { result }] =
        execution.observations()
    else {
        panic!("failed process result must be retained after the patch")
    };
    assert_eq!(result.outcome, ProcessOutcome::NoTestsMatched, "{result:?}");
    assert_eq!(
        result.test_filter.as_deref(),
        Some("task_fixture_filter_with_no_matches")
    );
    assert!(!result.succeeded());
    assert!(!result.tests_ran);
    assert_eq!(result.executed_tests, Some(0));

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "task control must not receive completion"
    );
    let task = session.get_task(task).unwrap();
    let recorded_turn = task.get_turn(turn).unwrap();
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(recorded_turn.state(), TurnState::Failed);
    assert!(matches!(
        recorded_turn.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    assert!(recorded_turn
        .activity_history()
        .iter()
        .any(|summary| matches!(
            summary,
            crate::task_lifecycle::TurnActivitySummary::ProcessFinished {
                succeeded: false,
                ..
            }
        )));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "host private canary");
    fs::remove_dir_all(path).unwrap();
    fs::remove_file(outside).unwrap();
}

#[test]
#[ignore = "run explicitly inside a host fixture without user-manager access"]
fn unavailable_host_controls_never_launch_workspace_code() {
    let toolchain = std::env::var_os("KRYPPER_TEST_TOOLCHAIN").expect("toolchain required");
    let registry = std::env::var_os("KRYPPER_TEST_REGISTRY").expect("registry required");
    let path = fixture();
    let marker = path.with_extension("launched");
    fs::write(
        path.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, b\"launched\").unwrap(); }}",
            marker
        ),
    )
    .unwrap();
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let root = capture_workspace_root_at(&path).unwrap();
    let result = run(&root, &policy, None);
    assert!(
        matches!(
            result.outcome,
            ProcessOutcome::SandboxUnavailable | ProcessOutcome::SandboxSetupFailed
        ),
        "{result:?}"
    );
    assert!(
        !marker.exists(),
        "workspace code must never launch on failed preflight"
    );
    assert!(result.cleanup_confirmed);
    fs::remove_dir_all(path).unwrap();
}

/// Explicit host fixture; ordinary CI never claims this passed when prerequisites are absent.
/// Run with --ignored on a preprovisioned Linux host and supply the real toolchain/registry.
#[test]
#[ignore = "requires verified Linux namespaces, delegated cgroup v2 and user systemd"]
fn real_offline_profile_is_confined_and_cleans_up() {
    let toolchain = std::env::var_os("KRYPPER_TEST_TOOLCHAIN").expect("toolchain required");
    let registry = std::env::var_os("KRYPPER_TEST_REGISTRY").expect("registry required");
    let path = fixture();
    let outside = Path::new("/var/tmp").join(format!("krypper-canary-{}", Uuid::new_v4()));
    fs::write(&outside, "host private canary").unwrap();
    std::os::unix::fs::symlink(&outside, path.join("escape")).unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let canary_address = listener.local_addr().unwrap();
    assert!(
        std::net::TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host canary must be reachable before sandbox launch"
    );
    fs::write(path.join("src/lib.rs"), r#"
#[test] fn confinement_selected_test() {
assert!(std::fs::read_to_string("escape").is_err());
assert!(std::fs::write("escape", "changed").is_err());
assert!(std::fs::read_dir("/home").is_err());
assert!(std::env::var("GITHUB_TOKEN").is_err());
assert!(std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err());
assert!(std::fs::metadata("/run/user").is_err());
assert!(std::net::TcpStream::connect_timeout(&"__HOST_CANARY__".parse().unwrap(), std::time::Duration::from_millis(100)).is_err());
assert_eq!(std::env::current_dir().unwrap().to_str().unwrap(), "/workspace");
}
#[test] fn unrelated_test_is_filtered() { panic!("unrelated test ran"); }
"#.replace("__HOST_CANARY__", &canary_address.to_string())).unwrap();
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let mut session = TaskSession::new_in_workspace(&path).unwrap();
    let task = session.create_task("offline test").id();
    let owner = session.get_task_mut(task).unwrap();
    owner
        .transition(crate::task_lifecycle::TaskState::Running)
        .unwrap();
    let turn = owner.create_turn().unwrap();
    owner.transition_turn(turn, TurnState::Running).unwrap();
    session.authorize_offline_cargo_tests(task, policy).unwrap();
    use crate::agent_loop::{
        run_goal_turn_with_plan, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    let plan = crate::WorkspacePlan::try_from_actions(
        vec![WorkspaceAction::OfflineCargoTest {
            test_filter: Some("confinement_selected_test".into()),
        }],
        1,
    );
    let mut control = |_: &str, observations: &[WorkspaceObservation]| {
        assert!(
            matches!(observations, [WorkspaceObservation::ProcessFinished { result }]
                if result.succeeded()
                    && result.test_filter.as_deref() == Some("confinement_selected_test")
                    && result.tests_ran
                    && result.executed_tests == Some(1)
                    && (result.stdout.contains("1 filtered out")
                        || result.stderr.contains("1 filtered out")))
        );
        WorkspaceAction::Complete {
            result: "offline Cargo verified".into(),
        }
    };
    let execution = run_goal_turn_with_plan(
        &mut session,
        task,
        turn,
        "offline test",
        2,
        plan,
        &mut control,
    )
    .unwrap();
    assert_eq!(
        execution.stop_reason(),
        GoalTurnStopReason::Completed,
        "{execution:?}"
    );
    assert_eq!(execution.actions_taken(), 2);
    assert!(matches!(
        execution.outcome(),
        Some(crate::task_lifecycle::TurnOutcome::Success { .. })
    ));
    let [WorkspaceObservation::ProcessFinished { result }] = execution.observations() else {
        panic!("one process observation required")
    };
    assert!(result.succeeded(), "{result:?}");
    assert_eq!(
        result.test_filter.as_deref(),
        Some("confinement_selected_test")
    );
    assert!(result.tests_ran);
    assert_eq!(result.executed_tests, Some(1));
    let recorded_turn = session.get_task(task).unwrap().get_turn(turn).unwrap();
    assert_eq!(recorded_turn.state(), TurnState::Succeeded);
    assert_eq!(recorded_turn.outcome(), execution.outcome());
    assert!(recorded_turn
        .activity_history()
        .iter()
        .any(|summary| matches!(
            summary,
            crate::task_lifecycle::TurnActivitySummary::ProcessFinished {
                succeeded: true,
                ..
            }
        )));
    assert!(
        std::net::TcpStream::connect_timeout(&canary_address, Duration::from_millis(100)).is_ok(),
        "host canary must remain reachable after sandbox launch"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "host private canary");
    assert!(!result.stdout.is_empty());
    // Use the actual sandbox/supervisor for failure cases, with lower-only test bounds.
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let limits = policy.profile().limits();
    let root = capture_workspace_root_at(&path).unwrap();
    let no_match = run(&root, &policy, Some("test_name_that_does_not_exist"));
    assert_eq!(
        no_match.outcome,
        ProcessOutcome::NoTestsMatched,
        "{no_match:?}"
    );
    assert_eq!(
        no_match.test_filter.as_deref(),
        Some("test_name_that_does_not_exist")
    );
    assert!(!no_match.tests_ran);
    assert_eq!(no_match.executed_tests, Some(0));
    assert!(!no_match.succeeded());
    for (source, wall, cap, expected) in [
        ("#[test] fn fails() { panic!(\"fixture failure\"); }", Duration::from_secs(10), limits.output_bytes, ProcessOutcome::Exited(101)),
        ("#[test] fn spins() { loop { std::hint::spin_loop(); } }", Duration::from_secs(2), limits.output_bytes, ProcessOutcome::TimedOut),
        ("#[test] fn floods() { use std::io::Write; for _ in 0..4096 { std::io::stdout().write_all(&[b'x';1024]).unwrap(); } }", Duration::from_secs(10), 8192, ProcessOutcome::OutputLimitExceeded),
    ] {
        fs::write(path.join("src/lib.rs"), source).unwrap();
        let result = run_with_bounds(
            &root,
            &policy,
            None,
            wall,
            cap,
            limits.cpu_ms.saturating_mul(1_000),
        );
        assert_eq!(result.outcome, expected, "{result:?}");
        assert!(result.cleanup_confirmed, "{result:?}");
        assert!(!result.succeeded());
        assert!(result.stdout.len() + result.stderr.len() <= cap);
    }
    for (source, resource) in [
        ("#[test] fn cpu() { loop { std::hint::spin_loop(); } }", ProcessResource::Cpu),
        ("#[test] fn memory() { let mut pages = Vec::new(); loop { let page = vec![1u8;16*1024*1024]; pages.push(page); std::hint::black_box(&pages); } }", ProcessResource::Memory),
        ("#[test] fn processes() { let mut children = Vec::new(); for _ in 0..256 { match std::process::Command::new(\"/usr/bin/sleep\").arg(\"20\").spawn() { Ok(child) => children.push(child), Err(_) => break, } } std::process::exit(0); }", ProcessResource::Processes),
    ] {
        fs::write(path.join("src/lib.rs"), source).unwrap();
        let result = run_with_bounds(
            &root,
            &policy,
            None,
            Duration::from_secs(limits.wall_secs),
            limits.output_bytes,
            if resource == ProcessResource::Cpu {
                10_000_000
            } else {
                limits.cpu_ms.saturating_mul(1_000)
            },
        );
        assert_eq!(result.outcome, ProcessOutcome::ResourceLimitExceeded { resource }, "{result:?}");
        assert!(result.cleanup_confirmed, "{result:?}");
        assert!(!result.succeeded());
    }
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn hangs() { std::thread::sleep(std::time::Duration::from_secs(30)); }",
    )
    .unwrap();
    let cancel = policy.cancellation();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        cancel.store(true, Ordering::SeqCst);
    });
    let result = run(&root, &policy, None);
    trigger.join().unwrap();
    assert_eq!(result.outcome, ProcessOutcome::Cancelled, "{result:?}");
    assert!(result.cleanup_confirmed);
    fs::remove_dir_all(path).unwrap();
    fs::remove_file(outside).unwrap();
}

#[test]
#[ignore = "requires the provisioned Linux host and a selected source workspace"]
fn real_product_workspace_runs_the_fixed_profile() {
    let toolchain = std::env::var_os("KRYPPER_TEST_TOOLCHAIN").expect("toolchain required");
    let registry = std::env::var_os("KRYPPER_TEST_REGISTRY").expect("registry required");
    let workspace = std::env::var_os("KRYPPER_TEST_WORKSPACE").expect("workspace required");
    let policy = OfflineCargoPolicy::new(Path::new(&toolchain), Path::new(&registry)).unwrap();
    let root = capture_workspace_root_at(Path::new(&workspace)).unwrap();
    let result = run(&root, &policy, None);
    assert!(result.succeeded(), "{result:?}");
    println!(
        "{:?} {} stdout bytes / {} stderr bytes; cleanup={}",
        result.outcome,
        result.stdout.len(),
        result.stderr.len(),
        result.cleanup_confirmed
    );
}
