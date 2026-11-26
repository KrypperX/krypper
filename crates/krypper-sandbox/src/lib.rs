//! Fixed, offline Cargo sandbox enforcement used by Krypper's process Tool.
//!
//! The Product Tools layer owns authorization and passes a validated version 2 requirements
//! contract, captured directory handles, bounded effective limits, a test filter, and cancellation
//! state. The runner accepts only the exact fixed Cargo contract and does not consume caller paths
//! or environment values.

use serde::Serialize;
use std::fs::File;
use std::io;
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uuid::Uuid;

const MAX_WALL_MILLIS: u64 = 300_000;
const MAX_CPU_USEC: u64 = 240_000_000;
const MAX_MEMORY_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PROCESSES: u64 = 128;
const EXECUTION_REQUIREMENTS_VERSION: u32 = 2;
const MAX_PROFILE_ID_BYTES: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_ARGUMENTS: usize = 16;
const MAX_ENVIRONMENT_NAMES: usize = 16;

const FIXED_CARGO_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
const FIXED_CARGO_ENVIRONMENT_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "CARGO_HOME",
    "CARGO_BUILD_JOBS",
    "CARGO_PROFILE_DEV_DEBUG",
    "CARGO_PROFILE_TEST_DEBUG",
    "CARGO_INCREMENTAL",
    "CARGO_TARGET_DIR",
    "RUSTC",
    "LANG",
];
/// The values actually installed by the fixed offline Cargo sandbox, in launch order.
pub const OFFLINE_CARGO_TEST_ENVIRONMENT: &[(&str, &str)] = &[
    ("PATH", "/toolchain/bin:/usr/bin:/bin"),
    ("HOME", "/tmp/home"),
    ("TMPDIR", "/tmp"),
    ("CARGO_HOME", "/cargo"),
    ("CARGO_BUILD_JOBS", "1"),
    ("CARGO_PROFILE_DEV_DEBUG", "0"),
    ("CARGO_PROFILE_TEST_DEBUG", "0"),
    ("CARGO_INCREMENTAL", "0"),
    ("CARGO_TARGET_DIR", "/tmp/target"),
    ("RUSTC", "/toolchain/bin/rustc"),
    ("LANG", "C"),
];
const FIXED_CARGO_PROFILE_LIMITS: ProfileLimits = ProfileLimits {
    wall_secs: 180,
    cpu_ms: 120_000,
    memory_bytes: MAX_MEMORY_BYTES,
    output_bytes: MAX_OUTPUT_BYTES,
    processes: MAX_PROCESSES,
};

/// Resource ceilings expressed as seconds, milliseconds, and bytes at profile-definition time.
///
/// These are ceilings, not the effective limits for one run. Convert to [`RunLimits`] with
/// checked arithmetic before dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileLimits {
    pub wall_secs: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub output_bytes: usize,
    pub processes: u64,
}

impl ProfileLimits {
    /// Convert this profile ceiling to its corresponding effective runner limits.
    pub fn to_run_limits(self) -> Result<RunLimits, &'static str> {
        let wall_millis = self
            .wall_secs
            .checked_mul(1_000)
            .ok_or("profile wall-seconds conversion overflow")?;
        let cpu_budget_usec = self
            .cpu_ms
            .checked_mul(1_000)
            .ok_or("profile CPU-milliseconds conversion overflow")?;
        RunLimits::new(
            wall_millis,
            cpu_budget_usec,
            self.memory_bytes,
            self.output_bytes,
            self.processes,
        )
    }
}

/// Closed executable identities accepted by process profiles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxExecutable {
    Cargo,
    Rustc,
}

impl SandboxExecutable {
    #[cfg(target_os = "linux")]
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Cargo => "/toolchain/bin/cargo",
            Self::Rustc => "/toolchain/bin/rustc",
        }
    }
}

/// Logical host-captured directory roles. These identifiers never contain host paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinnedResourceId {
    Workspace,
    Toolchain,
    CargoRegistry,
}

/// Access requested for one pinned resource role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinnedResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// One logical resource role and its requested access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedResourceRequirement {
    pub id: PinnedResourceId,
    pub access: PinnedResourceAccess,
}

/// Network policy supported by the execution contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkRequirement {
    DenyAll,
}

/// Fixed syscall policy supported by the execution contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyscallRequirement {
    OfflineCargoTestV1,
}

/// Process-session behavior supported by the execution contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTreeRequirement {
    EphemeralTree,
}

/// Validation errors for a bounded host-owned execution requirements contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionRequirementsError {
    UnsupportedVersion,
    InvalidProfileId,
    InvalidArguments,
    InvalidWorkingDirectory,
    InvalidResources,
    InvalidEnvironmentNames,
    InvalidLimits,
}

/// A versioned, immutable execution requirements contract.
///
/// Its private fields prevent caller-built launch authority. Requirements describe what the
/// host-owned process profile requests; they are not sandbox result evidence and do not prove
/// that a backend applied or behaviorally enforced any control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedExecutionRequirements {
    version: u32,
    profile_id: String,
    executable: SandboxExecutable,
    arguments: Vec<String>,
    working_directory: String,
    resources: Vec<PinnedResourceRequirement>,
    environment_names: Vec<String>,
    profile_limits: ProfileLimits,
    network: NetworkRequirement,
    syscall: SyscallRequirement,
    process_tree: ProcessTreeRequirement,
}

impl ValidatedExecutionRequirements {
    /// Construct a bounded contract for a host-owned profile. The sandbox runner still accepts
    /// only the exact `offline_cargo_test` contract returned by its profile adapter.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        profile_id: impl Into<String>,
        executable: SandboxExecutable,
        arguments: Vec<String>,
        working_directory: impl Into<String>,
        resources: Vec<PinnedResourceRequirement>,
        environment_names: Vec<String>,
        profile_limits: ProfileLimits,
        network: NetworkRequirement,
        syscall: SyscallRequirement,
        process_tree: ProcessTreeRequirement,
    ) -> Result<Self, ExecutionRequirementsError> {
        let requirements = Self {
            version,
            profile_id: profile_id.into(),
            executable,
            arguments,
            working_directory: working_directory.into(),
            resources,
            environment_names,
            profile_limits,
            network,
            syscall,
            process_tree,
        };
        requirements.validate()?;
        Ok(requirements)
    }

    /// The fixed requirements used by the sole launchable process profile.
    pub fn offline_cargo_test() -> Self {
        Self::new(
            EXECUTION_REQUIREMENTS_VERSION,
            "offline_cargo_test",
            SandboxExecutable::Cargo,
            FIXED_CARGO_ARGUMENTS
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            ".",
            fixed_resource_requirements(),
            FIXED_CARGO_ENVIRONMENT_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            FIXED_CARGO_PROFILE_LIMITS,
            NetworkRequirement::DenyAll,
            SyscallRequirement::OfflineCargoTestV1,
            ProcessTreeRequirement::EphemeralTree,
        )
        .expect("fixed offline Cargo requirements are valid")
    }

    fn validate(&self) -> Result<(), ExecutionRequirementsError> {
        if self.version != EXECUTION_REQUIREMENTS_VERSION {
            return Err(ExecutionRequirementsError::UnsupportedVersion);
        }
        if self.profile_id.is_empty()
            || self.profile_id.len() > MAX_PROFILE_ID_BYTES
            || !self
                .profile_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ExecutionRequirementsError::InvalidProfileId);
        }
        if self.arguments.is_empty()
            || self.arguments.len() > MAX_ARGUMENTS
            || self.arguments.iter().any(|argument| {
                argument.is_empty()
                    || argument.len() > MAX_ARGUMENT_BYTES
                    || !argument.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(
                                byte,
                                b'.' | b'_' | b'-' | b'/' | b':' | b'=' | b',' | b'@' | b'+'
                            )
                    })
            })
        {
            return Err(ExecutionRequirementsError::InvalidArguments);
        }
        if !valid_workspace_relative_path(&self.working_directory) {
            return Err(ExecutionRequirementsError::InvalidWorkingDirectory);
        }
        if self.resources.is_empty()
            || self.resources.len() > 3
            || self.resources.iter().enumerate().any(|(index, resource)| {
                self.resources[..index]
                    .iter()
                    .any(|previous| previous.id == resource.id)
            })
        {
            return Err(ExecutionRequirementsError::InvalidResources);
        }
        if self.environment_names.is_empty()
            || self.environment_names.len() > MAX_ENVIRONMENT_NAMES
            || self
                .environment_names
                .iter()
                .enumerate()
                .any(|(index, name)| {
                    !FIXED_CARGO_ENVIRONMENT_NAMES.contains(&name.as_str())
                        || self.environment_names[..index].contains(name)
                })
        {
            return Err(ExecutionRequirementsError::InvalidEnvironmentNames);
        }
        if self.profile_limits.to_run_limits().is_err() {
            return Err(ExecutionRequirementsError::InvalidLimits);
        }
        Ok(())
    }

    /// Contract schema version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Host-owned profile identifier.
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Closed executable identity.
    pub fn executable(&self) -> SandboxExecutable {
        self.executable
    }

    /// Fixed argv prefix. Test filters are validated and appended separately.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Workspace-relative working directory.
    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    /// Logical host-captured resource roles and requested access.
    pub fn resources(&self) -> &[PinnedResourceRequirement] {
        &self.resources
    }

    /// Environment names only; this contract contains no caller-supplied values.
    pub fn environment_names(&self) -> &[String] {
        &self.environment_names
    }

    /// Hard profile ceilings, separate from one run's effective limits.
    pub fn profile_limits(&self) -> ProfileLimits {
        self.profile_limits
    }

    /// Requested network policy.
    pub fn network(&self) -> NetworkRequirement {
        self.network
    }

    /// Requested host syscall policy.
    pub fn syscall(&self) -> SyscallRequirement {
        self.syscall
    }

    /// Requested process-tree lifetime policy.
    pub fn process_tree(&self) -> ProcessTreeRequirement {
        self.process_tree
    }

    fn is_fixed_offline_cargo_test(&self) -> bool {
        self == &Self::offline_cargo_test()
    }
}

fn fixed_resource_requirements() -> Vec<PinnedResourceRequirement> {
    [
        PinnedResourceId::Workspace,
        PinnedResourceId::Toolchain,
        PinnedResourceId::CargoRegistry,
    ]
    .into_iter()
    .map(|id| PinnedResourceRequirement {
        id,
        access: PinnedResourceAccess::ReadOnly,
    })
    .collect()
}

fn valid_workspace_relative_path(value: &str) -> bool {
    value == "."
        || (!value.is_empty()
            && value.len() <= 4_096
            && !value.starts_with('/')
            && value
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'/' | b'.')
            }))
}

#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(not(target_os = "linux"))]
mod unsupported;

/// A finite set of hard bounds. The runner validates these against its own fixed ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunLimits {
    pub wall_millis: u64,
    pub cpu_budget_usec: u64,
    pub memory_bytes: u64,
    pub output_bytes: usize,
    pub processes: u64,
}

impl RunLimits {
    /// The fixed Product offline Cargo test profile limits.
    pub fn offline_cargo_test() -> Self {
        Self::new(
            180_000,
            120_000_000,
            3 * 1024 * 1024 * 1024,
            256 * 1024,
            128,
        )
        .expect("fixed offline Cargo limits are within the sandbox ceilings")
    }

    /// Construct limits within the runner's fixed upper bounds.
    pub fn new(
        wall_millis: u64,
        cpu_budget_usec: u64,
        memory_bytes: u64,
        output_bytes: usize,
        processes: u64,
    ) -> Result<Self, &'static str> {
        let limits = Self {
            wall_millis,
            cpu_budget_usec,
            memory_bytes,
            output_bytes,
            processes,
        };
        if wall_millis == 0
            || wall_millis > MAX_WALL_MILLIS
            || cpu_budget_usec == 0
            || cpu_budget_usec > MAX_CPU_USEC
            || memory_bytes == 0
            || memory_bytes > MAX_MEMORY_BYTES
            || output_bytes == 0
            || output_bytes > MAX_OUTPUT_BYTES
            || processes == 0
            || processes > MAX_PROCESSES
        {
            return Err("offline Cargo limits exceed the fixed sandbox bounds");
        }
        Ok(limits)
    }

    /// Ensure one effective run is non-zero, within runner bounds, and below its profile ceiling.
    pub fn validate_against(self, ceiling: ProfileLimits) -> Result<(), &'static str> {
        let effective = Self::new(
            self.wall_millis,
            self.cpu_budget_usec,
            self.memory_bytes,
            self.output_bytes,
            self.processes,
        )?;
        let ceiling = ceiling.to_run_limits()?;
        if effective.wall_millis > ceiling.wall_millis
            || effective.cpu_budget_usec > ceiling.cpu_budget_usec
            || effective.memory_bytes > ceiling.memory_bytes
            || effective.output_bytes > ceiling.output_bytes
            || effective.processes > ceiling.processes
        {
            return Err("effective limits exceed the profile ceiling");
        }
        Ok(())
    }
}

/// Captured directory descriptors for the authorized workspace and trusted offline runtime.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug)]
pub struct DirectoryDescriptors {
    workspace: File,
    toolchain: File,
    registry: File,
}

impl DirectoryDescriptors {
    /// Transfer ownership of already captured directory descriptors into the runner.
    pub fn new(workspace: File, toolchain: File, registry: File) -> Self {
        Self {
            workspace,
            toolchain,
            registry,
        }
    }

    fn for_resource(&self, id: PinnedResourceId) -> &File {
        match id {
            PinnedResourceId::Workspace => &self.workspace,
            PinnedResourceId::Toolchain => &self.toolchain,
            PinnedResourceId::CargoRegistry => &self.registry,
        }
    }

    fn matches_requirements(&self, requirements: &ValidatedExecutionRequirements) -> bool {
        requirements.resources().len() == 3
            && requirements.resources().iter().all(|resource| {
                resource.access == PinnedResourceAccess::ReadOnly
                    && self
                        .for_resource(resource.id)
                        .metadata()
                        .is_ok_and(|metadata| metadata.is_dir())
            })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn all_are_directories(&self) -> bool {
        [&self.workspace, &self.toolchain, &self.registry]
            .iter()
            .all(|directory| directory.metadata().is_ok_and(|metadata| metadata.is_dir()))
    }
}

/// The only request accepted by this runner: one bounded offline Cargo test attempt.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SandboxRunRequest {
    pub(crate) run_id: Uuid,
    pub(crate) directories: Option<DirectoryDescriptors>,
    pub(crate) requirements: ValidatedExecutionRequirements,
    pub(crate) test_filter: Option<String>,
    pub(crate) limits: RunLimits,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) spawn_gate: Option<SpawnAuthorizationGate>,
}

/// The action supplied by Sandbox to start its already-constructed payload command.
pub type SpawnAction = Box<dyn FnOnce(Command) -> io::Result<Child>>;

/// Product-owned authority callback invoked at the final payload effect boundary.
pub type SpawnAuthorizationGate =
    Box<dyn FnOnce(Command, SpawnAction) -> Result<Child, SpawnGateError>>;

/// Distinguishes an authorization denial from a host spawn failure.
pub enum SpawnGateError {
    Denied,
    Spawn(io::Error),
}

impl SandboxRunRequest {
    /// Build a host-owned offline Cargo request using the fixed compatibility adapter.
    pub fn new(
        run_id: Uuid,
        directories: Option<DirectoryDescriptors>,
        test_filter: Option<String>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_requirements(
            run_id,
            directories,
            test_filter,
            limits,
            cancelled,
            ValidatedExecutionRequirements::offline_cargo_test(),
        )
    }

    /// Build a request from a host-owned validated contract. The common runner boundary checks
    /// that it is the exact supported profile before platform dispatch or sandbox probing.
    pub fn new_with_requirements(
        run_id: Uuid,
        directories: Option<DirectoryDescriptors>,
        test_filter: Option<String>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
        requirements: ValidatedExecutionRequirements,
    ) -> Self {
        Self {
            run_id,
            directories,
            requirements,
            test_filter,
            limits,
            cancelled,
            spawn_gate: None,
        }
    }

    /// Attach the Product-owned authorization callback for the final payload start.
    pub fn with_spawn_gate(mut self, gate: SpawnAuthorizationGate) -> Self {
        self.spawn_gate = Some(gate);
        self
    }

    fn validate(&self) -> Result<(), &'static str> {
        if !self.requirements.is_fixed_offline_cargo_test() {
            return Err("execution requirements do not match the fixed profile");
        }
        self.limits
            .validate_against(self.requirements.profile_limits())?;
        if self
            .directories
            .as_ref()
            .is_some_and(|directories| !directories.matches_requirements(&self.requirements))
        {
            return Err("captured directory roles do not match execution requirements");
        }
        Ok(())
    }
}

pub(crate) fn validate_test_filter(test_filter: Option<&str>) -> Result<(), &'static str> {
    const MAX_TEST_FILTER_BYTES: usize = 256;
    let Some(test_filter) = test_filter else {
        return Ok(());
    };
    if test_filter.is_empty()
        || test_filter.len() > MAX_TEST_FILTER_BYTES
        || test_filter.chars().any(char::is_control)
        || test_filter.trim_start().starts_with('-')
    {
        return Err("test_filter is invalid");
    }
    Ok(())
}

/// Sandbox execution state; it does not make authorization decisions for Product Tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxOutcome {
    Denied,
    InvalidTestFilter,
    SandboxUnavailable,
    SandboxSetupFailed,
    SpawnFailed,
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Cancelled,
    ResourceLimitExceeded { resource: SandboxResource },
    OutputLimitExceeded,
    OutcomeUnknown,
}

/// Kernel resource limit observed by the supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxResource {
    Cpu,
    Memory,
    Processes,
    FileSize,
}

/// Identity of the runtime selected for one sandbox run.
///
/// Backend support is negotiated separately; this identity does not prove host setup succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    Bubblewrap,
}

/// Typed controls used for backend negotiation and per-run sandbox evidence.
///
/// Backend support is queried independently from the evidence records that describe one run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxControl {
    SandboxRuntime,
    UserNamespace,
    PidNamespace,
    NetworkNamespace,
    IpcNamespace,
    UtsNamespace,
    CgroupNamespace,
    SeccompFilter,
    CapabilityDrop,
    ReadOnlySystemMounts,
    ReadOnlyWorkspace,
    ReadOnlyToolchain,
    ReadOnlyRegistry,
    TemporaryFilesystems,
    DeviceMount,
    ProcMount,
    ReadOnlyRoot,
    ClearedEnvironment,
    ProcessLifecycle,
    MemoryLimit,
    SwapLimit,
    ProcessLimit,
    CpuLimit,
    WallClockLimit,
    CgroupCleanup,
}

// The only accepted execution contract is the fixed offline Cargo profile. Keep its required
// controls separate from per-run evidence and backend support so a successful match cannot be
// mistaken for controls that were applied on the current host.
const OFFLINE_CARGO_REQUIRED_CONTROLS: [SandboxControl; 25] = [
    SandboxControl::SandboxRuntime,
    SandboxControl::UserNamespace,
    SandboxControl::PidNamespace,
    SandboxControl::NetworkNamespace,
    SandboxControl::IpcNamespace,
    SandboxControl::UtsNamespace,
    SandboxControl::CgroupNamespace,
    SandboxControl::SeccompFilter,
    SandboxControl::CapabilityDrop,
    SandboxControl::ReadOnlySystemMounts,
    SandboxControl::ReadOnlyWorkspace,
    SandboxControl::ReadOnlyToolchain,
    SandboxControl::ReadOnlyRegistry,
    SandboxControl::TemporaryFilesystems,
    SandboxControl::DeviceMount,
    SandboxControl::ProcMount,
    SandboxControl::ReadOnlyRoot,
    SandboxControl::ClearedEnvironment,
    SandboxControl::ProcessLifecycle,
    SandboxControl::MemoryLimit,
    SandboxControl::SwapLimit,
    SandboxControl::ProcessLimit,
    SandboxControl::CpuLimit,
    SandboxControl::WallClockLimit,
    SandboxControl::CgroupCleanup,
];

/// A backend declares the controls it can establish for the fixed profile.
///
/// Capabilities describe backend support only. Host prerequisites, setup success and runtime
/// enforcement remain separate checks performed by the selected backend and recorded as result
/// evidence.
pub(crate) trait SandboxExecutionBackend {
    fn identity(&self) -> Option<SandboxBackend>;

    /// Report implementation support only; this must not probe or change host state.
    fn supports_control(&self, control: SandboxControl) -> bool;

    fn run(&self, request: SandboxRunRequest) -> SandboxRunResult;
}

/// Evidence state for a requested control. `Observed` is reserved for a direct behavioral probe;
/// ordinary command readiness never produces it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEvidenceStatus {
    Requested,
    Applied,
    Observed,
    Unavailable,
    Unknown,
}

/// One typed, bounded control record. The fixed enum prevents model or host text from becoming
/// control evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SandboxControlEvidence {
    pub control: SandboxControl,
    pub status: SandboxEvidenceStatus,
}

/// Bounded setup evidence carried with one sandbox result.
///
/// The runner can emit at most one record for each fixed control variant. Its private fields and
/// lack of `Deserialize` keep external data from populating policy-facing evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SandboxIsolationEvidence {
    backend: Option<SandboxBackend>,
    controls: Vec<SandboxControlEvidence>,
}

impl SandboxIsolationEvidence {
    const MAX_CONTROL_RECORDS: usize = OFFLINE_CARGO_REQUIRED_CONTROLS.len();

    /// Create an empty evidence value for serialization and result construction.
    pub fn new(backend: Option<SandboxBackend>) -> Self {
        Self {
            backend,
            controls: Vec::new(),
        }
    }

    /// Selected backend identity, absent when this platform has no sandbox backend.
    pub fn backend(&self) -> Option<SandboxBackend> {
        self.backend
    }

    /// Fixed control records in the order they were requested.
    pub fn controls(&self) -> &[SandboxControlEvidence] {
        &self.controls
    }

    pub(crate) fn request(&mut self, control: SandboxControl) {
        if self.controls.iter().any(|record| record.control == control) {
            return;
        }
        if self.controls.len() < Self::MAX_CONTROL_RECORDS {
            self.controls.push(SandboxControlEvidence {
                control,
                status: SandboxEvidenceStatus::Requested,
            });
        }
    }

    pub(crate) fn unavailable(&mut self, control: SandboxControl) {
        if let Some(record) = self
            .controls
            .iter_mut()
            .find(|record| record.control == control)
        {
            record.status = SandboxEvidenceStatus::Unavailable;
        } else if self.controls.len() < Self::MAX_CONTROL_RECORDS {
            self.controls.push(SandboxControlEvidence {
                control,
                status: SandboxEvidenceStatus::Unavailable,
            });
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn mark_applied(&mut self) {
        self.transition_requested(SandboxEvidenceStatus::Applied);
    }

    pub(crate) fn mark_unknown(&mut self) {
        self.transition_requested(SandboxEvidenceStatus::Unknown);
    }

    fn transition_requested(&mut self, status: SandboxEvidenceStatus) {
        for record in &mut self.controls {
            if record.status == SandboxEvidenceStatus::Requested {
                record.status = status;
            }
        }
    }
}

/// Data returned by the supervisor. Output stays as raw bytes until Product applies its policy.
#[derive(Debug)]
pub struct SandboxRunResult {
    pub run_id: Uuid,
    pub outcome: SandboxOutcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub diagnostic: Option<String>,
    pub cleanup_confirmed: bool,
    pub elapsed_ms: u64,
    pub output_bytes: usize,
    pub output_limit: usize,
    pub resource_events: Vec<SandboxResource>,
    pub recovery_hold: bool,
    pub test_evidence_available: bool,
    pub sandbox_evidence: Option<SandboxIsolationEvidence>,
}

impl SandboxRunResult {
    pub(crate) fn empty(run_id: Uuid, outcome: SandboxOutcome, output_limit: usize) -> Self {
        Self {
            run_id,
            outcome,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            diagnostic: None,
            cleanup_confirmed: true,
            elapsed_ms: 0,
            output_bytes: 0,
            output_limit,
            resource_events: Vec::new(),
            recovery_hold: false,
            test_evidence_available: false,
            sandbox_evidence: None,
        }
    }
}

/// Execute the fixed offline Cargo test profile with the host-enforced sandbox.
pub fn run_offline_cargo(request: SandboxRunRequest) -> SandboxRunResult {
    run_offline_cargo_with_backend(request, selected_backend())
}

fn selected_backend() -> &'static dyn SandboxExecutionBackend {
    #[cfg(target_os = "linux")]
    {
        linux::backend()
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsupported::backend()
    }
}

// This is the only runner dispatch point: validate and negotiate once, then invoke one backend.
fn run_offline_cargo_with_backend(
    request: SandboxRunRequest,
    backend: &dyn SandboxExecutionBackend,
) -> SandboxRunResult {
    let output_limit = if request.limits.output_bytes <= MAX_OUTPUT_BYTES {
        request.limits.output_bytes
    } else {
        0
    };
    if let Err(error) = request.validate() {
        let mut result =
            SandboxRunResult::empty(request.run_id, SandboxOutcome::Denied, output_limit);
        result.diagnostic = Some(error.into());
        return result;
    }
    if validate_test_filter(request.test_filter.as_deref()).is_err() {
        let mut result = SandboxRunResult::empty(
            request.run_id,
            SandboxOutcome::InvalidTestFilter,
            output_limit,
        );
        result.diagnostic = Some("test_filter is invalid".into());
        return result;
    }

    let missing = OFFLINE_CARGO_REQUIRED_CONTROLS
        .iter()
        .copied()
        .filter(|control| !backend.supports_control(*control))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let mut result = SandboxRunResult::empty(
            request.run_id,
            SandboxOutcome::SandboxUnavailable,
            output_limit,
        );
        result.diagnostic = Some(
            if backend.identity().is_none() {
                "process sandbox is unavailable on this platform"
            } else {
                "selected backend cannot provide required sandbox controls"
            }
            .into(),
        );

        let mut evidence = SandboxIsolationEvidence::new(backend.identity());
        for control in OFFLINE_CARGO_REQUIRED_CONTROLS {
            if missing.contains(&control) {
                evidence.unavailable(control);
            } else {
                evidence.request(control);
            }
        }
        evidence.mark_unknown();
        result.sandbox_evidence = Some(evidence);
        return result;
    }

    if request.spawn_gate.is_none() {
        let mut result =
            SandboxRunResult::empty(request.run_id, SandboxOutcome::Denied, output_limit);
        result.diagnostic = Some("product spawn authorization gate is missing".into());
        return result;
    }

    backend.run(request)
}

#[cfg(all(test, target_os = "linux"))]
mod directory_descriptor_tests {
    use super::{DirectoryDescriptors, PinnedResourceId};
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use uuid::Uuid;

    #[test]
    fn descriptors_must_refer_to_directories() {
        let root = std::env::temp_dir().join(format!("krypper-sandbox-fds-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let directory = root.join("directory");
        fs::create_dir(&directory).unwrap();
        let file = root.join("file");
        fs::write(&file, b"not a directory").unwrap();

        let valid = DirectoryDescriptors::new(
            File::open(&directory).unwrap(),
            File::open(&directory).unwrap(),
            File::open(&directory).unwrap(),
        );
        assert!(valid.all_are_directories());

        let invalid = DirectoryDescriptors::new(
            File::open(&directory).unwrap(),
            File::open(&file).unwrap(),
            File::open(&directory).unwrap(),
        );
        assert!(!invalid.all_are_directories());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_resource_ids_map_to_their_captured_descriptor_slots() {
        let root =
            std::env::temp_dir().join(format!("krypper-sandbox-resource-map-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("toolchain")).unwrap();
        fs::create_dir_all(root.join("registry")).unwrap();
        let workspace = File::open(root.join("workspace")).unwrap();
        let toolchain = File::open(root.join("toolchain")).unwrap();
        let registry = File::open(root.join("registry")).unwrap();
        let workspace_fd = workspace.as_raw_fd();
        let toolchain_fd = toolchain.as_raw_fd();
        let registry_fd = registry.as_raw_fd();
        let descriptors = DirectoryDescriptors::new(workspace, toolchain, registry);
        assert_eq!(
            descriptors
                .for_resource(PinnedResourceId::Workspace)
                .as_raw_fd(),
            workspace_fd
        );
        assert_eq!(
            descriptors
                .for_resource(PinnedResourceId::Toolchain)
                .as_raw_fd(),
            toolchain_fd
        );
        assert_eq!(
            descriptors
                .for_resource(PinnedResourceId::CargoRegistry)
                .as_raw_fd(),
            registry_fd
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod execution_contract_tests {
    use super::*;
    use std::fs::{self, File};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn requirements_with_profile_id(profile_id: &str) -> ValidatedExecutionRequirements {
        let fixed = ValidatedExecutionRequirements::offline_cargo_test();
        ValidatedExecutionRequirements::new(
            fixed.version(),
            profile_id,
            fixed.executable(),
            fixed.arguments().to_vec(),
            fixed.working_directory(),
            fixed.resources().to_vec(),
            fixed.environment_names().to_vec(),
            fixed.profile_limits(),
            fixed.network(),
            fixed.syscall(),
            fixed.process_tree(),
        )
        .unwrap()
    }

    fn request_with(
        requirements: ValidatedExecutionRequirements,
        limits: RunLimits,
    ) -> SandboxRunRequest {
        SandboxRunRequest::new_with_requirements(
            Uuid::new_v4(),
            None,
            None,
            limits,
            Arc::new(AtomicBool::new(false)),
            requirements,
        )
        .with_spawn_gate(Box::new(|command, spawn| {
            spawn(command).map_err(SpawnGateError::Spawn)
        }))
    }

    #[derive(Clone, Copy)]
    enum TestBackendBehavior {
        LaunchPayload,
        FailDuringSetup,
    }

    struct TestBackend {
        identity: Option<SandboxBackend>,
        controls: Vec<SandboxControl>,
        behavior: TestBackendBehavior,
        setup_attempts: AtomicUsize,
        payload_launches: AtomicUsize,
    }

    impl TestBackend {
        fn new(
            identity: Option<SandboxBackend>,
            controls: Vec<SandboxControl>,
            behavior: TestBackendBehavior,
        ) -> Self {
            Self {
                identity,
                controls,
                behavior,
                setup_attempts: AtomicUsize::new(0),
                payload_launches: AtomicUsize::new(0),
            }
        }
    }

    impl SandboxExecutionBackend for TestBackend {
        fn identity(&self) -> Option<SandboxBackend> {
            self.identity
        }

        fn supports_control(&self, control: SandboxControl) -> bool {
            self.controls.contains(&control)
        }

        fn run(&self, request: SandboxRunRequest) -> SandboxRunResult {
            self.setup_attempts.fetch_add(1, Ordering::SeqCst);
            if matches!(self.behavior, TestBackendBehavior::FailDuringSetup) {
                let mut result = SandboxRunResult::empty(
                    request.run_id,
                    SandboxOutcome::SandboxSetupFailed,
                    request.limits.output_bytes,
                );
                result.diagnostic = Some("injected backend setup failure".into());
                return result;
            }

            self.payload_launches.fetch_add(1, Ordering::SeqCst);
            SandboxRunResult::empty(
                request.run_id,
                SandboxOutcome::Exited(0),
                request.limits.output_bytes,
            )
        }
    }

    #[test]
    fn missing_backend_control_denies_before_backend_or_payload_launch() {
        let missing = SandboxControl::MemoryLimit;
        let controls = OFFLINE_CARGO_REQUIRED_CONTROLS
            .iter()
            .copied()
            .filter(|control| *control != missing)
            .collect();
        let backend = TestBackend::new(
            Some(SandboxBackend::Bubblewrap),
            controls,
            TestBackendBehavior::LaunchPayload,
        );

        let result = run_offline_cargo_with_backend(
            request_with(
                ValidatedExecutionRequirements::offline_cargo_test(),
                RunLimits::offline_cargo_test(),
            ),
            &backend,
        );

        assert_eq!(result.outcome, SandboxOutcome::SandboxUnavailable);
        assert_eq!(backend.setup_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(backend.payload_launches.load(Ordering::SeqCst), 0);
        let evidence = result.sandbox_evidence.expect("negotiation evidence");
        assert_eq!(evidence.backend(), Some(SandboxBackend::Bubblewrap));
        assert!(evidence.controls().iter().any(|record| {
            record.control == missing && record.status == SandboxEvidenceStatus::Unavailable
        }));
        assert!(evidence.controls().iter().any(|record| {
            record.control != missing && record.status == SandboxEvidenceStatus::Unknown
        }));
    }

    #[test]
    fn unsupported_backend_denies_before_backend_or_payload_launch() {
        let backend = TestBackend::new(None, Vec::new(), TestBackendBehavior::LaunchPayload);

        let result = run_offline_cargo_with_backend(
            request_with(
                ValidatedExecutionRequirements::offline_cargo_test(),
                RunLimits::offline_cargo_test(),
            ),
            &backend,
        );

        assert_eq!(result.outcome, SandboxOutcome::SandboxUnavailable);
        assert_eq!(
            result.diagnostic.as_deref(),
            Some("process sandbox is unavailable on this platform")
        );
        assert_eq!(backend.setup_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(backend.payload_launches.load(Ordering::SeqCst), 0);
        let evidence = result.sandbox_evidence.expect("unsupported evidence");
        assert_eq!(evidence.backend(), None);
        assert_eq!(
            evidence.controls().len(),
            OFFLINE_CARGO_REQUIRED_CONTROLS.len()
        );
        assert!(evidence
            .controls()
            .iter()
            .all(|record| record.status == SandboxEvidenceStatus::Unavailable));
    }

    #[test]
    fn missing_product_spawn_gate_denies_before_backend_or_payload_launch() {
        let backend = TestBackend::new(
            Some(SandboxBackend::Bubblewrap),
            OFFLINE_CARGO_REQUIRED_CONTROLS.to_vec(),
            TestBackendBehavior::LaunchPayload,
        );
        let request = SandboxRunRequest::new(
            Uuid::new_v4(),
            None,
            None,
            RunLimits::offline_cargo_test(),
            Arc::new(AtomicBool::new(false)),
        );

        let result = run_offline_cargo_with_backend(request, &backend);

        assert_eq!(result.outcome, SandboxOutcome::Denied);
        assert_eq!(
            result.diagnostic.as_deref(),
            Some("product spawn authorization gate is missing")
        );
        assert_eq!(backend.setup_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(backend.payload_launches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn backend_setup_failure_is_terminal_before_payload_launch() {
        let backend = TestBackend::new(
            Some(SandboxBackend::Bubblewrap),
            OFFLINE_CARGO_REQUIRED_CONTROLS.to_vec(),
            TestBackendBehavior::FailDuringSetup,
        );

        let gate_calls = Arc::new(AtomicUsize::new(0));
        let simulated_one_shot_consumed = Arc::new(AtomicBool::new(false));
        let gate_calls_for_gate = Arc::clone(&gate_calls);
        let consumed_for_gate = Arc::clone(&simulated_one_shot_consumed);
        let request = SandboxRunRequest::new(
            Uuid::new_v4(),
            None,
            None,
            RunLimits::offline_cargo_test(),
            Arc::new(AtomicBool::new(false)),
        )
        .with_spawn_gate(Box::new(move |command, spawn| {
            gate_calls_for_gate.fetch_add(1, Ordering::SeqCst);
            consumed_for_gate.store(true, Ordering::SeqCst);
            spawn(command).map_err(SpawnGateError::Spawn)
        }));

        let result = run_offline_cargo_with_backend(request, &backend);

        assert_eq!(result.outcome, SandboxOutcome::SandboxSetupFailed);
        assert_eq!(backend.setup_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.payload_launches.load(Ordering::SeqCst), 0);
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert!(!simulated_one_shot_consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn fixed_requirements_are_versioned_closed_and_host_owned() {
        let requirements = ValidatedExecutionRequirements::offline_cargo_test();
        assert_eq!(requirements.version(), EXECUTION_REQUIREMENTS_VERSION);
        assert_eq!(requirements.profile_id(), "offline_cargo_test");
        assert_eq!(requirements.executable(), SandboxExecutable::Cargo);
        assert_eq!(
            requirements.arguments(),
            &["test", "--offline", "--locked", "--all-targets"]
        );
        assert_eq!(requirements.working_directory(), ".");
        assert_eq!(
            requirements.resources(),
            &[
                PinnedResourceRequirement {
                    id: PinnedResourceId::Workspace,
                    access: PinnedResourceAccess::ReadOnly,
                },
                PinnedResourceRequirement {
                    id: PinnedResourceId::Toolchain,
                    access: PinnedResourceAccess::ReadOnly,
                },
                PinnedResourceRequirement {
                    id: PinnedResourceId::CargoRegistry,
                    access: PinnedResourceAccess::ReadOnly,
                },
            ]
        );
        assert_eq!(requirements.environment_names().len(), 11);
        assert_eq!(requirements.network(), NetworkRequirement::DenyAll);
        assert_eq!(
            requirements.syscall(),
            SyscallRequirement::OfflineCargoTestV1
        );
        assert_eq!(
            requirements.process_tree(),
            ProcessTreeRequirement::EphemeralTree
        );
    }

    #[test]
    fn rejects_unknown_execution_contract_versions_and_unsafe_fields() {
        let fixed = ValidatedExecutionRequirements::offline_cargo_test();
        assert_eq!(
            ValidatedExecutionRequirements::new(
                EXECUTION_REQUIREMENTS_VERSION + 1,
                "offline_cargo_test",
                fixed.executable(),
                fixed.arguments().to_vec(),
                fixed.working_directory(),
                fixed.resources().to_vec(),
                fixed.environment_names().to_vec(),
                fixed.profile_limits(),
                fixed.network(),
                fixed.syscall(),
                fixed.process_tree(),
            ),
            Err(ExecutionRequirementsError::UnsupportedVersion)
        );
        for cwd in ["/workspace", "../outside", "src/../tests", "a//b"] {
            assert_eq!(
                ValidatedExecutionRequirements::new(
                    fixed.version(),
                    fixed.profile_id(),
                    fixed.executable(),
                    fixed.arguments().to_vec(),
                    cwd,
                    fixed.resources().to_vec(),
                    fixed.environment_names().to_vec(),
                    fixed.profile_limits(),
                    fixed.network(),
                    fixed.syscall(),
                    fixed.process_tree(),
                ),
                Err(ExecutionRequirementsError::InvalidWorkingDirectory)
            );
        }
    }

    #[test]
    fn profile_ceilings_use_checked_unit_conversions_and_runner_bounds() {
        let ceiling = FIXED_CARGO_PROFILE_LIMITS;
        let converted = ceiling.to_run_limits().unwrap();
        assert_eq!(converted.wall_millis, 180_000);
        assert_eq!(converted.cpu_budget_usec, 120_000_000);
        assert!(RunLimits::new(1, 1, 1, 1, 1)
            .unwrap()
            .validate_against(ceiling)
            .is_ok());
        assert!(converted.validate_against(ceiling).is_ok());

        let over_wall = RunLimits {
            wall_millis: 180_001,
            ..converted
        };
        let over_cpu = RunLimits {
            cpu_budget_usec: 120_000_001,
            ..converted
        };
        let over_memory = RunLimits {
            memory_bytes: converted.memory_bytes + 1,
            ..converted
        };
        let over_output = RunLimits {
            output_bytes: converted.output_bytes + 1,
            ..converted
        };
        let over_processes = RunLimits {
            processes: converted.processes + 1,
            ..converted
        };
        for over in [
            over_wall,
            over_cpu,
            over_memory,
            over_output,
            over_processes,
        ] {
            assert!(over.validate_against(ceiling).is_err());
        }
        assert!(RunLimits {
            wall_millis: 0,
            ..converted
        }
        .validate_against(ceiling)
        .is_err());
        assert!(RunLimits {
            cpu_budget_usec: 0,
            ..converted
        }
        .validate_against(ceiling)
        .is_err());
        assert!(RunLimits {
            memory_bytes: 0,
            ..converted
        }
        .validate_against(ceiling)
        .is_err());
        assert!(RunLimits {
            output_bytes: 0,
            ..converted
        }
        .validate_against(ceiling)
        .is_err());
        assert!(RunLimits {
            processes: 0,
            ..converted
        }
        .validate_against(ceiling)
        .is_err());
        assert!(ProfileLimits {
            wall_secs: u64::MAX,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            cpu_ms: u64::MAX,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            wall_secs: 301,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            cpu_ms: 240_001,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            memory_bytes: MAX_MEMORY_BYTES + 1,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            output_bytes: MAX_OUTPUT_BYTES + 1,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
        assert!(ProfileLimits {
            processes: MAX_PROCESSES + 1,
            ..ceiling
        }
        .to_run_limits()
        .is_err());
    }

    #[test]
    fn request_contract_mismatch_is_denied_before_platform_dispatch() {
        let request = request_with(
            requirements_with_profile_id("other_profile"),
            RunLimits::offline_cargo_test(),
        );
        let result = run_offline_cargo(request);
        assert_eq!(result.outcome, SandboxOutcome::Denied);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert!(result.sandbox_evidence.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn captured_resource_type_mismatch_is_denied_before_platform_dispatch() {
        let root = std::env::temp_dir().join(format!(
            "krypper-sandbox-invalid-resource-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("toolchain")).unwrap();
        fs::create_dir_all(root.join("registry")).unwrap();
        fs::write(root.join("not-a-directory"), b"invalid resource").unwrap();
        let directories = DirectoryDescriptors::new(
            File::open(root.join("workspace")).unwrap(),
            File::open(root.join("not-a-directory")).unwrap(),
            File::open(root.join("registry")).unwrap(),
        );
        let request = SandboxRunRequest::new_with_requirements(
            Uuid::new_v4(),
            Some(directories),
            None,
            RunLimits::offline_cargo_test(),
            Arc::new(AtomicBool::new(false)),
            ValidatedExecutionRequirements::offline_cargo_test(),
        )
        .with_spawn_gate(Box::new(|command, spawn| {
            spawn(command).map_err(SpawnGateError::Spawn)
        }));
        let result = run_offline_cargo(request);
        assert_eq!(result.outcome, SandboxOutcome::Denied);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert!(result.sandbox_evidence.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lower_effective_limits_are_allowed_but_missing_handles_fail_closed() {
        let mut lower = RunLimits::offline_cargo_test();
        lower.wall_millis -= 1;
        lower.cpu_budget_usec -= 1;
        lower.output_bytes -= 1;
        let request = request_with(ValidatedExecutionRequirements::offline_cargo_test(), lower);
        let result = run_offline_cargo(request);
        assert_eq!(result.outcome, SandboxOutcome::SandboxUnavailable);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn effective_limit_errors_are_denied_before_platform_dispatch() {
        let requirements = ValidatedExecutionRequirements::offline_cargo_test();
        let ceiling = RunLimits::offline_cargo_test();
        for limits in [
            RunLimits {
                wall_millis: 0,
                ..ceiling
            },
            RunLimits {
                wall_millis: 180_001,
                ..ceiling
            },
            RunLimits {
                cpu_budget_usec: 0,
                ..ceiling
            },
            RunLimits {
                cpu_budget_usec: 120_000_001,
                ..ceiling
            },
            RunLimits {
                memory_bytes: 0,
                ..ceiling
            },
            RunLimits {
                memory_bytes: MAX_MEMORY_BYTES + 1,
                ..ceiling
            },
            RunLimits {
                output_bytes: 0,
                ..ceiling
            },
            RunLimits {
                output_bytes: MAX_OUTPUT_BYTES + 1,
                ..ceiling
            },
            RunLimits {
                processes: 0,
                ..ceiling
            },
            RunLimits {
                processes: MAX_PROCESSES + 1,
                ..ceiling
            },
            RunLimits {
                wall_millis: u64::MAX,
                ..ceiling
            },
        ] {
            let result = run_offline_cargo(request_with(requirements.clone(), limits));
            assert_eq!(result.outcome, SandboxOutcome::Denied);
            assert!(result.stdout.is_empty());
            assert!(result.stderr.is_empty());
            assert!(result.sandbox_evidence.is_none());
        }
    }
}
