use ring::digest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 2;
pub const REDACTED: &str = "[REDACTED]";
pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 1;
pub const EXIT_GATE: i32 = 2;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FixtureClass {
    ArtifactCreation,
    BugFix,
    CrossFileEdit,
    FailedTest,
    InterruptedExecution,
    DeniedOperation,
    Timeout,
    MalformedProviderOutput,
    TruncatedEvidence,
    Cancellation,
    CheckpointRestart,
    SandboxUnavailable,
    ReflectionCorrection,
}

impl FixtureClass {
    pub fn all() -> Vec<Self> {
        vec![
            Self::ArtifactCreation,
            Self::BugFix,
            Self::CrossFileEdit,
            Self::FailedTest,
            Self::InterruptedExecution,
            Self::DeniedOperation,
            Self::Timeout,
            Self::MalformedProviderOutput,
            Self::TruncatedEvidence,
            Self::Cancellation,
            Self::CheckpointRestart,
            Self::SandboxUnavailable,
            Self::ReflectionCorrection,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactCreation => "artifact_creation",
            Self::BugFix => "bug_fix",
            Self::CrossFileEdit => "cross_file_edit",
            Self::FailedTest => "failed_test",
            Self::InterruptedExecution => "interrupted_execution",
            Self::DeniedOperation => "denied_operation",
            Self::Timeout => "timeout",
            Self::MalformedProviderOutput => "malformed_provider_output",
            Self::TruncatedEvidence => "truncated_evidence",
            Self::Cancellation => "cancellation",
            Self::CheckpointRestart => "checkpoint_restart",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::ReflectionCorrection => "reflection_correction",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Reactive,
    Planned,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrant {
    ReadFile,
    CreateFile,
    ReplaceText,
    Cancel,
    Checkpoint,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    OfflineLocal,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    None,
    Scripted,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptedModel {
    FixtureV1,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptPolicy {
    FixedScript,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArmKind {
    Baseline,
    Candidate,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementScope {
    ScriptedFixtures,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationObservation {
    NotApplicable,
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntervalMethod {
    Wilson95,
    InsufficientSamples,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdBasis {
    ObservedBaselineRuns,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Regression,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FollowUp {
    pub fixture: FixtureClass,
    pub reference: String,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub schema_version: u32,
    pub fixture_revision: String,
    pub repeat_runs: usize,
    pub fixtures: Vec<FixtureClass>,
    pub baseline: ArmSpec,
    pub candidate: ArmSpec,
    pub follow_ups: Vec<FollowUp>,
}

impl Spec {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| format!("read spec: {error}"))?;
        let spec: Self =
            serde_json::from_slice(&bytes).map_err(|error| format!("parse spec: {error}"))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported spec schema version".to_owned());
        }
        if !is_full_sha(&self.fixture_revision) {
            return Err("fixture revision must be a full 40-character commit SHA".to_owned());
        }
        if !is_full_sha(&self.baseline.sha) || !is_full_sha(&self.candidate.sha) {
            return Err("arm SHAs must be full 40-character commit hashes".to_owned());
        }
        if self.repeat_runs == 0 {
            return Err("repeat_runs must be positive".to_owned());
        }
        let unique: BTreeSet<FixtureClass> = self.fixtures.iter().copied().collect();
        if self.fixtures.is_empty() || unique.len() != self.fixtures.len() {
            return Err("fixtures must be a non-empty unique set".to_owned());
        }
        self.baseline.config.validate()?;
        self.candidate.config.validate()?;
        for fixture in &self.fixtures {
            if !self
                .follow_ups
                .iter()
                .any(|entry| entry.fixture == *fixture)
            {
                return Err(format!(
                    "missing follow-up linkage for {}",
                    fixture.as_str()
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArmSpec {
    pub sha: String,
    pub config: Config,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    pub mode: ExecutionMode,
    pub action_budget: usize,
    pub cost_per_action: u64,
    pub max_attempts: usize,
    pub tool_grants: Vec<ToolGrant>,
    pub runtime: RuntimeKind,
    pub provider: ProviderKind,
    pub model: ScriptedModel,
    pub prompt_policy: PromptPolicy,
    #[serde(default)]
    pub secrets: Vec<String>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.action_budget == 0 {
            return Err("action_budget must be positive".to_owned());
        }
        if self.max_attempts == 0 {
            return Err("max_attempts must be positive".to_owned());
        }
        if self.secrets.is_empty() || self.secrets.iter().any(|secret| secret.is_empty()) {
            return Err("configured secrets must not be empty".to_owned());
        }
        if self.tool_grants.is_empty() {
            return Err("tool_grants must not be empty".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FixtureRequest {
    pub arm: ArmKind,
    pub sha: String,
    pub config: Config,
    pub config_digest: String,
    pub tool_version: String,
    pub fixture_revision: String,
    pub run: usize,
    pub fixtures: Vec<FixtureClass>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FixtureResult {
    pub id: FixtureClass,
    pub run: usize,
    pub success: bool,
    pub expected: String,
    pub observed_stop_reason: String,
    pub verification: VerificationObservation,
    pub scripted_outcome: String,
    pub actions: usize,
    pub cost_units: u64,
    pub tokens: u64,
    pub denied_operations: u64,
    pub redactions: u64,
    pub secret_leaks: u64,
    pub outside_write: bool,
    pub safety_incidents: u64,
    pub duplicate_effects: u64,
    pub attempts: usize,
    pub retries: usize,
    pub corrections: u64,
    pub latencies_ms: Vec<u64>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArmRun {
    pub index: usize,
    pub fixtures: Vec<FixtureResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArmResult {
    pub arm: ArmKind,
    pub sha: String,
    pub config_digest: String,
    pub config: Config,
    pub tool_version: String,
    pub fixture_revision: String,
    pub runs: Vec<ArmRun>,
}

impl ArmResult {
    pub fn fixture_values(&self, id: FixtureClass) -> Vec<&FixtureResult> {
        self.runs
            .iter()
            .flat_map(|run| run.fixtures.iter())
            .filter(|fixture| fixture.id == id)
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HarnessProvenance {
    pub version: String,
    pub sha: String,
    pub tool_version: String,
    pub fixture_revision: String,
    pub fixture_digest: String,
    pub scope: MeasurementScope,
    pub repeat_runs: usize,
    pub offline: bool,
    pub provider_calls: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NumericDistribution {
    pub min: u64,
    pub median: u64,
    pub max: u64,
}

impl NumericDistribution {
    pub fn from_values(values: &[u64]) -> Self {
        if values.is_empty() {
            return Self {
                min: 0,
                median: 0,
                max: 0,
            };
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        Self {
            min: sorted[0],
            median: sorted[sorted.len() / 2],
            max: sorted[sorted.len() - 1],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SuccessDistribution {
    pub successes: usize,
    pub runs: usize,
    pub rate_bps: u32,
    pub interval_bps: [u32; 2],
    pub method: IntervalMethod,
}

impl SuccessDistribution {
    pub fn from_values(successes: usize, runs: usize) -> Self {
        let rate_bps = if runs == 0 {
            0
        } else {
            ((successes as f64 / runs as f64) * 10_000.0).round() as u32
        };
        let (interval_bps, method) = if runs >= 2 {
            (wilson_interval(successes, runs), IntervalMethod::Wilson95)
        } else {
            ([rate_bps, rate_bps], IntervalMethod::InsufficientSamples)
        };
        Self {
            successes,
            runs,
            rate_bps,
            interval_bps,
            method,
        }
    }
}

fn wilson_interval(successes: usize, runs: usize) -> [u32; 2] {
    let n = runs as f64;
    let p = successes as f64 / n;
    let z = 1.96_f64;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let margin = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    [
        rate_bps((center - margin).max(0.0)),
        rate_bps((center + margin).min(1.0)),
    ]
}

fn rate_bps(rate: f64) -> u32 {
    (rate * 10_000.0).round() as u32
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FixtureDistribution {
    pub id: FixtureClass,
    pub baseline_success: SuccessDistribution,
    pub candidate_success: SuccessDistribution,
    pub baseline_latency_ms: NumericDistribution,
    pub candidate_latency_ms: NumericDistribution,
    pub baseline_cost_units: NumericDistribution,
    pub candidate_cost_units: NumericDistribution,
    pub baseline_retries: NumericDistribution,
    pub candidate_retries: NumericDistribution,
    pub follow_up: FollowUp,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegressionThreshold {
    pub id: FixtureClass,
    pub basis: ThresholdBasis,
    pub min_successes: usize,
    pub max_cost_units: u64,
    pub max_safety_incidents: u64,
    pub max_duplicate_effects: u64,
    pub max_retries: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EvidenceRef {
    pub run: usize,
    pub arm: ArmKind,
    pub fixture: FixtureClass,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub fixture: FixtureClass,
    pub summary: String,
    pub evidence: Vec<EvidenceRef>,
    pub follow_up: FollowUp,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Comparison {
    pub fixtures: Vec<FixtureDistribution>,
    pub thresholds: Vec<RegressionThreshold>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Violation {
    pub run: usize,
    pub arm: ArmKind,
    pub fixture: FixtureClass,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Gate {
    pub passed: bool,
    pub violations: Vec<Violation>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Report {
    pub schema_version: u32,
    pub harness: HarnessProvenance,
    pub baseline: ArmResult,
    pub candidate: ArmResult,
    pub comparison: Comparison,
    pub gate: Gate,
}

pub fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn option_value(args: &[String], name: &str) -> Result<String, String> {
    let mut positions = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == name)
        .map(|(index, _)| index);
    let index = positions
        .next()
        .ok_or_else(|| format!("missing option {name}"))?;
    if positions.next().is_some() {
        return Err(format!("duplicate option {name}"));
    }
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}

pub fn config_digest(config: &Config) -> String {
    let bytes = serde_json::to_vec(config).expect("serialize pinned config");
    let value = digest::digest(&digest::SHA256, &bytes);
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn fixture_request(
    arm: ArmKind,
    arm_spec: &ArmSpec,
    tool_version: &str,
    fixture_revision: &str,
    run: usize,
    fixtures: &[FixtureClass],
) -> FixtureRequest {
    FixtureRequest {
        arm,
        sha: arm_spec.sha.clone(),
        config: arm_spec.config.clone(),
        config_digest: config_digest(&arm_spec.config),
        tool_version: tool_version.to_owned(),
        fixture_revision: fixture_revision.to_owned(),
        run,
        fixtures: fixtures.to_vec(),
    }
}

pub fn redact(input: &str, secrets: &[String]) -> (String, u64) {
    let mut ordered: Vec<&String> = secrets.iter().filter(|secret| !secret.is_empty()).collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut output = input.to_owned();
    let mut count = 0;
    for secret in ordered {
        let occurrences = output.matches(secret.as_str()).count() as u64;
        if occurrences > 0 {
            output = output.replace(secret.as_str(), REDACTED);
            count += occurrences;
        }
    }
    (output, count)
}

pub fn public_config(config: &Config) -> Config {
    let mut public = config.clone();
    public.secrets = public
        .secrets
        .iter()
        .map(|secret| {
            if secret.is_empty() {
                secret.clone()
            } else {
                REDACTED.to_owned()
            }
        })
        .collect();
    public
}

pub fn arm_result(request: &FixtureRequest, fixtures: Vec<FixtureResult>) -> ArmResult {
    ArmResult {
        arm: request.arm,
        sha: request.sha.clone(),
        config_digest: request.config_digest.clone(),
        config: public_config(&request.config),
        tool_version: request.tool_version.clone(),
        fixture_revision: request.fixture_revision.clone(),
        runs: vec![ArmRun {
            index: request.run,
            fixtures,
        }],
    }
}

pub fn merge_runs(target: &mut ArmResult, next: ArmResult) -> Result<(), String> {
    if target.arm != next.arm
        || target.sha != next.sha
        || target.config_digest != next.config_digest
        || target.tool_version != next.tool_version
        || target.fixture_revision != next.fixture_revision
    {
        return Err("repeat arm results do not share pinned provenance".to_owned());
    }
    let [run] =
        <[ArmRun; 1]>::try_from(next.runs).map_err(|_| "expected one arm run".to_owned())?;
    if target
        .runs
        .iter()
        .any(|existing| existing.index == run.index)
    {
        return Err("duplicate repeat run index".to_owned());
    }
    target.runs.push(run);
    target.runs.sort_by_key(|run| run.index);
    Ok(())
}

pub fn build_report(
    spec: &Spec,
    harness: HarnessProvenance,
    baseline: ArmResult,
    candidate: ArmResult,
) -> Result<(Report, i32), String> {
    spec.validate()?;
    if baseline.sha != spec.baseline.sha
        || candidate.sha != spec.candidate.sha
        || baseline.config_digest != config_digest(&spec.baseline.config)
        || candidate.config_digest != config_digest(&spec.candidate.config)
        || baseline.tool_version != candidate.tool_version
        || baseline.fixture_revision != spec.fixture_revision
        || candidate.fixture_revision != spec.fixture_revision
        || harness.fixture_revision != spec.fixture_revision
        || harness.repeat_runs != spec.repeat_runs
    {
        return Err("arm results do not match the pinned provenance".to_owned());
    }
    if baseline.runs.len() != spec.repeat_runs || candidate.runs.len() != spec.repeat_runs {
        return Err("arm results do not contain the pinned number of runs".to_owned());
    }

    let mut distributions = Vec::new();
    let mut thresholds = Vec::new();
    let mut findings = Vec::new();
    let mut violations = Vec::new();

    for id in &spec.fixtures {
        let baseline_values = baseline.fixture_values(*id);
        let candidate_values = candidate.fixture_values(*id);
        if baseline_values.len() != spec.repeat_runs || candidate_values.len() != spec.repeat_runs {
            return Err(format!(
                "missing repeat fixture results for {}",
                id.as_str()
            ));
        }
        let follow_up = spec
            .follow_ups
            .iter()
            .find(|entry| entry.fixture == *id)
            .cloned()
            .ok_or_else(|| format!("missing follow-up linkage for {}", id.as_str()))?;

        let min_successes = usize::from(baseline_values.iter().all(|value| value.success));
        let max_cost_units = baseline_values
            .iter()
            .map(|value| value.cost_units)
            .max()
            .unwrap_or(0);
        let max_safety_incidents = baseline_values
            .iter()
            .map(|value| value.safety_incidents)
            .max()
            .unwrap_or(0);
        let max_duplicate_effects = baseline_values
            .iter()
            .map(|value| value.duplicate_effects)
            .max()
            .unwrap_or(0);
        let max_retries = baseline_values
            .iter()
            .map(|value| value.retries)
            .max()
            .unwrap_or(0);

        thresholds.push(RegressionThreshold {
            id: *id,
            basis: ThresholdBasis::ObservedBaselineRuns,
            min_successes,
            max_cost_units,
            max_safety_incidents,
            max_duplicate_effects,
            max_retries,
        });

        for value in &candidate_values {
            let mut reasons = Vec::new();
            if usize::from(value.success) < min_successes {
                reasons.push(format!(
                    "candidate successes {} below observed baseline successes {min_successes}",
                    usize::from(value.success)
                ));
            }
            if value.cost_units > max_cost_units {
                reasons.push(format!(
                    "candidate cost {} exceeded observed baseline maximum {max_cost_units}",
                    value.cost_units
                ));
            }
            if value.safety_incidents > max_safety_incidents {
                reasons.push(format!(
                    "candidate safety incidents {} exceeded observed baseline maximum {max_safety_incidents}",
                    value.safety_incidents
                ));
            }
            if value.duplicate_effects > max_duplicate_effects {
                reasons.push(format!(
                    "candidate duplicate effects {} exceeded observed baseline maximum {max_duplicate_effects}",
                    value.duplicate_effects
                ));
            }
            if value.retries > max_retries {
                reasons.push(format!(
                    "candidate retries {} exceeded observed baseline maximum {max_retries}",
                    value.retries
                ));
            }
            if value.actions > spec.candidate.config.action_budget {
                reasons.push(format!(
                    "candidate exceeded the action budget on {}",
                    id.as_str()
                ));
            }
            if value.outside_write || value.secret_leaks > 0 {
                reasons.push(format!(
                    "candidate safety incident on {} (outside_write={}, secret_leaks={})",
                    id.as_str(),
                    value.outside_write,
                    value.secret_leaks
                ));
            }
            for reason in reasons {
                violations.push(Violation {
                    run: value.run,
                    arm: ArmKind::Candidate,
                    fixture: *id,
                    reason,
                });
            }
        }

        for violation in violations
            .iter()
            .filter(|violation| violation.fixture == *id)
        {
            findings.push(Finding {
                severity: FindingSeverity::Regression,
                fixture: *id,
                summary: violation.reason.clone(),
                evidence: vec![EvidenceRef {
                    run: violation.run,
                    arm: ArmKind::Candidate,
                    fixture: *id,
                }],
                follow_up: follow_up.clone(),
            });
        }

        distributions.push(FixtureDistribution {
            id: *id,
            baseline_success: SuccessDistribution::from_values(
                baseline_values.iter().filter(|value| value.success).count(),
                baseline_values.len(),
            ),
            candidate_success: SuccessDistribution::from_values(
                candidate_values
                    .iter()
                    .filter(|value| value.success)
                    .count(),
                candidate_values.len(),
            ),
            baseline_latency_ms: NumericDistribution::from_values(
                &baseline_values
                    .iter()
                    .flat_map(|value| value.latencies_ms.iter().copied())
                    .collect::<Vec<_>>(),
            ),
            candidate_latency_ms: NumericDistribution::from_values(
                &candidate_values
                    .iter()
                    .flat_map(|value| value.latencies_ms.iter().copied())
                    .collect::<Vec<_>>(),
            ),
            baseline_cost_units: NumericDistribution::from_values(
                &baseline_values
                    .iter()
                    .map(|value| value.cost_units)
                    .collect::<Vec<_>>(),
            ),
            candidate_cost_units: NumericDistribution::from_values(
                &candidate_values
                    .iter()
                    .map(|value| value.cost_units)
                    .collect::<Vec<_>>(),
            ),
            baseline_retries: NumericDistribution::from_values(
                &baseline_values
                    .iter()
                    .map(|value| value.retries as u64)
                    .collect::<Vec<_>>(),
            ),
            candidate_retries: NumericDistribution::from_values(
                &candidate_values
                    .iter()
                    .map(|value| value.retries as u64)
                    .collect::<Vec<_>>(),
            ),
            follow_up,
        });
    }

    let gate = Gate {
        passed: violations.is_empty(),
        violations,
    };
    let report = Report {
        schema_version: SCHEMA_VERSION,
        harness,
        baseline,
        candidate,
        comparison: Comparison {
            fixtures: distributions,
            thresholds,
            findings,
        },
        gate,
    };

    let encoded =
        serde_json::to_string(&report).map_err(|error| format!("encode report: {error}"))?;
    let secrets: Vec<String> = spec
        .baseline
        .config
        .secrets
        .iter()
        .chain(spec.candidate.config.secrets.iter())
        .cloned()
        .collect();
    if secrets
        .iter()
        .any(|secret| !secret.is_empty() && encoded.contains(secret.as_str()))
    {
        return Err("a configured secret reached the report".to_owned());
    }

    let exit_code = if report.gate.passed {
        EXIT_OK
    } else {
        EXIT_GATE
    };
    Ok((report, exit_code))
}

pub fn encode_report(report: &Report) -> Result<Vec<u8>, String> {
    let mut bytes =
        serde_json::to_vec_pretty(report).map_err(|error| format!("encode report: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}
