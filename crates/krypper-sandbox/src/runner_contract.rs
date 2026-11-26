//! Shared fixed Cargo invocation contract for the sandbox supervisor and runner.

pub const FIXED_CARGO_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
pub const MAX_TEST_FILTER_BYTES: usize = 256;
pub const RUNNER_MOUNT_FD: i32 = 206;
pub const RUNNER_MOUNT_PATH: &str = "/run/krypper-sandbox-runner";

pub fn is_valid_test_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.len() <= MAX_TEST_FILTER_BYTES
        && !filter.chars().any(char::is_control)
        && !filter.trim_start().starts_with('-')
}
