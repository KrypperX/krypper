//! Fixed, offline Cargo sandbox enforcement used by Krypper's process Tool.
//!
//! This crate deliberately accepts no executable, argv, environment, mount list, or grant.
//! The Product Tools layer owns authorization and passes only captured directory handles,
//! bounded limits, a test filter, and cancellation state.

use std::fs::File;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uuid::Uuid;

const MAX_WALL_MILLIS: u64 = 300_000;
const MAX_CPU_USEC: u64 = 240_000_000;
const MAX_MEMORY_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PROCESSES: u64 = 128;

#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod sandbox;
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
    pub(crate) test_filter: Option<String>,
    pub(crate) limits: RunLimits,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl SandboxRunRequest {
    /// Build a host-owned offline Cargo request. Missing descriptors fail closed at runtime.
    pub fn new(
        run_id: Uuid,
        directories: Option<DirectoryDescriptors>,
        test_filter: Option<String>,
        limits: RunLimits,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            run_id,
            directories,
            test_filter,
            limits,
            cancelled,
        }
    }
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
        }
    }
}

/// Execute the fixed offline Cargo test profile with the host-enforced sandbox.
pub fn run_offline_cargo(request: SandboxRunRequest) -> SandboxRunResult {
    #[cfg(target_os = "linux")]
    {
        linux::run(request)
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsupported::run(request)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod directory_descriptor_tests {
    use super::DirectoryDescriptors;
    use std::fs::{self, File};
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
}
