//! Fail-closed process execution for platforms without the Linux sandbox backend.

use super::outcome::ProcessOutcome;
use super::policy::{validate_test_filter, OfflineCargoPolicy};
use super::result::ProcessResult;
use crate::tools::WorkspaceRoot;

pub(crate) fn run(
    _root: &WorkspaceRoot,
    _policy: &OfflineCargoPolicy,
    test_filter: Option<&str>,
) -> ProcessResult {
    if validate_test_filter(test_filter).is_err() {
        return ProcessResult::invalid_test_filter();
    }
    let mut result =
        ProcessResult::empty_with_filter(ProcessOutcome::SandboxUnavailable, test_filter);
    result.diagnostic = Some("process sandbox is unavailable on this platform".into());
    result
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::run;
    use crate::test_support::TempDir;
    use crate::tools::capture_workspace_root_at;
    use crate::{OfflineCargoPolicy, ProcessOutcome};
    use std::fs;

    #[test]
    fn sandboxed_cargo_returns_a_typed_unavailable_outcome() {
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
