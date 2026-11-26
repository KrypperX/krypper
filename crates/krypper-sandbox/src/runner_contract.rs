//! Shared fixed Cargo invocation contract for the sandbox supervisor and runner.

pub const FIXED_CARGO_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
pub const MAX_TEST_FILTER_BYTES: usize = 256;
pub const RUNNER_MOUNT_FD: i32 = 206;
pub const RUNNER_MOUNT_PATH: &str = "/run/krypper-sandbox-runner";
pub const WORKSPACE_MOUNT: (i32, &str) = (200, "/workspace");
pub const TOOLCHAIN_MOUNT: (i32, &str) = (201, "/toolchain");
pub const REGISTRY_MOUNT: (i32, &str) = (202, "/cargo/registry");
pub const RUNNER_MOUNT: (i32, &str) = (RUNNER_MOUNT_FD, RUNNER_MOUNT_PATH);
pub const RESOURCE_FD_MOUNTS: [(i32, &str); 3] = [WORKSPACE_MOUNT, TOOLCHAIN_MOUNT, REGISTRY_MOUNT];
pub const FIXED_FD_MOUNTS: [(i32, &str); 4] = [
    WORKSPACE_MOUNT,
    TOOLCHAIN_MOUNT,
    REGISTRY_MOUNT,
    RUNNER_MOUNT,
];

pub fn is_valid_test_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.len() <= MAX_TEST_FILTER_BYTES
        && !filter.chars().any(char::is_control)
        && !filter.trim_start().starts_with('-')
}
