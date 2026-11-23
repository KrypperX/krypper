//! Opt-in Linux offline test profile. Host policy is never deserialized from model input.
//!
//! `policy` owns the fixed Cargo profile and its limits, `sandbox` owns the contained child
//! command, `supervisor` owns leases, pipes, reaping and the run entry points. This file is a
//! facade so every existing `crate::tools::process::*` path keeps working.

mod outcome;
mod policy;
mod result;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(target_os = "linux")]
mod supervisor;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(all(test, target_os = "linux"))]
mod host_fixtures;

#[cfg(all(test, target_os = "linux"))]
mod tests;

pub use self::outcome::{ProcessOutcome, ProcessResource};
pub use self::policy::OfflineCargoPolicy;
pub(crate) use self::policy::{validate_test_filter, MAX_TEST_FILTER_BYTES};
pub use self::result::ProcessResult;
#[cfg(target_os = "linux")]
pub(crate) use self::supervisor::run;
#[cfg(not(target_os = "linux"))]
pub(crate) use self::unsupported::run;

#[cfg(all(test, target_os = "linux"))]
use self::policy::cargo_command_args;
#[cfg(all(test, target_os = "linux"))]
use self::supervisor::{
    record_raw_output, record_test_evidence, run_with_bounds, run_without_containment,
    settle_before_launch, Supervisor,
};
#[cfg(all(test, target_os = "linux"))]
use crate::tools::capture_workspace_root_at;
#[cfg(all(test, target_os = "linux"))]
use std::fs;
#[cfg(all(test, target_os = "linux"))]
use std::path::{Path, PathBuf};
#[cfg(all(test, target_os = "linux"))]
use std::sync::atomic::Ordering;
#[cfg(all(test, target_os = "linux"))]
use std::time::Duration;
#[cfg(all(test, target_os = "linux"))]
use uuid::Uuid;

/// The effective user id of this process.
#[cfg(target_os = "linux")]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}
