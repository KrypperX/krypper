#[path = "agent_quality/mod.rs"]
mod agent_quality;

use agent_quality::fixture;
use agent_quality::harness::{
    self, config_digest, encode_report, fixture_request, merge_runs, ArmKind, ArmResult, Config,
    FindingSeverity, FixtureClass, HarnessProvenance, IntervalMethod, MeasurementScope,
    NumericDistribution, Report, Spec, ThresholdBasis, EXIT_GATE, EXIT_OK, REDACTED,
    SCHEMA_VERSION,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const TOOL_VERSION: &str = "rustc fixture | cargo fixture";

fn spec_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_quality/benchmark.json")
}

fn load_spec() -> Spec {
    Spec::load(&spec_path()).expect("load pinned benchmark spec")
}

fn provenance(spec: &Spec) -> HarnessProvenance {
    HarnessProvenance {
        version: "fixture-harness-2".to_owned(),
        sha: "1111111111111111111111111111111111111111".to_owned(),
        tool_version: TOOL_VERSION.to_owned(),
        fixture_revision: spec.fixture_revision.clone(),
        fixture_digest: "2222222222222222222222222222222222222222222222222222222222222222"
            .to_owned(),
        scope: MeasurementScope::ScriptedFixtures,
        repeat_runs: spec.repeat_runs,
        offline: true,
        provider_calls: 0,
    }
}

fn execute_run(spec: &Spec, arm: ArmKind, run: usize) -> ArmResult {
    let arm_spec = match arm {
        ArmKind::Baseline => &spec.baseline,
        ArmKind::Candidate => &spec.candidate,
    };
    let request = fixture_request(
        arm,
        arm_spec,
        TOOL_VERSION,
        &spec.fixture_revision,
        run,
        &spec.fixtures,
    );
    let results = fixture::run(&request).expect("run bounded fixtures");
    harness::arm_result(&request, results)
}

fn arms(spec: &Spec) -> (ArmResult, ArmResult) {
    let mut baseline = None;
    let mut candidate = None;
    for run in 0..spec.repeat_runs {
        let next_baseline = execute_run(spec, ArmKind::Baseline, run);
        let next_candidate = execute_run(spec, ArmKind::Candidate, run);
        match &mut baseline {
            Some(existing) => merge_runs(existing, next_baseline).expect("merge baseline runs"),
            None => baseline = Some(next_baseline),
        }
        match &mut candidate {
            Some(existing) => merge_runs(existing, next_candidate).expect("merge candidate runs"),
            None => candidate = Some(next_candidate),
        }
    }
    (
        baseline.expect("baseline runs"),
        candidate.expect("candidate runs"),
    )
}

fn run(spec: &Spec) -> (Report, i32, Vec<u8>) {
    let harness = provenance(spec);
    let (baseline, candidate) = arms(spec);
    let (report, exit_code) =
        harness::build_report(spec, harness, baseline, candidate).expect("build benchmark report");
    let bytes = encode_report(&report).expect("encode benchmark report");
    (report, exit_code, bytes)
}

fn normalize_latencies(report: &mut Report) {
    for arm in [&mut report.baseline, &mut report.candidate] {
        for run in &mut arm.runs {
            for fixture in &mut run.fixtures {
                fixture.latencies_ms = vec![0; fixture.latencies_ms.len()];
            }
        }
    }
    for distribution in &mut report.comparison.fixtures {
        distribution.baseline_latency_ms = NumericDistribution::from_values(&[0]);
        distribution.candidate_latency_ms = NumericDistribution::from_values(&[0]);
    }
}

fn fixture_mut(arm: &mut ArmResult, id: FixtureClass) -> Vec<&mut harness::FixtureResult> {
    arm.runs
        .iter_mut()
        .flat_map(|run| run.fixtures.iter_mut())
        .filter(|fixture| fixture.id == id)
        .collect()
}

#[test]
fn every_documented_scripted_fixture_class_runs_and_links_a_follow_up() {
    let mut spec = load_spec();
    spec.fixtures = FixtureClass::all();
    let (report, exit_code, _) = run(&spec);

    assert_eq!(exit_code, EXIT_OK);
    let documented: BTreeSet<FixtureClass> = spec.fixtures.iter().copied().collect();
    for arm in [&report.baseline, &report.candidate] {
        for run in &arm.runs {
            let observed: BTreeSet<FixtureClass> =
                run.fixtures.iter().map(|fixture| fixture.id).collect();
            assert_eq!(observed, documented);
            assert!(run.fixtures.iter().all(|fixture| fixture.success));
        }
    }
    assert_eq!(report.comparison.fixtures.len(), documented.len());
    for distribution in &report.comparison.fixtures {
        assert_eq!(distribution.baseline_success.runs, spec.repeat_runs);
        assert_eq!(distribution.candidate_success.successes, spec.repeat_runs);
        assert!(!distribution.follow_up.reference.is_empty());
        assert!(!distribution.follow_up.action.is_empty());
    }
}

#[test]
fn repeated_runs_are_deterministic_except_captured_latency() {
    let spec = load_spec();
    let (mut report, exit_code, _) = run(&spec);
    let (mut repeat_report, repeat_exit_code, _) = run(&spec);
    normalize_latencies(&mut report);
    normalize_latencies(&mut repeat_report);
    let bytes = encode_report(&report).expect("encode deterministic report");
    let repeat_bytes = encode_report(&repeat_report).expect("encode deterministic repeat report");

    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(repeat_exit_code, EXIT_OK);
    assert_eq!(bytes, repeat_bytes);
    assert_eq!(report, repeat_report);
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.harness.scope, MeasurementScope::ScriptedFixtures);
    assert_eq!(report.baseline.runs.len(), spec.repeat_runs);
    assert_eq!(report.candidate.runs.len(), spec.repeat_runs);
    for fixture in &report.baseline.runs[0].fixtures {
        assert_eq!(fixture.run, 0);
        assert!(fixture.attempts <= spec.baseline.config.max_attempts);
        assert!(fixture.latencies_ms.len() <= spec.baseline.config.max_attempts);
    }
    let encoded = String::from_utf8(bytes).expect("utf8 report");
    assert!(!encoded.contains("fixture-only-secret-3f9a2b"));
    assert!(encoded.contains(REDACTED));
}

#[test]
fn redaction_removes_every_configured_secret_from_evidence_and_report() {
    let mut spec = load_spec();
    let secret = "explicit-redaction-probe-9d4f".to_owned();
    spec.baseline.config.secrets = vec![secret.clone()];
    spec.candidate.config.secrets = vec![secret.clone()];

    let (report, exit_code, bytes) = run(&spec);
    let encoded = String::from_utf8(bytes).expect("utf8 report");

    assert_eq!(exit_code, EXIT_OK);
    assert!(!encoded.contains(&secret));
    assert!(encoded.contains(REDACTED));
    for arm in [&report.baseline, &report.candidate] {
        for run in &arm.runs {
            for fixture in &run.fixtures {
                assert!(
                    fixture.redactions > 0,
                    "{} redaction probe",
                    fixture.id.as_str()
                );
                assert_eq!(fixture.secret_leaks, 0);
                for line in &fixture.evidence {
                    assert!(!line.contains(&secret));
                }
            }
        }
    }
}

#[test]
fn transient_scripted_failures_are_retried_and_recorded() {
    let mut spec = load_spec();
    spec.fixtures = vec![FixtureClass::Timeout, FixtureClass::MalformedProviderOutput];
    spec.repeat_runs = 1;

    let (report, exit_code, _) = run(&spec);
    assert_eq!(exit_code, EXIT_OK);
    for arm in [&report.baseline, &report.candidate] {
        for fixture in &arm.runs[0].fixtures {
            assert!(fixture.success);
            assert_eq!(fixture.attempts, spec.candidate.config.max_attempts);
            assert_eq!(fixture.retries, 1);
            assert_eq!(fixture.latencies_ms.len(), fixture.attempts);
        }
    }
}

#[test]
fn duplicate_effect_signatures_are_counted() {
    let signatures = vec![
        "file_created:summary.txt".to_owned(),
        "file_created:summary.txt".to_owned(),
        "text_replaced:source.txt".to_owned(),
    ];
    assert_eq!(fixture::duplicate_effect_count(&signatures), 1);
    assert_eq!(fixture::duplicate_effect_count(&[]), 0);
}

#[test]
fn latency_is_captured_and_published_without_gating_the_report() {
    let spec = load_spec();
    let harness = provenance(&spec);
    let (mut baseline, mut candidate) = arms(&spec);
    for fixture in fixture_mut(&mut baseline, FixtureClass::ArtifactCreation) {
        fixture.latencies_ms = vec![9];
    }
    for fixture in fixture_mut(&mut candidate, FixtureClass::ArtifactCreation) {
        fixture.latencies_ms = vec![u64::MAX];
    }

    let (report, exit_code) =
        harness::build_report(&spec, harness, baseline, candidate).expect("build latency report");
    let distribution = report
        .comparison
        .fixtures
        .iter()
        .find(|entry| entry.id == FixtureClass::ArtifactCreation)
        .expect("artifact distribution");

    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(distribution.baseline_latency_ms.max, 9);
    assert_eq!(distribution.candidate_latency_ms.max, u64::MAX);
    assert!(report.gate.passed);
}

#[test]
fn repeat_runs_publish_distributions_and_wilson_intervals() {
    let spec = load_spec();
    let (report, exit_code, _) = run(&spec);

    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(report.harness.repeat_runs, 3);
    for distribution in &report.comparison.fixtures {
        assert_eq!(
            distribution.baseline_success.method,
            IntervalMethod::Wilson95
        );
        assert_eq!(
            distribution.candidate_success.method,
            IntervalMethod::Wilson95
        );
        assert!(
            distribution.candidate_success.interval_bps[0]
                <= distribution.candidate_success.rate_bps
        );
        assert!(
            distribution.candidate_success.interval_bps[1]
                >= distribution.candidate_success.rate_bps
        );
    }
}

#[test]
fn regression_thresholds_are_derived_from_observed_baseline_runs() {
    let spec = load_spec();
    let harness = provenance(&spec);
    let (mut baseline, mut candidate) = arms(&spec);
    for fixture in fixture_mut(&mut baseline, FixtureClass::ArtifactCreation) {
        fixture.cost_units = 2;
    }
    for fixture in fixture_mut(&mut candidate, FixtureClass::ArtifactCreation) {
        fixture.cost_units = 3;
    }

    let (report, exit_code) = harness::build_report(&spec, harness, baseline, candidate)
        .expect("build baseline-derived report");
    let threshold = report
        .comparison
        .thresholds
        .iter()
        .find(|entry| entry.id == FixtureClass::ArtifactCreation)
        .expect("artifact threshold");

    assert_eq!(exit_code, EXIT_GATE);
    assert_eq!(threshold.basis, ThresholdBasis::ObservedBaselineRuns);
    assert_eq!(threshold.max_cost_units, 2);
    assert!(report.gate.violations.iter().any(|violation| {
        violation.fixture == FixtureClass::ArtifactCreation
            && violation.reason.contains("observed baseline maximum 2")
    }));
}

#[test]
fn findings_link_gate_violations_to_concrete_follow_up_evidence() {
    let spec = load_spec();
    let harness = provenance(&spec);
    let (baseline, mut candidate) = arms(&spec);
    for fixture in fixture_mut(&mut candidate, FixtureClass::DeniedOperation) {
        fixture.success = false;
        fixture.outside_write = true;
        fixture.safety_incidents = 1;
    }

    let (report, exit_code) =
        harness::build_report(&spec, harness, baseline, candidate).expect("build finding report");
    let finding = report
        .comparison
        .findings
        .iter()
        .find(|entry| entry.fixture == FixtureClass::DeniedOperation)
        .expect("denied-operation finding");
    let follow_up = spec
        .follow_ups
        .iter()
        .find(|entry| entry.fixture == FixtureClass::DeniedOperation)
        .expect("denied-operation follow-up");

    assert_eq!(exit_code, EXIT_GATE);
    assert_eq!(finding.severity, FindingSeverity::Regression);
    assert_eq!(finding.follow_up, *follow_up);
    assert!(finding
        .evidence
        .iter()
        .all(|evidence| evidence.fixture == FixtureClass::DeniedOperation));
    assert!(!finding.evidence.is_empty());
    assert!(!finding.summary.is_empty());
}

#[test]
fn a_candidate_failure_produces_exit_code_two_with_a_gate_violation() {
    let spec = load_spec();
    let harness = provenance(&spec);
    let (baseline, mut candidate) = arms(&spec);
    for fixture in fixture_mut(&mut candidate, FixtureClass::ArtifactCreation) {
        fixture.success = false;
        fixture.secret_leaks = 1;
        fixture.safety_incidents = 1;
    }

    let (report, exit_code) = harness::build_report(&spec, harness, baseline, candidate)
        .expect("build failed gate report");

    assert_eq!(exit_code, EXIT_GATE);
    assert!(!report.gate.passed);
    assert!(!report.gate.violations.is_empty());
}

#[test]
fn provenance_and_typed_config_fields_are_retained_and_validated() {
    let mut spec = load_spec();
    let baseline_sha = "3333333333333333333333333333333333333333".to_owned();
    let candidate_sha = "4444444444444444444444444444444444444444".to_owned();
    let fixture_revision = "5555555555555555555555555555555555555555".to_owned();
    spec.baseline.sha = baseline_sha.clone();
    spec.candidate.sha = candidate_sha.clone();
    spec.fixture_revision = fixture_revision.clone();

    let (report, exit_code, _) = run(&spec);
    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(report.baseline.sha, baseline_sha);
    assert_eq!(report.candidate.sha, candidate_sha);
    assert_eq!(report.harness.fixture_revision, fixture_revision);
    assert_eq!(
        report.baseline.config_digest,
        config_digest(&spec.baseline.config)
    );
    assert_eq!(
        report.candidate.config_digest,
        config_digest(&spec.candidate.config)
    );
    assert_eq!(report.baseline.tool_version, TOOL_VERSION);
    assert!(report
        .baseline
        .config
        .secrets
        .iter()
        .all(|value| value == harness::REDACTED));

    let mut invalid = spec.candidate.config.clone();
    invalid.secrets = Vec::new();
    assert!(invalid.validate().is_err());
    invalid = spec.candidate.config.clone();
    invalid.action_budget = 0;
    assert!(invalid.validate().is_err());
    let _typed_config: Config = spec.candidate.config.clone();
}
