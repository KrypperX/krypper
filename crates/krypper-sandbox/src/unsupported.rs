use crate::{SandboxOutcome, SandboxRunRequest, SandboxRunResult};

pub(super) fn run(request: SandboxRunRequest) -> SandboxRunResult {
    let mut result = SandboxRunResult::empty(request.run_id, SandboxOutcome::SandboxUnavailable, 0);
    result.diagnostic = Some("process sandbox is unavailable on this platform".into());
    result
}

#[cfg(test)]
mod tests {
    use crate::{run_offline_cargo, RunLimits, SandboxOutcome, SandboxRunRequest};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn unsupported_platform_returns_unavailable_without_directory_handles() {
        let request = SandboxRunRequest::new(
            Uuid::new_v4(),
            None,
            Some("selected_test".into()),
            RunLimits::offline_cargo_test(),
            Arc::new(AtomicBool::new(false)),
        );
        let result = run_offline_cargo(request);
        assert_eq!(result.outcome, SandboxOutcome::SandboxUnavailable);
        assert!(!result.test_evidence_available);
    }
}
