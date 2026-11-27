//! Shared fixed Cargo invocation contract for the sandbox supervisor and runner.

pub const FIXED_CARGO_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
pub const READ_ONLY_EXEC_PROFILE_ID: &str = "offline_read_only_exec_v1";
pub const MAX_READ_ONLY_EXEC_ARGUMENTS: usize = 32;
pub const MAX_READ_ONLY_EXEC_ARGUMENT_BYTES: usize = 1_024;
pub const MAX_READ_ONLY_EXEC_ARGUMENT_TOTAL_BYTES: usize = 4_096;
pub const MAX_TEST_FILTER_BYTES: usize = 256;
pub const RUNNER_MOUNT_FD: i32 = 206;
pub const RUNNER_MOUNT_PATH: &str = "/run/krypper-sandbox-runner";
pub const WORKSPACE_MOUNT: (i32, &str) = (200, "/workspace");
pub const TOOLCHAIN_MOUNT: (i32, &str) = (201, "/toolchain");
pub const REGISTRY_MOUNT: (i32, &str) = (202, "/cargo/registry");
/// Descriptor used to bind a separately pinned workspace working directory for direct argv.
pub const WORKSPACE_CWD_MOUNT_FD: i32 = 207;
pub const RUNNER_MOUNT: (i32, &str) = (RUNNER_MOUNT_FD, RUNNER_MOUNT_PATH);
pub const OFFLINE_CARGO_HOME: &str = "/tmp/home";
pub const OFFLINE_CARGO_TARGET_DIR: &str = "/tmp/target";
pub const OFFLINE_CARGO_TMPDIR: &str = "/tmp/tmp";
pub const OFFLINE_CARGO_REGISTRY: &str = "/tmp/home/registry";
pub const RESOURCE_FD_MOUNTS: [(i32, &str); 3] = [WORKSPACE_MOUNT, TOOLCHAIN_MOUNT, REGISTRY_MOUNT];
pub const FIXED_FD_MOUNTS: [(i32, &str); 4] = [
    WORKSPACE_MOUNT,
    TOOLCHAIN_MOUNT,
    REGISTRY_MOUNT,
    RUNNER_MOUNT,
];

/// Resolve the sandbox path for a validated workspace-relative cwd.
pub fn workspace_cwd_path(working_directory: &str) -> Option<String> {
    if !is_valid_workspace_relative_path(working_directory) {
        return None;
    }
    Some(if working_directory == "." {
        WORKSPACE_MOUNT.1.to_owned()
    } else {
        format!("{}/{working_directory}", WORKSPACE_MOUNT.1)
    })
}

/// The profile's exact bind destinations, including the cwd pin only below the workspace root.
pub fn read_only_exec_fd_mounts(working_directory: &str) -> Option<Vec<(i32, String)>> {
    let cwd_path = workspace_cwd_path(working_directory)?;
    let mut mounts = RESOURCE_FD_MOUNTS
        .iter()
        .map(|(fd, path)| (*fd, (*path).to_owned()))
        .collect::<Vec<_>>();
    if working_directory != "." {
        mounts.push((WORKSPACE_CWD_MOUNT_FD, cwd_path));
    }
    mounts.push((RUNNER_MOUNT_FD, RUNNER_MOUNT_PATH.to_owned()));
    Some(mounts)
}

pub fn is_valid_test_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.len() <= MAX_TEST_FILTER_BYTES
        && !filter.chars().any(char::is_control)
        && !filter.trim_start().starts_with('-')
}

pub fn is_valid_workspace_relative_path(value: &str) -> bool {
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

pub fn is_valid_read_only_exec_arguments(arguments: &[String]) -> bool {
    !arguments.is_empty()
        && arguments.len() <= MAX_READ_ONLY_EXEC_ARGUMENTS
        && arguments.iter().all(|argument| {
            !argument.is_empty()
                && argument.len() <= MAX_READ_ONLY_EXEC_ARGUMENT_BYTES
                && !argument.chars().any(char::is_control)
        })
        && arguments.iter().map(String::len).sum::<usize>()
            <= MAX_READ_ONLY_EXEC_ARGUMENT_TOTAL_BYTES
}

#[cfg(test)]
mod tests {
    use super::{read_only_exec_fd_mounts, workspace_cwd_path, WORKSPACE_CWD_MOUNT_FD};

    #[test]
    fn cwd_mount_preserves_workspace_relative_location_and_skips_root_remount() {
        assert_eq!(workspace_cwd_path("."), Some("/workspace".to_owned()));
        assert_eq!(
            workspace_cwd_path("crates/member"),
            Some("/workspace/crates/member".to_owned())
        );
        let root_mounts = read_only_exec_fd_mounts(".").unwrap();
        assert_eq!(root_mounts.len(), 4);
        assert!(!root_mounts
            .iter()
            .any(|(fd, _)| *fd == WORKSPACE_CWD_MOUNT_FD));
        let nested_mounts = read_only_exec_fd_mounts("crates/member").unwrap();
        assert_eq!(nested_mounts.len(), 5);
        assert!(nested_mounts
            .iter()
            .any(|(fd, path)| *fd == WORKSPACE_CWD_MOUNT_FD && path == "/workspace/crates/member"));
        assert_eq!(workspace_cwd_path("../outside"), None);
        assert_eq!(read_only_exec_fd_mounts("a//b"), None);
    }
}
