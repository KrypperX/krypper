//! Opt-in Linux offline test profile. Host policy is never deserialized from model input.
//!
//! `policy` owns the fixed Cargo profile and its limits, `sandbox` owns the contained child
//! command, `supervisor` owns leases, pipes, reaping and the run entry points. This file is a
//! facade so every existing `crate::tools::process::*` path keeps working.

mod policy;
mod sandbox;
mod supervisor;

#[cfg(test)]
mod tests;

pub use self::policy::OfflineCargoPolicy;
pub(crate) use self::policy::{validate_test_filter, MAX_TEST_FILTER_BYTES};
pub(crate) use self::supervisor::run;
pub use self::supervisor::{ProcessOutcome, ProcessResource, ProcessResult};

#[cfg(test)]
use self::policy::{cargo_command_args, CPU_BUDGET, MEMORY, OUTPUT, TASKS, WALL};
#[cfg(test)]
use self::sandbox::sandbox;
#[cfg(test)]
use self::supervisor::{record_test_evidence, run_with_bounds, Supervisor};
#[cfg(test)]
use crate::tools::capture_workspace_root_at;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use uuid::Uuid;
