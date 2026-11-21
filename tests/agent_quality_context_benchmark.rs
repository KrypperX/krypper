mod agent_quality {
    #[path = "context_fixture.rs"]
    pub mod context_fixture;
    #[path = "fixture.rs"]
    pub mod fixture;
    #[path = "harness.rs"]
    pub mod harness;
    #[path = "support.rs"]
    pub mod support;
}

use agent_quality::harness::{
    self, fixture_request, ArmKind, ArmResult, FixtureClass, HarnessProvenance, MeasurementScope,
    Spec, EXIT_OK,
};
use std::path::Path;

const TOOL_VERSION: &str = "context-slice-fixture-tool";
const HEAD_SHA: &str = "68b19809db5f2f29a3395a686a7fd6c1c0c69073";

fn spec() -> Spec {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_quality/benchmark.json");
    let mut spec = Spec::load(&path).expect("load pinned benchmark spec");
    spec.fixtures = vec![FixtureClass::BugFix];
    spec.repeat_runs = 1;
    spec
}

fn execute(spec: &Spec, arm: ArmKind) -> ArmResult {
    let arm_spec = match arm {
        ArmKind::Baseline => &spec.baseline,
        ArmKind::Candidate => &spec.candidate,
    };
    let request = fixture_request(
        arm,
        arm_spec,
        TOOL_VERSION,
        &spec.fixture_revision,
        0,
        &spec.fixtures,
    );
    let fixtures = match arm {
        ArmKind::Baseline => agent_quality::fixture::run(&request),
        ArmKind::Candidate => agent_quality::context_fixture::run(&request),
    }
    .expect("run matched-budget fixture");
    harness::arm_result(&request, fixtures)
}

#[test]
fn matched_budget_context_slice_preserves_the_existing_gate() {
    let spec = spec();
    let baseline = execute(&spec, ArmKind::Baseline);
    let candidate = execute(&spec, ArmKind::Candidate);
    let provenance = HarnessProvenance {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        sha: HEAD_SHA.to_owned(),
        tool_version: TOOL_VERSION.to_owned(),
        fixture_revision: spec.fixture_revision.clone(),
        fixture_digest: "evidence-only-context-slice".to_owned(),
        scope: MeasurementScope::ScriptedFixtures,
        repeat_runs: spec.repeat_runs,
        offline: true,
        provider_calls: 0,
    };

    let (report, exit_code) =
        harness::build_report(&spec, provenance, baseline.clone(), candidate.clone())
            .expect("build matched-budget evidence report");
    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(report.gate.violations.len(), 0);

    let baseline_fixture = &baseline.fixture_values(FixtureClass::BugFix)[0];
    let candidate_fixture = &candidate.fixture_values(FixtureClass::BugFix)[0];
    assert_eq!(baseline_fixture.actions, candidate_fixture.actions);
    assert_eq!(baseline_fixture.cost_units, candidate_fixture.cost_units);
    assert!(baseline_fixture.success && candidate_fixture.success);
    assert_eq!(candidate_fixture.denied_operations, 0);
    assert_eq!(candidate_fixture.safety_incidents, 0);

    let evidence = &candidate_fixture.evidence;
    assert!(evidence.iter().any(|line| line == "context_matches=1"));
    assert!(evidence
        .iter()
        .any(|line| line == "context_oversized_skipped=1"));
    assert!(evidence.iter().any(|line| {
        line.strip_prefix("context_neutralized_lines=")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|value| value > 0)
    }));
    assert!(evidence.iter().any(|line| {
        line.strip_prefix("context_bytes=")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|value| value <= 4096)
    }));
}
