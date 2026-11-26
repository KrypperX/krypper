//! The fixed offline Cargo policy: limits, test filters and the trusted toolchain profile.

use crate::tools::process_profile::ProcessProfile;
use crate::tools::FileIdentity;
#[cfg(target_os = "linux")]
use crate::tools::{capture_workspace_root_at, WorkspaceRoot};
use krypper_sandbox::{ProfileLimits, RunLimits, ValidatedExecutionRequirements};
#[cfg(all(test, target_os = "linux"))]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub(crate) const MAX_TEST_FILTER_BYTES: usize = 256;

/// Trusted host-selected read-only Rust toolchain and registry. Not a model request.
/// The directories must be provisioned outside every untrusted task workspace. System
/// `/usr`, `/lib` and `/lib64` are trusted read-only runtime inputs on supported Linux hosts.
#[derive(Debug)]
pub struct OfflineCargoPolicy {
    #[cfg(target_os = "linux")]
    pub(super) toolchain: WorkspaceRoot,
    #[cfg(target_os = "linux")]
    pub(super) registry: WorkspaceRoot,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) profile: ProcessProfile<'static>,
}

/// Closed host-owned policy details used to bind one process request to its approval.
#[derive(Clone, Debug)]
pub(crate) struct ProcessPolicyFacts {
    pub(crate) profile_version: u32,
    pub(crate) profile_id: String,
    pub(crate) executable: String,
    pub(crate) arguments: Vec<String>,
    pub(crate) working_directory: String,
    pub(crate) environment_names: Vec<String>,
    pub(crate) environment: Vec<(String, String)>,
    pub(crate) profile_limits: ProfileLimits,
    pub(crate) effective_limits: RunLimits,
    pub(crate) requirements: ValidatedExecutionRequirements,
    pub(crate) toolchain_identity: Option<FileIdentity>,
    pub(crate) registry_identity: Option<FileIdentity>,
}

impl OfflineCargoPolicy {
    /// Capture preprovisioned toolchain/registry identities on Linux. Does not install or fetch
    /// anything. Other platforms retain only cancellation state because execution is unavailable.
    /// The toolchain contains real `bin/cargo` and `bin/rustc` executables (not rustup shims).
    pub fn new(_toolchain: &Path, _registry: &Path) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let toolchain = capture_workspace_root_at(_toolchain)?;
        #[cfg(target_os = "linux")]
        let registry = capture_workspace_root_at(_registry)?;
        #[cfg(target_os = "linux")]
        {
            for binary in ["cargo", "rustc"] {
                let mut magic = [0; 4];
                File::open(toolchain.path().join("bin").join(binary))
                    .and_then(|mut f| f.read_exact(&mut magic))
                    .map_err(|_| "toolchain binary unavailable")?;
                if magic != *b"\x7fELF" {
                    return Err("real ELF toolchain required".into());
                }
            }
        }
        let profile = ProcessProfile::offline_cargo_test()
            .map_err(|error| format!("invalid fixed process profile: {error:?}"))?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            toolchain,
            #[cfg(target_os = "linux")]
            registry,
            cancelled: Arc::new(AtomicBool::new(false)),
            profile,
        })
    }

    /// A trusted caller may cancel the current run. Cancellation never raises a limit.
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub(super) fn profile(&self) -> &ProcessProfile<'static> {
        &self.profile
    }

    pub(crate) fn authorization_facts(&self) -> Result<ProcessPolicyFacts, &'static str> {
        let requirements = self
            .profile
            .execution_requirements()
            .cloned()
            .ok_or("process profile has no validated requirements")?;
        let profile_limits = self.profile.limits();
        let effective_limits = profile_limits
            .to_run_limits()
            .map_err(|_| "process profile limits are invalid")?;
        #[cfg(target_os = "linux")]
        let (toolchain_identity, registry_identity) = (
            self.toolchain
                .identity()
                .map_err(|_| "toolchain identity is unavailable")
                .map(Some)?,
            self.registry
                .identity()
                .map_err(|_| "registry identity is unavailable")
                .map(Some)?,
        );
        #[cfg(not(target_os = "linux"))]
        let (toolchain_identity, registry_identity) = (None, None);
        Ok(ProcessPolicyFacts {
            profile_version: self.profile.version(),
            profile_id: self.profile.id().to_owned(),
            executable: self.profile.executable().to_owned(),
            arguments: self
                .profile
                .arguments()
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            working_directory: self.profile.cwd().to_owned(),
            environment_names: self
                .profile
                .environment_allowlist()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            environment: krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            profile_limits,
            effective_limits,
            requirements,
            toolchain_identity,
            registry_identity,
        })
    }
}

// No outcome except `Exited(0)` with complete output and confirmed cleanup is success.

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

#[cfg(all(test, target_os = "linux"))]
pub(super) fn cargo_command_args(
    profile: &ProcessProfile<'_>,
    test_filter: Option<&str>,
) -> Vec<OsString> {
    let mut args = vec![OsString::from("--"), OsString::from(profile.executable())];
    args.extend(profile.arguments().iter().copied().map(OsString::from));
    if let Some(test_filter) = test_filter {
        args.push(OsString::from("--"));
        args.push(OsString::from(test_filter));
    }
    args
}
