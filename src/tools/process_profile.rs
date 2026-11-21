//! Versioned, immutable contracts for host-owned process profiles.
//!
//! A profile descriptor is descriptive metadata only. It does not register a
//! tool, grant an authority, select a sandbox backend or launch a process.
//! The fixed offline Cargo policy validates the `offline_cargo_test` descriptor
//! and uses its executable and argument prefix. Runtime authorization stays with
//! TaskSession and the existing Tools boundary. Construction validates the
//! contract, so an unvalidated descriptor cannot exist.

/// The only profile contract format currently understood by Krypper.
pub const PROFILE_CONTRACT_VERSION: u32 = 1;

const MAX_ID_BYTES: usize = 64;
const MAX_EXECUTABLE_BYTES: usize = 4096;
const MAX_ARG_BYTES: usize = 256;
const MAX_ARGUMENTS: usize = 16;
const MAX_CWD_BYTES: usize = 4096;
const MAX_WALL_SECS: u64 = 300;
const MAX_CPU_MS: u64 = 240_000;
const MAX_MEMORY_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PROCESSES: u64 = 128;
const ALLOWED_EXECUTABLES: &[&str] = &["/toolchain/bin/cargo", "/toolchain/bin/rustc"];
const ALLOWED_ENVIRONMENT_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "CARGO_HOME",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "RUSTC",
    "LANG",
];
const OFFLINE_CARGO_TEST_ID: &str = "offline_cargo_test";
const OFFLINE_CARGO_TEST_EXECUTABLE: &str = "/toolchain/bin/cargo";
const OFFLINE_CARGO_TEST_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
const OFFLINE_CARGO_TEST_CWD: &str = ".";
const OFFLINE_CARGO_TEST_LIMITS: ProfileLimits = ProfileLimits {
    wall_secs: 180,
    cpu_ms: 120_000,
    memory_bytes: 3 * 1024 * 1024 * 1024,
    output_bytes: 256 * 1024,
    processes: 128,
};

/// Hard ceilings for one profile run. Values must be finite and non-zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileLimits {
    pub wall_secs: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub output_bytes: usize,
    pub processes: u64,
}

/// A validated host-owned description of one bounded process profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessProfile<'a> {
    version: u32,
    id: &'a str,
    executable: &'a str,
    arguments: &'a [&'a str],
    cwd: &'a str,
    environment_allowlist: &'a [&'a str],
    limits: ProfileLimits,
    extra_fields: &'a [(&'a str, &'a str)],
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

impl ProcessProfile<'static> {
    /// The single approved offline Cargo test profile used by the host policy.
    pub fn offline_cargo_test() -> Result<Self, ProfileContractError> {
        Self::new(
            PROFILE_CONTRACT_VERSION,
            OFFLINE_CARGO_TEST_ID,
            OFFLINE_CARGO_TEST_EXECUTABLE,
            OFFLINE_CARGO_TEST_ARGUMENTS,
            OFFLINE_CARGO_TEST_CWD,
            ALLOWED_ENVIRONMENT_NAMES,
            OFFLINE_CARGO_TEST_LIMITS,
            &[],
        )
    }
}

impl<'a> ProcessProfile<'a> {
    /// Validate and construct a descriptor from typed host-owned fields.
    ///
    /// Validation is fail closed: unrepresentable or unknown metadata is rejected,
    /// never defaulted or silently allowed. It does not claim sandbox confinement.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        version: u32,
        id: &'a str,
        executable: &'a str,
        arguments: &'a [&'a str],
        cwd: &'a str,
        environment_allowlist: &'a [&'a str],
        limits: ProfileLimits,
        extra_fields: &'a [(&'a str, &'a str)],
    ) -> Result<Self, ProfileContractError> {
        let profile = Self {
            version,
            id,
            executable,
            arguments,
            cwd,
            environment_allowlist,
            limits,
            extra_fields,
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), ProfileContractError> {
        if self.version != PROFILE_CONTRACT_VERSION {
            return Err(ProfileContractError::UnsupportedVersion);
        }
        if !valid_identifier(self.id) {
            return Err(ProfileContractError::InvalidIdentifier);
        }
        if self.id == OFFLINE_CARGO_TEST_ID {
            if self.executable != OFFLINE_CARGO_TEST_EXECUTABLE {
                return Err(ProfileContractError::InvalidExecutable);
            }
            if self.arguments != OFFLINE_CARGO_TEST_ARGUMENTS {
                return Err(ProfileContractError::InvalidArgument);
            }
            if self.cwd != OFFLINE_CARGO_TEST_CWD {
                return Err(ProfileContractError::InvalidCwd);
            }
            if self.environment_allowlist != ALLOWED_ENVIRONMENT_NAMES {
                return Err(ProfileContractError::InvalidEnvironmentAllowlist);
            }
            if self.limits != OFFLINE_CARGO_TEST_LIMITS {
                return Err(ProfileContractError::InvalidLimits);
            }
        } else {
            if self.executable.len() > MAX_EXECUTABLE_BYTES
                || !ALLOWED_EXECUTABLES.contains(&self.executable)
            {
                return Err(ProfileContractError::InvalidExecutable);
            }
            if self.arguments.len() > MAX_ARGUMENTS
                || self
                    .arguments
                    .iter()
                    .any(|argument| !valid_argument(argument))
            {
                return Err(ProfileContractError::InvalidArgument);
            }
            if !valid_cwd(self.cwd) {
                return Err(ProfileContractError::InvalidCwd);
            }
            if self
                .environment_allowlist
                .iter()
                .any(|name| !ALLOWED_ENVIRONMENT_NAMES.contains(name))
            {
                return Err(ProfileContractError::InvalidEnvironmentAllowlist);
            }
            if !valid_limits(&self.limits) {
                return Err(ProfileContractError::InvalidLimits);
            }
        }
        if !self.extra_fields.is_empty() {
            return Err(ProfileContractError::UnknownField);
        }
        Ok(())
    }

    /// The contract version this descriptor was validated against.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The stable host-owned profile identifier.
    pub fn id(&self) -> &'a str {
        self.id
    }

    /// The exact trusted executable identity.
    pub fn executable(&self) -> &'a str {
        self.executable
    }

    /// The closed single-value argument sequence.
    pub fn arguments(&self) -> &'a [&'a str] {
        self.arguments
    }

    /// The workspace-relative working directory.
    pub fn cwd(&self) -> &'a str {
        self.cwd
    }

    /// The reviewed environment variable names a future run may consider.
    pub fn environment_allowlist(&self) -> &'a [&'a str] {
        self.environment_allowlist
    }

    /// The hard resource ceilings for one run.
    pub fn limits(&self) -> ProfileLimits {
        self.limits
    }

    /// Reserved metadata; always empty on a validated descriptor.
    pub fn extra_fields(&self) -> &'a [(&'a str, &'a str)] {
        self.extra_fields
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_argument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ARG_BYTES
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'_' | b'-' | b'/' | b':' | b'=' | b',' | b'@' | b'+'
                )
        })
}

fn valid_cwd(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CWD_BYTES
        && !value.starts_with('/')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
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
        ProcessProfile::new(
            PROFILE_CONTRACT_VERSION,
            "offline-check",
            "/toolchain/bin/cargo",
            &["test", "offline"],
            ".",
            &["LANG"],
            limits(),
            &[],
        )
        .unwrap()
    }

    #[test]
    fn accepts_a_well_formed_profile() {
        let item = profile();
        assert_eq!(item.version(), PROFILE_CONTRACT_VERSION);
        assert_eq!(item.id(), "offline-check");
        assert_eq!(item.executable(), "/toolchain/bin/cargo");
        assert_eq!(item.arguments(), &["test", "offline"]);
        assert_eq!(item.cwd(), ".");
        assert_eq!(item.environment_allowlist(), &["LANG"]);
        assert_eq!(item.limits(), limits());
        assert!(item.extra_fields().is_empty());
    }

    #[test]
    fn rejects_unknown_contract_versions_and_extra_fields() {
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION + 1,
                "offline-check",
                "/toolchain/bin/cargo",
                &["test"],
                ".",
                &["LANG"],
                limits(),
                &[],
            ),
            Err(ProfileContractError::UnsupportedVersion)
        );

        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline-check",
                "/toolchain/bin/cargo",
                &["test"],
                ".",
                &["LANG"],
                limits(),
                &[("sandbox_flags", "--no-sandbox")],
            ),
            Err(ProfileContractError::UnknownField)
        );
    }

    #[test]
    fn rejects_arbitrary_and_wrapper_executables() {
        for executable in [
            "cargo",
            "../toolchain/bin/cargo",
            "/bin/sh",
            "/bin/bash",
            "/usr/bin/python3.11",
            "/usr/bin/env",
            "/toolchain/bin/busybox",
            "/toolchain/bin/cargo;touch /tmp/x",
            "/toolchain/bin/cargo/",
        ] {
            assert_eq!(
                ProcessProfile::new(
                    PROFILE_CONTRACT_VERSION,
                    "offline-check",
                    executable,
                    &["test"],
                    ".",
                    &["LANG"],
                    limits(),
                    &[],
                ),
                Err(ProfileContractError::InvalidExecutable)
            );
        }
    }

    #[test]
    fn rejects_unsafe_arguments_and_excessive_argument_counts() {
        for argument in [
            "--force",
            "a;rm -rf /",
            "a|b",
            "",
            "foo(bar)",
            "a\\ b",
            "a'b",
            "a\"b",
            "a#b",
            "a~b",
        ] {
            assert_eq!(
                ProcessProfile::new(
                    PROFILE_CONTRACT_VERSION,
                    "offline-check",
                    "/toolchain/bin/cargo",
                    &[argument],
                    ".",
                    &["LANG"],
                    limits(),
                    &[],
                ),
                Err(ProfileContractError::InvalidArgument)
            );
        }

        let arguments = ["a"; MAX_ARGUMENTS + 1];
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline-check",
                "/toolchain/bin/cargo",
                &arguments,
                ".",
                &["LANG"],
                limits(),
                &[],
            ),
            Err(ProfileContractError::InvalidArgument)
        );
    }

    #[test]
    fn rejects_escaping_cwd_and_unreviewed_environment_names() {
        for cwd in ["/etc", "../outside", "a/../b", "a b", "a;rm"] {
            assert_eq!(
                ProcessProfile::new(
                    PROFILE_CONTRACT_VERSION,
                    "offline-check",
                    "/toolchain/bin/cargo",
                    &["test"],
                    cwd,
                    &["LANG"],
                    limits(),
                    &[],
                ),
                Err(ProfileContractError::InvalidCwd)
            );
        }
        for name in [
            "API_TOKEN",
            "SECRET_KEY",
            "MYSQL_PWD",
            "PASS",
            "CRED",
            "AUTH",
            "BEARER",
            "MY_CUSTOM_VAR",
        ] {
            assert_eq!(
                ProcessProfile::new(
                    PROFILE_CONTRACT_VERSION,
                    "offline-check",
                    "/toolchain/bin/cargo",
                    &["test"],
                    ".",
                    &[name],
                    limits(),
                    &[],
                ),
                Err(ProfileContractError::InvalidEnvironmentAllowlist)
            );
        }
    }

    #[test]
    fn rejects_raised_and_zero_limits() {
        let mut over = limits();
        over.output_bytes = MAX_OUTPUT_BYTES + 1;
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline-check",
                "/toolchain/bin/cargo",
                &["test"],
                ".",
                &["LANG"],
                over,
                &[],
            ),
            Err(ProfileContractError::InvalidLimits)
        );

        let mut zero = limits();
        zero.wall_secs = 0;
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline-check",
                "/toolchain/bin/cargo",
                &["test"],
                ".",
                &["LANG"],
                zero,
                &[],
            ),
            Err(ProfileContractError::InvalidLimits)
        );
    }

    #[test]
    fn construction_does_not_modify_registered_tools() {
        let before = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        let item = profile();
        let after = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        assert_eq!(item.id(), "offline-check");
        assert_eq!(before, after);
    }

    #[test]
    fn fixed_offline_cargo_profile_has_the_live_command_shape() {
        let item = ProcessProfile::offline_cargo_test().unwrap();
        assert_eq!(item.version(), PROFILE_CONTRACT_VERSION);
        assert_eq!(item.id(), "offline_cargo_test");
        assert_eq!(item.executable(), "/toolchain/bin/cargo");
        assert_eq!(
            item.arguments(),
            &["test", "--offline", "--locked", "--all-targets"]
        );
        assert_eq!(item.cwd(), ".");
        assert_eq!(
            item.environment_allowlist(),
            &[
                "PATH",
                "HOME",
                "TMPDIR",
                "CARGO_HOME",
                "CARGO_BUILD_JOBS",
                "CARGO_TARGET_DIR",
                "RUSTC",
                "LANG"
            ]
        );
        assert_eq!(
            item.limits(),
            ProfileLimits {
                wall_secs: 180,
                cpu_ms: 120_000,
                memory_bytes: 3 * 1024 * 1024 * 1024,
                output_bytes: 256 * 1024,
                processes: 128,
            }
        );
        assert!(item.extra_fields().is_empty());
    }

    #[test]
    fn fixed_cargo_arguments_are_rejected_for_other_profile_ids() {
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline-check",
                "/toolchain/bin/cargo",
                &["test", "--offline"],
                ".",
                &["LANG"],
                limits(),
                &[],
            ),
            Err(ProfileContractError::InvalidArgument)
        );
        assert_eq!(
            ProcessProfile::new(
                PROFILE_CONTRACT_VERSION,
                "offline_cargo_test",
                "/toolchain/bin/cargo",
                &["test"],
                ".",
                &["LANG"],
                limits(),
                &[],
            ),
            Err(ProfileContractError::InvalidArgument)
        );
    }
}
