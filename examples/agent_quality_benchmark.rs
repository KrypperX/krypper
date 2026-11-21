#[path = "../tests/agent_quality/mod.rs"]
mod agent_quality;

use agent_quality::harness::{
    self, fixture_request, merge_runs, ArmKind, ArmResult, HarnessProvenance, MeasurementScope,
    Spec, EXIT_GATE, EXIT_OK, EXIT_USAGE,
};
use agent_quality::support::{git, git_bytes, run_command, TempRoot};
use ring::digest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use uuid::Uuid;

const FIXTURE_FILES: &[&str] = &[
    "examples/agent_quality_fixture.rs",
    "tests/agent_quality/fixture.rs",
    "tests/agent_quality/harness.rs",
    "tests/agent_quality/mod.rs",
    "tests/agent_quality/support.rs",
];

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy();
        let _ = git(
            &self.repo,
            &["worktree", "remove", "--force", path.as_ref()],
        );
    }
}

fn canonical_sha(repo: &Path, sha: &str) -> Result<String, String> {
    if !harness::is_full_sha(sha) {
        return Err("spec SHAs must be full 40-character commit hashes".to_owned());
    }
    let revision = format!("{sha}^{{commit}}");
    let resolved = git(repo, &["rev-parse", "--verify", &revision])?;
    if !harness::is_full_sha(&resolved) {
        return Err("git did not resolve a commit SHA".to_owned());
    }
    Ok(resolved)
}

fn tool_version() -> Result<String, String> {
    let rustc = run_command(Command::new("rustc").arg("--version"))?;
    let cargo = run_command(Command::new("cargo").arg("--version"))?;
    Ok(format!("{rustc} | {cargo}"))
}

fn write_pinned_fixture_bundle(
    repo: &Path,
    fixture_revision: &str,
    worktree: &Path,
) -> Result<String, String> {
    let revision = canonical_sha(repo, fixture_revision)?;
    let mut context = digest::Context::new(&digest::SHA256);
    for path in FIXTURE_FILES {
        let object = format!("{revision}:{path}");
        let bytes = git_bytes(repo, &["show", &object])?;
        context.update(path.as_bytes());
        context.update(&[0]);
        context.update(&bytes);
        let target = worktree.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create pinned fixture directory: {error}"))?;
        }
        fs::write(&target, &bytes)
            .map_err(|error| format!("write pinned fixture file: {error}"))?;
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn parse_fixture_results(path: &Path) -> Result<Vec<harness::FixtureResult>, String> {
    let bytes = fs::read(path).map_err(|error| format!("read fixture results: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse fixture results: {error}"))
}

fn execute_arm(
    repo: &Path,
    root: &Path,
    spec: &Spec,
    arm: ArmKind,
    tool_version: &str,
) -> Result<(ArmResult, String), String> {
    let arm_spec = match arm {
        ArmKind::Baseline => &spec.baseline,
        ArmKind::Candidate => &spec.candidate,
    };
    let sha = canonical_sha(repo, &arm_spec.sha)?;
    let worktree_path = root.join(format!("{arm:?}-{}", Uuid::new_v4()));
    let worktree_arg = worktree_path.to_string_lossy();
    git(
        repo,
        &["worktree", "add", "--detach", worktree_arg.as_ref(), &sha],
    )?;
    let worktree = Worktree {
        repo: repo.to_path_buf(),
        path: worktree_path,
    };
    let fixture_digest = write_pinned_fixture_bundle(repo, &spec.fixture_revision, &worktree.path)?;

    let mut arm_result = None;
    for run in 0..spec.repeat_runs {
        let request = fixture_request(
            arm,
            arm_spec,
            tool_version,
            &spec.fixture_revision,
            run,
            &spec.fixtures,
        );
        let request_path = root.join(format!("{arm:?}-{run}-request.json"));
        let output_path = root.join(format!("{arm:?}-{run}-fixtures.json"));
        let request_bytes = serde_json::to_vec_pretty(&request)
            .map_err(|error| format!("encode fixture request: {error}"))?;
        fs::write(&request_path, request_bytes)
            .map_err(|error| format!("write fixture request: {error}"))?;

        let config_arg = request_path.to_string_lossy();
        let output_arg = output_path.to_string_lossy();
        run_command(
            Command::new("cargo")
                .args([
                    "run",
                    "--quiet",
                    "--offline",
                    "--locked",
                    "--example",
                    "agent_quality_fixture",
                    "--",
                    "--config",
                    config_arg.as_ref(),
                    "--out",
                    output_arg.as_ref(),
                ])
                .current_dir(&worktree.path)
                .env("CARGO_NET_OFFLINE", "true")
                .env("CARGO_TERM_COLOR", "never"),
        )?;

        let fixtures = parse_fixture_results(&output_path)?;
        let run_result = harness::arm_result(&request, fixtures);
        match &mut arm_result {
            Some(existing) => merge_runs(existing, run_result)?,
            None => arm_result = Some(run_result),
        }
    }

    Ok((
        arm_result.ok_or_else(|| "no fixture runs recorded".to_owned())?,
        fixture_digest,
    ))
}

fn main() -> ExitCode {
    let result = (|| -> Result<i32, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.len() != 4 {
            return Err("usage: agent_quality_benchmark --spec <path> --out <path>".to_owned());
        }
        let spec_path = PathBuf::from(harness::option_value(&args, "--spec")?);
        let out_path = PathBuf::from(harness::option_value(&args, "--out")?);
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let spec = Spec::load(&spec_path)?;
        let tool_version = tool_version()?;
        let root = TempRoot::new("benchmark")?;
        let (baseline, baseline_digest) =
            execute_arm(&repo, root.path(), &spec, ArmKind::Baseline, &tool_version)?;
        let (candidate, candidate_digest) =
            execute_arm(&repo, root.path(), &spec, ArmKind::Candidate, &tool_version)?;
        if baseline_digest != candidate_digest {
            return Err("pinned fixture bundle differed between arms".to_owned());
        }
        let provenance = HarnessProvenance {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            sha: git(&repo, &["rev-parse", "HEAD"])?,
            tool_version,
            fixture_revision: spec.fixture_revision.clone(),
            fixture_digest: baseline_digest,
            scope: MeasurementScope::ScriptedFixtures,
            repeat_runs: spec.repeat_runs,
            offline: true,
            provider_calls: 0,
        };
        let (report, exit_code) = harness::build_report(&spec, provenance, baseline, candidate)?;
        let bytes = harness::encode_report(&report)?;
        fs::write(&out_path, bytes).map_err(|error| format!("write report: {error}"))?;
        println!("{}", out_path.display());
        Ok(exit_code)
    })();

    match result {
        Ok(EXIT_OK) => ExitCode::SUCCESS,
        Ok(EXIT_GATE) => ExitCode::from(EXIT_GATE as u8),
        Ok(_) => ExitCode::from(EXIT_USAGE as u8),
        Err(error) => {
            eprintln!("agent_quality_benchmark: {error}");
            ExitCode::from(EXIT_USAGE as u8)
        }
    }
}
