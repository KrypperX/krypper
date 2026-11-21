#[path = "agent_quality/mod.rs"]
mod agent_quality;

use agent_quality::fixture;
use agent_quality::harness::{
    self, config_digest, encode_report, fixture_request, ArmResult, FixtureRequest,
    HarnessProvenance, Report, Spec, EXIT_GATE, EXIT_OK, REDACTED, SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};

fn spec_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_quality/benchmark.json")
}

fn load_spec() -> Spec {
    Spec::load(&spec_path()).expect("load pinned benchmark spec")
}

fn provenance() -> HarnessProvenance {
    HarnessProvenance {
        version: "fixture-harness-1".to_owned(),
        sha: "1111111111111111111111111111111111111111".to_owned(),
        tool_version: "rustc fixture | cargo fixture".to_owned(),
        offline: true,
        provider_calls: 0,
    }
}

fn execute(request: &FixtureRequest) -> ArmResult {
    let results = fixture::run(request).expect("run bounded fixtures");
    harness::arm_result(request, results)
}

fn arms(spec: &Spec, tool_version: &str) -> (ArmResult, ArmResult) {
    let baseline = fixture_request("baseline", &spec.baseline, tool_version, &spec.fixtures);
    let candidate = fixture_request("candidate", &spec.candidate, tool_version, &spec.fixtures);
    (execute(&baseline), execute(&candidate))
}

fn run(spec: &Spec) -> (Report, i32, Vec<u8>) {
    let harness = provenance();
    let (baseline, candidate) = arms(spec, &harness.tool_version);
    let (report, exit_code) =
        harness::build_report(spec, harness, baseline, candidate).expect("build benchmark report");
    let bytes = encode_report(&report).expect("encode benchmark report");
    (report, exit_code, bytes)
}

#[test]
fn repeated_runs_are_byte_identical_and_secret_free() {
    let spec = load_spec();
    let (report, exit_code, bytes) = run(&spec);
    let (repeat_report, repeat_exit_code, repeat_bytes) = run(&spec);

    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(repeat_exit_code, EXIT_OK);
    assert_eq!(bytes, repeat_bytes);
    assert_eq!(report, repeat_report);
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(
        report
            .baseline
            .fixtures
            .iter()
            .find(|fixture| fixture.id == "artifact_creation")
            .expect("baseline artifact fixture")
            .actions,
        2
    );
    assert!(report.gate.passed);
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
        for fixture in &arm.fixtures {
            assert!(fixture.redactions > 0, "{} redaction probe", fixture.id);
            assert_eq!(fixture.secret_leaks, 0);
            for line in &fixture.evidence {
                assert!(!line.contains(&secret));
            }
        }
    }
}

#[test]
fn a_candidate_failure_produces_exit_code_two_with_a_gate_violation() {
    let spec = load_spec();
    let harness = provenance();
    let (baseline, mut candidate) = arms(&spec, &harness.tool_version);

    let artifact = candidate
        .fixtures
        .iter_mut()
        .find(|fixture| fixture.id == "artifact_creation")
        .expect("artifact fixture result");
    artifact.success = false;
    artifact.outside_write = true;
    artifact.secret_leaks = 1;
    artifact.safety_incidents = 1;

    let (report, exit_code) = harness::build_report(&spec, harness, baseline, candidate)
        .expect("build failed gate report");

    assert_eq!(exit_code, EXIT_GATE);
    assert!(!report.gate.passed);
    assert!(!report.gate.violations.is_empty());
    assert!(report
        .comparison
        .fixtures
        .iter()
        .any(|delta| delta.success_regressed));
}

#[test]
fn results_retain_sha_config_and_tool_version_provenance() {
    let mut spec = load_spec();
    let baseline_sha = "2222222222222222222222222222222222222222".to_owned();
    let candidate_sha = "3333333333333333333333333333333333333333".to_owned();
    spec.baseline.sha = baseline_sha.clone();
    spec.candidate.sha = candidate_sha.clone();

    let (report, exit_code, _) = run(&spec);
    let harness = provenance();

    assert_eq!(exit_code, EXIT_OK);
    assert_eq!(report.baseline.sha, baseline_sha);
    assert_eq!(report.candidate.sha, candidate_sha);
    assert_eq!(
        report.baseline.config_digest,
        config_digest(&spec.baseline.config)
    );
    assert_eq!(
        report.candidate.config_digest,
        config_digest(&spec.candidate.config)
    );
    assert_eq!(report.baseline.tool_version, harness.tool_version);
    assert_eq!(report.candidate.tool_version, harness.tool_version);
    assert_eq!(report.harness, harness);
    assert!(!report
        .baseline
        .config
        .secrets
        .iter()
        .any(|value| value == &spec.baseline.config.secrets[0]));
}
