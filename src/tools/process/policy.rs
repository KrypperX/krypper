//! The fixed offline Cargo policy: limits, test filters and the trusted toolchain profile.

use crate::tools::{capture_workspace_root_at, WorkspaceRoot};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub(super) const WALL: Duration = Duration::from_secs(180);
pub(super) const CPU_BUDGET: u64 = 120_000_000;
pub(super) const OUTPUT: usize = 256 * 1024;
pub(crate) const MAX_TEST_FILTER_BYTES: usize = 256;
pub(super) const MEMORY: u64 = 3 * 1024 * 1024 * 1024;
pub(super) const TASKS: u64 = 128;

/// Trusted host-selected read-only Rust toolchain and registry. Not a model request.
/// The directories must be provisioned outside every untrusted task workspace. System
/// `/usr`, `/lib` and `/lib64` are trusted read-only runtime inputs on supported Linux hosts.
#[derive(Debug)]
pub struct OfflineCargoPolicy {
    pub(super) toolchain: WorkspaceRoot,
    pub(super) registry: WorkspaceRoot,
    pub(super) cancelled: Arc<AtomicBool>,
}

impl OfflineCargoPolicy {
    /// Capture preprovisioned toolchain/registry identities. Does not install or fetch anything.
    /// The toolchain contains real `bin/cargo` and `bin/rustc` executables (not rustup shims).
    pub fn new(toolchain: &Path, registry: &Path) -> Result<Self, String> {
        let toolchain = capture_workspace_root_at(toolchain)?;
        let registry = capture_workspace_root_at(registry)?;
        for binary in ["cargo", "rustc"] {
            let mut magic = [0; 4];
            File::open(toolchain.path().join("bin").join(binary))
                .and_then(|mut f| f.read_exact(&mut magic))
                .map_err(|_| "toolchain binary unavailable")?;
            if magic != *b"\x7fELF" {
                return Err("real ELF toolchain required".into());
            }
        }
        Ok(Self {
            toolchain,
            registry,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// A trusted caller may cancel the current run. Cancellation never raises a limit.
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
}

/// No outcome except `Exited(0)` with complete output and confirmed cleanup is success.

pub(crate) fn validate_test_filter(test_filter: Option<&str>) -> Result<(), &'static str> {
    let Some(test_filter) = test_filter else {
        return Ok(());
    };
    if test_filter.is_empty() {
        return Err("test_filter must not be empty");
    }
    if test_filter.len() > MAX_TEST_FILTER_BYTES {
        return Err("test_filter exceeds the byte limit");
    }
    if test_filter.chars().any(char::is_control) {
        return Err("test_filter must not contain control characters");
    }
    if test_filter.trim_start().starts_with('-') {
        return Err("test_filter must not look like an option");
    }
    Ok(())
}

pub(super) fn cargo_command_args(test_filter: Option<&str>) -> Vec<OsString> {
    let mut args = [
        "--",
        "/toolchain/bin/cargo",
        "test",
        "--offline",
        "--locked",
        "--all-targets",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    if let Some(test_filter) = test_filter {
        args.push(OsString::from("--"));
        args.push(OsString::from(test_filter));
    }
    args
}
