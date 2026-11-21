#[path = "../tests/agent_quality/mod.rs"]
mod agent_quality;

use agent_quality::harness::{
    self, ArmResult, FixtureRequest, FixtureResult, HarnessProvenance, Spec, EXIT_GATE, EXIT_OK,
    EXIT_USAGE,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
const FIXTURE_EXAMPLE: &str = "examples/agent_quality_fixture.rs";
const HARNESS_TESTS: &str = "tests/agent_quality";

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "krypper-aqb-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|error| format!("create benchmark root: {error}"))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy();
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(path.as_ref())
            .output();
    }
}

fn run_command(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("run command: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn canonical_sha(repo: &Path, sha: &str) -> Result<String, String> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("spec SHAs must be full 40-character commit hashes".to_owned());
    }
    let revision = format!("{sha}^{{commit}}");
    let resolved = git(repo, &["rev-parse", "--verify", &revision])?;
    if resolved.len() != 40 || !resolved.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git did not resolve a commit SHA".to_owned());
    }
    Ok(resolved)
}

fn tool_version() -> Result<String, String> {
    let rustc = run_command(Command::new("rustc").arg("--version"))?;
    let cargo = run_command(Command::new("cargo").arg("--version"))?;
    Ok(format!("{rustc} | {cargo}"))
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| format!("create harness copy: {error}"))?;
    for entry in fs::read_dir(source).map_err(|error| format!("read harness source: {error}"))? {
        let entry = entry.map_err(|error| format!("read harness entry: {error}"))?;
        let target = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|error| format!("inspect harness entry: {error}"))?
            .is_dir()
        {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .map_err(|error| format!("copy harness entry: {error}"))?;
        }
    }
    Ok(())
}

fn execute_arm(
    repo: &Path,
    root: &Path,
    arm: &str,
    request: &FixtureRequest,
) -> Result<ArmResult, String> {
    let sha = canonical_sha(repo, &request.sha)?;
    let worktree_path = root.join(format!("{arm}-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
    let worktree_arg = worktree_path.to_string_lossy();
    git(
        repo,
        &["worktree", "add", "--detach", worktree_arg.as_ref(), &sha],
    )?;
    let worktree = Worktree {
        repo: repo.to_path_buf(),
        path: worktree_path,
    };

    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    copy_dir_all(
        &source_root.join(HARNESS_TESTS),
        &worktree.path.join(HARNESS_TESTS),
    )?;
    fs::create_dir_all(worktree.path.join("examples"))
        .map_err(|error| format!("create fixture example directory: {error}"))?;
    fs::copy(
        source_root.join(FIXTURE_EXAMPLE),
        worktree.path.join(FIXTURE_EXAMPLE),
    )
    .map_err(|error| format!("copy fixture example: {error}"))?;

    let request_path = root.join(format!("{arm}-request.json"));
    let output_path = root.join(format!("{arm}-fixtures.json"));
    let request_bytes = serde_json::to_vec_pretty(request)
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

    let bytes = fs::read(&output_path).map_err(|error| format!("read fixture results: {error}"))?;
    let fixtures: Vec<FixtureResult> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse fixture results: {error}"))?;
    let mut provenance = request.clone();
    provenance.sha = sha;
    Ok(harness::arm_result(&provenance, fixtures))
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
        let root = TempRoot::new()?;
        let baseline_request =
            harness::fixture_request("baseline", &spec.baseline, &tool_version, &spec.fixtures);
        let candidate_request =
            harness::fixture_request("candidate", &spec.candidate, &tool_version, &spec.fixtures);
        let baseline = execute_arm(&repo, root.path(), "baseline", &baseline_request)?;
        let candidate = execute_arm(&repo, root.path(), "candidate", &candidate_request)?;
        let provenance = HarnessProvenance {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            sha: git(&repo, &["rev-parse", "HEAD"])?,
            tool_version,
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
