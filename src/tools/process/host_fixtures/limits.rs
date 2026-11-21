//! Bounded limits and cancellation against the real sandbox, gated on a real host.

use super::super::supervisor::{run, run_with_bounds};
use super::{gate, policy, workspace};
use crate::tools::capture_workspace_root_at;
use crate::tools::process::{ProcessOutcome, ProcessResource};
use std::fs;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_limits_and_cancellation_stay_bounded() {
    let Some(host) = gate("real_sandbox_limits_and_cancellation_stay_bounded") else {
        return;
    };
    let path = workspace();
    let policy = policy(&host);
    let limits = policy.profile().limits();
    let root = capture_workspace_root_at(&path).unwrap();
    let no_match = run(&root, &policy, Some("test_name_that_does_not_exist"));
    assert_eq!(
        no_match.outcome,
        ProcessOutcome::NoTestsMatched,
        "{no_match:?}"
    );
    assert!(!no_match.tests_ran);
    for (source, wall, cap, expected) in [
        (
            "#[test] fn fails() { panic!(\"fixture failure\"); }",
            Duration::from_secs(10),
            limits.output_bytes,
            ProcessOutcome::Exited(101),
        ),
        (
            "#[test] fn spins() { loop { std::hint::spin_loop(); } }",
            Duration::from_secs(2),
            limits.output_bytes,
            ProcessOutcome::TimedOut,
        ),
        (
            "#[test] fn floods() { use std::io::Write; for _ in 0..4096 { std::io::stdout().write_all(&[b'x'; 1024]).unwrap(); } }",
            Duration::from_secs(10),
            8192,
            ProcessOutcome::OutputLimitExceeded,
        ),
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
        (
            "#[test] fn cpu() { loop { std::hint::spin_loop(); } }",
            ProcessResource::Cpu,
        ),
        (
            "#[test] fn memory() { let mut pages = Vec::new(); loop { let page = vec![1u8; 16 * 1024 * 1024]; pages.push(page); std::hint::black_box(&pages); } }",
            ProcessResource::Memory,
        ),
        (
            "#[test] fn processes() { let mut children = Vec::new(); for _ in 0..256 { match std::process::Command::new(\"/usr/bin/sleep\").arg(\"20\").spawn() { Ok(child) => children.push(child), Err(_) => break } } std::process::exit(0); }",
            ProcessResource::Processes,
        ),
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
        assert_eq!(
            result.outcome,
            ProcessOutcome::ResourceLimitExceeded { resource },
            "{result:?}"
        );
        assert!(result.cleanup_confirmed, "{result:?}");
        assert!(!result.succeeded());
    }
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn hangs() { std::thread::sleep(std::time::Duration::from_secs(30)); }\n",
    )
    .unwrap();
    let cancel = policy.cancellation();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        cancel.store(true, Ordering::SeqCst);
    });
    let cancelled = run(&root, &policy, None);
    trigger.join().unwrap();
    assert_eq!(
        cancelled.outcome,
        ProcessOutcome::Cancelled,
        "{cancelled:?}"
    );
    assert!(cancelled.cleanup_confirmed);
    fs::remove_dir_all(path).unwrap();
}
