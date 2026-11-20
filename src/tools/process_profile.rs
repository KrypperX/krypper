//! Versioned, immutable contracts for host-owned process profiles.
//!
//! A profile descriptor is descriptive metadata only. It does not register a
//! tool, grant an authority, select a sandbox backend or launch a process.
//! Runtime authorization stays with TaskSession and the existing Tools boundary.

/// The only profile contract format currently understood by Krypper.
pub const PROFILE_CONTRACT_VERSION: u32 = 1;

const MAX_ID_BYTES: usize = 64;
const MAX_EXECUTABLE_BYTES: usize = 4096;
const MAX_ARG_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 4096;
const MAX_WALL_SECS: u64 = 300;
const MAX_CPU_MS: u64 = 240_000;
const MAX_MEMORY_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PROCESSES: u64 = 128;

/// Hard ceilings for one profile run. Values must be finite and non-zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileLimits {
    pub wall_secs: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub output_bytes: usize,
    pub processes: u64,
}

/// A host-owned description of one bounded process profile.
///
/// Constructing this value never makes a command runnable. Every field is
/// validated before a future caller may turn the descriptor into a launch plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessProfile<'a> {
    pub version: u32,
    pub id: &'a str,
    /// Trusted, absolute executable identity supplied by the host, not a model.
    pub executable: &'a str,
    /// Closed argument grammar; each entry is a single value, never a shell string.
    pub arguments: &'a [&'a str],
    /// Workspace-relative working directory metadata.
    pub cwd: &'a str,
    /// Non-secret environment variable names that a future run may consider.
    pub environment_allowlist: &'a [&'a str],
    pub limits: ProfileLimits,
    /// Reserved metadata. Any entry is rejected until a reviewed contract extends this type.
    pub extra_fields: &'a [(&'a str, &'a str)],
}

/// Rejection reasons for malformed or unsafe profile metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileContractError {
    UnsupportedVersion,
    InvalidIdentifier,
    InvalidExecutable,
    InvalidArgument,
    InvalidCwd,
    InvalidEnvironmentAllowlist,
    InvalidLimits,
    UnknownField,
}

/// Validate a profile before any future dispatch or launch planning.
///
/// Validation is fail closed: unrepresentable or unknown metadata is rejected,
/// never defaulted or silently allowed. It does not claim sandbox confinement.
pub fn validate(profile: &ProcessProfile<'_>) -> Result<(), ProfileContractError> {
    if profile.version != PROFILE_CONTRACT_VERSION {
        return Err(ProfileContractError::UnsupportedVersion);
    }
    if !valid_identifier(profile.id) {
        return Err(ProfileContractError::InvalidIdentifier);
    }
    if !valid_executable(profile.executable) {
        return Err(ProfileContractError::InvalidExecutable);
    }
    if profile
        .arguments
        .iter()
        .any(|argument| !valid_argument(argument))
    {
        return Err(ProfileContractError::InvalidArgument);
    }
    if !valid_cwd(profile.cwd) {
        return Err(ProfileContractError::InvalidCwd);
    }
    if profile
        .environment_allowlist
        .iter()
        .any(|name| !valid_environment_name(name))
    {
        return Err(ProfileContractError::InvalidEnvironmentAllowlist);
    }
    if !valid_limits(&profile.limits) {
        return Err(ProfileContractError::InvalidLimits);
    }
    if !profile.extra_fields.is_empty() {
        return Err(ProfileContractError::UnknownField);
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_executable(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_EXECUTABLE_BYTES || !value.starts_with('/') {
        return false;
    }
    if value.chars().any(char::is_control) || value.split('/').any(|part| part == "..") {
        return false;
    }
    let name = value.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    !matches!(
        name.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "csh"
            | "tcsh"
            | "cmd"
            | "cmd.exe"
            | "powershell"
            | "pwsh"
            | "python"
            | "python3"
            | "node"
            | "ruby"
            | "perl"
    )
}

fn valid_argument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ARG_BYTES
        && !value.chars().any(char::is_control)
        && !value.starts_with('-')
        && !value.bytes().any(|byte| {
            matches!(
                byte,
                b';' | b'|' | b'&' | b'$' | b'`' | b'*' | b'?' | b'<' | b'>' | b'\n'
            )
        })
}

fn valid_cwd(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CWD_BYTES
        && !value.starts_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "..")
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return false;
    }
    let upper = value.to_ascii_uppercase();
    !["TOKEN", "KEY", "SECRET", "PASSWORD", "CREDENTIAL", "COOKIE"]
        .iter()
        .any(|marker| upper.contains(marker))
}

fn valid_limits(limits: &ProfileLimits) -> bool {
    limits.wall_secs > 0
        && limits.wall_secs <= MAX_WALL_SECS
        && limits.cpu_ms > 0
        && limits.cpu_ms <= MAX_CPU_MS
        && limits.memory_bytes > 0
        && limits.memory_bytes <= MAX_MEMORY_BYTES
        && limits.output_bytes > 0
        && limits.output_bytes <= MAX_OUTPUT_BYTES
        && limits.processes > 0
        && limits.processes <= MAX_PROCESSES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ProfileLimits {
        ProfileLimits {
            wall_secs: 60,
            cpu_ms: 30_000,
            memory_bytes: 512 * 1024 * 1024,
            output_bytes: 64 * 1024,
            processes: 16,
        }
    }

    fn profile<'a>() -> ProcessProfile<'a> {
        ProcessProfile {
            version: PROFILE_CONTRACT_VERSION,
            id: "offline-check",
            executable: "/toolchain/bin/cargo",
            arguments: &["test", "offline"],
            cwd: ".",
            environment_allowlist: &["LANG"],
            limits: limits(),
            extra_fields: &[],
        }
    }

    #[test]
    fn accepts_a_well_formed_profile() {
        assert_eq!(validate(&profile()), Ok(()));
    }

    #[test]
    fn rejects_unknown_contract_versions_and_extra_fields() {
        let mut item = profile();
        item.version += 1;
        assert_eq!(
            validate(&item),
            Err(ProfileContractError::UnsupportedVersion)
        );

        let mut item = profile();
        item.extra_fields = &[("sandbox_flags", "--no-sandbox")];
        assert_eq!(validate(&item), Err(ProfileContractError::UnknownField));
    }

    #[test]
    fn rejects_unsafe_executables_and_arguments() {
        for executable in ["cargo", "../toolchain/bin/cargo", "/bin/sh", "/bin/bash"] {
            let mut item = profile();
            item.executable = executable;
            assert_eq!(
                validate(&item),
                Err(ProfileContractError::InvalidExecutable)
            );
        }
        for argument in ["--force", "a;rm -rf /", "a|b", ""] {
            let arguments = [argument];
            let mut item = profile();
            item.arguments = &arguments;
            assert_eq!(validate(&item), Err(ProfileContractError::InvalidArgument));
        }
    }

    #[test]
    fn rejects_escaping_cwd_secret_names_and_raised_limits() {
        for cwd in ["/etc", "../outside", "a/../b"] {
            let mut item = profile();
            item.cwd = cwd;
            assert_eq!(validate(&item), Err(ProfileContractError::InvalidCwd));
        }
        for name in ["API_TOKEN", "SECRET_KEY", "1BAD", "BAD NAME"] {
            let allowlist = [name];
            let mut item = profile();
            item.environment_allowlist = &allowlist;
            assert_eq!(
                validate(&item),
                Err(ProfileContractError::InvalidEnvironmentAllowlist)
            );
        }
        let mut over = limits();
        over.output_bytes = MAX_OUTPUT_BYTES + 1;
        let mut item = profile();
        item.limits = over;
        assert_eq!(validate(&item), Err(ProfileContractError::InvalidLimits));

        let mut zero = limits();
        zero.wall_secs = 0;
        let mut item = profile();
        item.limits = zero;
        assert_eq!(validate(&item), Err(ProfileContractError::InvalidLimits));
    }

    #[test]
    fn validation_does_not_modify_registered_tools() {
        let before = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        validate(&profile()).unwrap();
        let after = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }
}
