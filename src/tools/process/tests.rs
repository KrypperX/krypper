use super::*;
use crate::task_lifecycle::{TaskSession, TurnState};

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
    assert_eq!(
        session.run_offline_cargo_tests(task, turn).unwrap().outcome,
        ProcessOutcome::Denied
    );
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
        .run_offline_cargo_tests(Uuid::new_v4(), turn)
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
    let plan = crate::WorkspacePlan::try_from_actions(vec![WorkspaceAction::OfflineCargoTest], 1);
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
fn resource_readback_rejects_missing_and_unlimited_controls() {
    let path = fixture();
    let supervisor = std::mem::ManuallyDrop::new(Supervisor {
        unit: "unused".into(),
        group: path.clone(),
        cleaned: true,
    });
    assert!(supervisor.verify().is_err());
    for (name, value) in [
        ("memory.max", MEMORY.to_string()),
        ("memory.swap.max", "0".into()),
        ("pids.max", TASKS.to_string()),
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
            .run_offline_cargo_tests(task, turn)
            .unwrap()
            .outcome,
        ProcessOutcome::Denied
    );
    fs::remove_dir_all(path).unwrap();
    fs::remove_dir_all(runtime).unwrap();
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
    let result = run(&root, &policy);
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
#[test] fn confinement() {
assert!(std::fs::read_to_string("escape").is_err());
assert!(std::fs::write("escape", "changed").is_err());
assert!(std::fs::read_dir("/home").is_err());
assert!(std::env::var("GITHUB_TOKEN").is_err());
assert!(std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err());
assert!(std::fs::metadata("/run/user").is_err());
assert!(std::net::TcpStream::connect_timeout(&"__HOST_CANARY__".parse().unwrap(), std::time::Duration::from_millis(100)).is_err());
assert_eq!(std::env::current_dir().unwrap().to_str().unwrap(), "/workspace");
}
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
    let plan = crate::WorkspacePlan::try_from_actions(vec![WorkspaceAction::OfflineCargoTest], 1);
    let mut control = |_: &str, observations: &[WorkspaceObservation]| {
        assert!(
            matches!(observations, [WorkspaceObservation::ProcessFinished { result }] if result.succeeded())
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
    let root = capture_workspace_root_at(&path).unwrap();
    for (source, wall, cap, expected) in [
        ("#[test] fn fails() { panic!(\"fixture failure\"); }", Duration::from_secs(10), OUTPUT, ProcessOutcome::Exited(101)),
        ("#[test] fn spins() { loop { std::hint::spin_loop(); } }", Duration::from_secs(2), OUTPUT, ProcessOutcome::TimedOut),
        ("#[test] fn floods() { use std::io::Write; for _ in 0..4096 { std::io::stdout().write_all(&[b'x';1024]).unwrap(); } }", Duration::from_secs(10), 8192, ProcessOutcome::OutputLimitExceeded),
    ] {
        fs::write(path.join("src/lib.rs"), source).unwrap();
        let result = run_with_bounds(&root, &policy, wall, cap, CPU_BUDGET);
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
        let result = run_with_bounds(&root, &policy, WALL, OUTPUT, if resource == ProcessResource::Cpu { 10_000_000 } else { CPU_BUDGET });
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
    let result = run(&root, &policy);
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
    let result = run(&root, &policy);
    assert!(result.succeeded(), "{result:?}");
    println!(
        "{:?} {} stdout bytes / {} stderr bytes; cleanup={}",
        result.outcome,
        result.stdout.len(),
        result.stderr.len(),
        result.cleanup_confirmed
    );
}
