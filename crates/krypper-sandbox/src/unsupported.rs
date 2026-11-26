use crate::{
    SandboxBackend, SandboxControl, SandboxExecutionBackend, SandboxIsolationEvidence,
    SandboxOutcome, SandboxRunRequest, SandboxRunResult,
};

struct Backend;

pub(super) fn backend() -> &'static dyn SandboxExecutionBackend {
    static BACKEND: Backend = Backend;
    &BACKEND
}

impl SandboxExecutionBackend for Backend {
    fn identity(&self) -> Option<SandboxBackend> {
        None
    }

    fn supports_control(&self, _control: SandboxControl) -> bool {
        false
    }

    fn run(&self, request: SandboxRunRequest) -> SandboxRunResult {
        run_unsupported(request)
    }
}

fn run_unsupported(request: SandboxRunRequest) -> SandboxRunResult {
    let mut result = SandboxRunResult::empty(request.run_id, SandboxOutcome::SandboxUnavailable, 0);
    result.diagnostic = Some("process sandbox is unavailable on this platform".into());
    let mut evidence = SandboxIsolationEvidence::new(None);
    evidence.unavailable(SandboxControl::SandboxRuntime);
    result.sandbox_evidence = Some(evidence);
    result
}

#[cfg(test)]
mod tests {
    use crate::{
        run_offline_cargo, RunLimits, SandboxControl, SandboxEvidenceStatus, SandboxOutcome,
        SandboxRunRequest,
    };
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
        let evidence = result.sandbox_evidence.expect("unsupported evidence");
        assert_eq!(evidence.backend(), None);
        assert!(evidence.controls().iter().any(|record| {
            record.control == SandboxControl::SandboxRuntime
                && record.status == SandboxEvidenceStatus::Unavailable
        }));
        assert!(!evidence.controls().iter().any(|record| {
            record.status == SandboxEvidenceStatus::Observed
                || record.status == SandboxEvidenceStatus::Applied
        }));
    }
}
