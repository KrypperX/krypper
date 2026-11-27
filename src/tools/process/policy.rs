//! Host-owned offline process policy, limits and the trusted toolchain profile.

use crate::tools::process_profile::ProcessProfile;
use crate::tools::FileIdentity;
#[cfg(target_os = "linux")]
use crate::tools::{capture_workspace_root_at, WorkspaceRoot};
use krypper_sandbox::OfflineReadOnlyExecRequest;
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

/// Why a host-owned offline Cargo policy could not be constructed.
#[derive(Debug)]
#[non_exhaustive]
pub enum OfflineCargoPolicyError {
    /// The preprovisioned Cargo toolchain root could not be captured.
    ToolchainRootUnavailable(String),
    /// The offline registry root could not be captured.
    RegistryRootUnavailable(String),
    /// A required toolchain binary could not be read.
    ToolchainBinaryUnavailable {
        /// The required executable name (`cargo` or `rustc`).
        binary: &'static str,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// A required toolchain binary did not contain an ELF header.
    ToolchainBinaryNotElf {
        /// The required executable name (`cargo` or `rustc`).
        binary: &'static str,
    },
    /// The fixed process profile failed its internal validation.
    InvalidFixedProfile(String),
}

impl std::fmt::Display for OfflineCargoPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolchainRootUnavailable(message) | Self::RegistryRootUnavailable(message) => {
                formatter.write_str(message)
            }
            Self::ToolchainBinaryUnavailable { .. } => {
                formatter.write_str("toolchain binary unavailable")
            }
            Self::ToolchainBinaryNotElf { .. } => {
                formatter.write_str("real ELF toolchain required")
            }
            Self::InvalidFixedProfile(reason) => {
                write!(formatter, "invalid fixed process profile: {reason}")
            }
        }
    }
}

impl std::error::Error for OfflineCargoPolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ToolchainBinaryUnavailable { source, .. } => Some(source),
            _ => None,
        }
    }
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
    ///
    /// # Errors
    ///
    /// Returns [`OfflineCargoPolicyError`] with a category for unavailable roots, unreadable or
    /// non-ELF toolchain binaries, or an invalid fixed profile. Binary I/O failures retain their
    /// underlying [`std::io::Error`] as the error source.
    pub fn new(_toolchain: &Path, _registry: &Path) -> Result<Self, OfflineCargoPolicyError> {
        #[cfg(target_os = "linux")]
        let toolchain = capture_workspace_root_at(_toolchain).map_err(|error| {
            OfflineCargoPolicyError::ToolchainRootUnavailable(error.message().to_owned())
        })?;
        #[cfg(target_os = "linux")]
        let registry = capture_workspace_root_at(_registry).map_err(|error| {
            OfflineCargoPolicyError::RegistryRootUnavailable(error.message().to_owned())
        })?;
        #[cfg(target_os = "linux")]
        {
            for binary in ["cargo", "rustc"] {
                let mut magic = [0; 4];
                File::open(toolchain.path().join("bin").join(binary))
                    .and_then(|mut f| f.read_exact(&mut magic))
                    .map_err(
                        |source| OfflineCargoPolicyError::ToolchainBinaryUnavailable {
                            binary,
                            source,
                        },
                    )?;
                if magic != *b"\x7fELF" {
                    return Err(OfflineCargoPolicyError::ToolchainBinaryNotElf { binary });
                }
            }
        }
        let profile = ProcessProfile::offline_cargo_test()
            .map_err(|error| OfflineCargoPolicyError::InvalidFixedProfile(format!("{error:?}")))?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            toolchain,
            #[cfg(target_os = "linux")]
            registry,
            cancelled: Arc::new(AtomicBool::new(false)),
            profile,
        })
    }

    /// Forks the captured host policy for a newly authorized grant.
    ///
    /// Directory descriptors and their identities remain pinned to the same trusted roots, but
    /// cancellation belongs to a single grant epoch. Replacing a source grant must therefore
    /// cancel its in-flight process without cancelling the exact request installed in its place.
    pub(crate) fn fork_for_new_grant(&self) -> Result<Self, &'static str> {
        #[cfg(target_os = "linux")]
        let (toolchain, registry) = (
            self.toolchain
                .duplicate()
                .map_err(|_| "trusted process policy is unavailable")?,
            self.registry
                .duplicate()
                .map_err(|_| "trusted process policy is unavailable")?,
        );

        Ok(Self {
            #[cfg(target_os = "linux")]
            toolchain,
            #[cfg(target_os = "linux")]
            registry,
            cancelled: Arc::new(AtomicBool::new(false)),
            profile: self.profile.clone(),
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
        let environment = krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        self.authorization_facts_for_requirements(requirements, environment)
    }

    pub(crate) fn authorization_facts_for_read_only_exec(
        &self,
        request: &OfflineReadOnlyExecRequest,
    ) -> Result<ProcessPolicyFacts, &'static str> {
        let requirements = request.requirements();
        let workspace_cwd =
            krypper_sandbox::runner_contract::workspace_cwd_path(request.working_directory())
                .ok_or("process working directory is invalid")?;
        let environment = krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_owned(),
                    if *name == "PWD" {
                        workspace_cwd.clone()
                    } else {
                        (*value).to_owned()
                    },
                )
            })
            .collect();
        self.authorization_facts_for_requirements(requirements, environment)
    }

    fn authorization_facts_for_requirements(
        &self,
        requirements: ValidatedExecutionRequirements,
        environment: Vec<(String, String)>,
    ) -> Result<ProcessPolicyFacts, &'static str> {
        let profile_limits = self.profile.limits();
        let profile_limits = if requirements.profile_id() == "offline_read_only_exec_v1" {
            requirements.profile_limits()
        } else {
            profile_limits
        };
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
        let executable = match requirements.executable() {
            krypper_sandbox::SandboxExecutable::Cargo => "/toolchain/bin/cargo",
            krypper_sandbox::SandboxExecutable::Rustc => "/toolchain/bin/rustc",
        };
        Ok(ProcessPolicyFacts {
            profile_version: requirements.version(),
            profile_id: requirements.profile_id().to_owned(),
            executable: executable.to_owned(),
            arguments: requirements.arguments().to_vec(),
            working_directory: requirements.working_directory().to_owned(),
            environment_names: requirements.environment_names().to_vec(),
            environment,
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
