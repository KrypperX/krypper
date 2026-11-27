//! Opt-in Linux offline test profile. Host policy is never deserialized from model input.
//!
//! Product-owned authorization and result types stay here; the `supervisor` module adapts them
//! to the fixed offline Cargo sandbox crate while preserving existing imports.

mod outcome;
mod policy;
mod result;
mod supervisor;

#[cfg(all(test, target_os = "linux"))]
mod host_fixtures;

#[cfg(all(test, target_os = "linux"))]
mod tests;

// Keep the historical module paths available to internal callers.
#[allow(unused_imports)]
pub use self::outcome::{ProcessOutcome, ProcessResource};
pub(crate) use self::policy::{validate_test_filter, ProcessPolicyFacts, MAX_TEST_FILTER_BYTES};
pub use self::policy::{OfflineCargoPolicy, OfflineCargoPolicyError};
#[allow(unused_imports)]
pub use self::result::ProcessResult;
pub(crate) use self::supervisor::{run, run_read_only_exec};
pub use krypper_sandbox::{OfflineReadOnlyExecRequest, SandboxExecutable};

#[cfg(all(test, target_os = "linux"))]
use self::policy::cargo_command_args;
#[cfg(all(test, target_os = "linux"))]
use self::supervisor::{record_raw_output, record_test_evidence, run_with_bounds};
#[cfg(all(test, target_os = "linux"))]
use crate::tools::capture_workspace_root_at;
#[cfg(all(test, target_os = "linux"))]
use std::fs;
#[cfg(all(test, target_os = "linux"))]
use std::path::{Path, PathBuf};
#[cfg(all(test, target_os = "linux"))]
use std::sync::atomic::Ordering;
#[cfg(all(test, target_os = "linux"))]
use std::time::Duration;
#[cfg(all(test, target_os = "linux"))]
use uuid::Uuid;

#[cfg(test)]
pub(crate) fn run_for_test(
    root: &crate::tools::WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    run(root, policy, test_filter, allow_spawn_for_test())
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_without_landlock_for_test(
    root: &crate::tools::WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    supervisor::run_with_landlock_requirement_for_test(
        root,
        policy,
        test_filter,
        krypper_sandbox::LandlockRequirement::Disabled,
        allow_spawn_for_test(),
    )
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_with_bounds_for_test(
    root: &crate::tools::WorkspaceRoot,
    policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
    wall: Duration,
    output: usize,
    cpu_budget_usec: u64,
) -> ProcessResult {
    run_with_bounds(
        root,
        policy,
        test_filter,
        wall,
        output,
        cpu_budget_usec,
        allow_spawn_for_test(),
    )
}

#[cfg(test)]
fn allow_spawn_for_test() -> krypper_sandbox::SpawnAuthorizationGate {
    Box::new(|command, spawn| spawn(command).map_err(krypper_sandbox::SpawnGateError::Spawn))
}

/// The effective user id of this process.
#[cfg(target_os = "linux")]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(all(test, target_os = "macos"))]
mod unsupported_platform_tests {
    use super::{run_for_test as run, OfflineCargoPolicy, ProcessOutcome};
    use crate::test_support::TempDir;
    use crate::tools::capture_workspace_root_at;
    use std::fs;

    #[test]
    fn offline_cargo_reports_unavailable_without_a_direct_spawn_fallback() {
        let root = TempDir::with_prefix("krypper-macos-sandbox-unavailable-");
        let workspace = root.join("workspace");
        let toolchain = root.join("toolchain");
        let registry = root.join("registry");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(toolchain.join("bin")).unwrap();
        fs::create_dir_all(&registry).unwrap();
        fs::write(toolchain.join("bin/cargo"), "placeholder").unwrap();
        fs::write(toolchain.join("bin/rustc"), "placeholder").unwrap();

        let root_handle = capture_workspace_root_at(&workspace).unwrap();
        let policy = OfflineCargoPolicy::new(&toolchain, &registry).unwrap();
        let result = run(&root_handle, &policy, None);

        assert_eq!(result.outcome, ProcessOutcome::SandboxUnavailable);
        assert_eq!(
            result.diagnostic.as_deref(),
            Some("process sandbox is unavailable on this platform")
        );
    }
}
