//! Applies the fixed optional filesystem-write policy before the fixed Cargo payload.

#[cfg(target_os = "linux")]
mod linux {
    use krypper_sandbox::runner_contract::{
        is_valid_read_only_exec_arguments, is_valid_test_filter, is_valid_workspace_relative_path,
        workspace_cwd_path, FIXED_CARGO_ARGUMENTS, READ_ONLY_EXEC_PROFILE_ID, RUNNER_MOUNT_PATH,
        WORKSPACE_CWD_MOUNT_FD, WORKSPACE_MOUNT,
    };
    use krypper_sandbox::runner_protocol::{
        encode_status, RunnerStatus, ACK_FD, ACK_FRAME, STATUS_FD,
    };
    use krypper_sandbox::{apply_landlock_write_policy, LandlockSetupStatus};
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    const CARGO_EXECUTABLE: &str = "/toolchain/bin/cargo";
    const RUSTC_EXECUTABLE: &str = "/toolchain/bin/rustc";

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Invocation {
        requirement: Requirement,
        profile_id: &'static str,
        executable: &'static str,
        working_directory: String,
        payload_arguments: Vec<String>,
    }

    pub(super) fn run() -> i32 {
        let Ok(arguments) = std::env::args_os()
            .map(|argument| argument.into_string())
            .collect::<Result<Vec<_>, _>>()
        else {
            return 126;
        };
        let Some(invocation) = parse_invocation(&arguments) else {
            return 126;
        };
        if !valid_execution_context(&invocation) {
            return report_failure(libc::EPERM);
        }
        if let Err(error) = close_if_open(WORKSPACE_CWD_MOUNT_FD) {
            return report_failure(error.raw_os_error().unwrap_or(libc::EIO));
        }

        let status = match apply_landlock_write_policy() {
            Ok(LandlockSetupStatus::Applied { abi }) => RunnerStatus::Applied { abi },
            Ok(LandlockSetupStatus::Unavailable { abi }) => RunnerStatus::Unavailable { abi },
            Err(error) => RunnerStatus::Failed {
                errno: error.raw_os_error().unwrap_or(libc::EIO),
            },
        };
        if matches!(status, RunnerStatus::Unavailable { .. })
            && invocation.requirement == Requirement::Required
        {
            return report_status(status);
        }

        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let Some(mut acknowledgement) = take_fd(ACK_FD) else {
            let _ = write_status(
                &mut status_pipe,
                RunnerStatus::Failed { errno: libc::EBADF },
            );
            return 125;
        };
        if matches!(status, RunnerStatus::Failed { .. }) {
            let _ = write_status(&mut status_pipe, status);
            return 125;
        }
        if write_status(&mut status_pipe, status).is_err() {
            return 125;
        }
        let mut ack = [0u8; ACK_FRAME.len()];
        if acknowledgement.read_exact(&mut ack).is_err() || &ack != ACK_FRAME {
            return 125;
        }
        drop(status_pipe);
        drop(acknowledgement);

        let mut payload = Command::new(invocation.executable);
        payload.args(&invocation.payload_arguments);
        let error = payload.exec();
        eprintln!("offline process exec failed: {error}");
        126
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Requirement {
        Optional,
        Required,
    }

    fn parse_invocation(arguments: &[String]) -> Option<Invocation> {
        if arguments.len() < 13
            || arguments[0] != RUNNER_MOUNT_PATH
            || arguments[1] != "--krypper-landlock-runner-v1"
            || arguments[2] != "--status-fd"
            || arguments[3] != STATUS_FD.to_string()
            || arguments[4] != "--ack-fd"
            || arguments[5] != ACK_FD.to_string()
        {
            return None;
        }
        let requirement = match arguments[6].as_str() {
            "--optional" => Requirement::Optional,
            "--required" => Requirement::Required,
            _ => return None,
        };
        if arguments
            .get(7)
            .is_some_and(|argument| argument == "--profile")
        {
            if requirement != Requirement::Required
                || arguments.len() < 14
                || arguments[8] != READ_ONLY_EXEC_PROFILE_ID
                || arguments[9] != "--cwd"
                || !is_valid_workspace_relative_path(&arguments[10])
                || arguments[11] != "--"
            {
                return None;
            }
            let executable = match arguments[12].as_str() {
                CARGO_EXECUTABLE => CARGO_EXECUTABLE,
                RUSTC_EXECUTABLE => RUSTC_EXECUTABLE,
                _ => return None,
            };
            let payload_arguments = arguments[13..].to_vec();
            if !is_valid_read_only_exec_arguments(&payload_arguments) {
                return None;
            }
            return Some(Invocation {
                requirement,
                profile_id: READ_ONLY_EXEC_PROFILE_ID,
                executable,
                working_directory: arguments[10].clone(),
                payload_arguments,
            });
        }
        if arguments[7] != "--" || arguments[8] != CARGO_EXECUTABLE {
            return None;
        }
        let cargo_arguments = &arguments[9..];
        if cargo_arguments.len() < FIXED_CARGO_ARGUMENTS.len()
            || !cargo_arguments
                .iter()
                .take(FIXED_CARGO_ARGUMENTS.len())
                .map(String::as_str)
                .eq(FIXED_CARGO_ARGUMENTS.iter().copied())
        {
            return None;
        }
        match &cargo_arguments[FIXED_CARGO_ARGUMENTS.len()..] {
            [] => Some(Invocation {
                requirement,
                profile_id: "offline_cargo_test",
                executable: CARGO_EXECUTABLE,
                working_directory: ".".to_owned(),
                payload_arguments: cargo_arguments.to_vec(),
            }),
            [separator, filter] if separator.as_str() == "--" && is_valid_test_filter(filter) => {
                Some(Invocation {
                    requirement,
                    profile_id: "offline_cargo_test",
                    executable: CARGO_EXECUTABLE,
                    working_directory: ".".to_owned(),
                    payload_arguments: cargo_arguments.to_vec(),
                })
            }
            _ => None,
        }
    }

    fn valid_execution_context(invocation: &Invocation) -> bool {
        let expected_cwd = if invocation.profile_id == READ_ONLY_EXEC_PROFILE_ID {
            workspace_cwd_path(&invocation.working_directory).unwrap_or_default()
        } else if invocation.working_directory == "." {
            WORKSPACE_MOUNT.1.to_owned()
        } else {
            format!("{}/{}/", WORKSPACE_MOUNT.1, invocation.working_directory)
        };
        if std::env::current_exe().ok().as_deref() != Some(Path::new(RUNNER_MOUNT_PATH))
            || std::env::current_dir().ok().as_deref()
                != Some(Path::new(expected_cwd.trim_end_matches('/')))
        {
            return false;
        }
        let actual = std::env::vars_os()
            .map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .collect::<Option<BTreeMap<_, _>>>();
        let Some(actual) = actual else {
            return false;
        };
        let expected_pwd = expected_cwd.trim_end_matches('/');
        let expected = krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_owned(),
                    if *name == "PWD" {
                        expected_pwd.to_owned()
                    } else {
                        (*value).to_owned()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        actual == expected
    }

    fn take_fd(fd: i32) -> Option<File> {
        // Check ownership before converting the descriptor into an owning File.
        // SAFETY: `fd` is one of the two fixed inherited protocol descriptors; F_GETFD reads
        // its descriptor flags and does not dereference any user pointer.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return None;
        }
        // SAFETY: the preceding F_GETFD confirmed `fd` is open; F_SETFD updates only its flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return None;
        }
        // SAFETY: this process inherited the fixed descriptor from the sandbox supervisor and
        // `fcntl(F_GETFD)` confirmed it is open. The returned File owns it from this point onward.
        Some(unsafe { File::from_raw_fd(fd) })
    }

    fn close_if_open(fd: i32) -> std::io::Result<()> {
        // SAFETY: F_GETFD only inspects the fixed host-owned descriptor slot.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::EBADF) {
                Ok(())
            } else {
                Err(error)
            };
        }
        // SAFETY: F_GETFD established that this fixed descriptor slot is open.
        if unsafe { libc::close(fd) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn report_failure(errno: i32) -> i32 {
        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let _ = write_status(&mut status_pipe, RunnerStatus::Failed { errno });
        125
    }

    fn report_status(status: RunnerStatus) -> i32 {
        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let _ = write_status(&mut status_pipe, status);
        125
    }

    fn write_status(pipe: &mut File, status: RunnerStatus) -> std::io::Result<()> {
        pipe.write_all(&encode_status(status))
    }

    #[cfg(test)]
    mod tests {
        use super::{parse_invocation, Requirement, ACK_FD, CARGO_EXECUTABLE, STATUS_FD};
        use krypper_sandbox::runner_contract::RUNNER_MOUNT_PATH;

        fn invocation(requirement: &str, cargo_tail: &[&str]) -> Vec<String> {
            let mut args = vec![
                RUNNER_MOUNT_PATH.to_owned(),
                "--krypper-landlock-runner-v1".to_owned(),
                "--status-fd".to_owned(),
                STATUS_FD.to_string(),
                "--ack-fd".to_owned(),
                ACK_FD.to_string(),
                requirement.to_owned(),
                "--".to_owned(),
                CARGO_EXECUTABLE.to_owned(),
                "test".to_owned(),
                "--offline".to_owned(),
                "--locked".to_owned(),
                "--all-targets".to_owned(),
            ];
            args.extend(cargo_tail.iter().map(|argument| (*argument).to_owned()));
            args
        }

        #[test]
        fn accepts_fixed_optional_and_required_invocations() {
            assert_eq!(
                parse_invocation(&invocation("--optional", &[])).map(|value| value.requirement),
                Some(Requirement::Optional)
            );
            assert_eq!(
                parse_invocation(&invocation("--required", &["--", "fixture::case"]))
                    .map(|value| value.requirement),
                Some(Requirement::Required)
            );
        }

        #[test]
        fn accepts_only_the_pinned_direct_exec_profile_and_executables() {
            let mut args = vec![
                RUNNER_MOUNT_PATH.to_owned(),
                "--krypper-landlock-runner-v1".to_owned(),
                "--status-fd".to_owned(),
                STATUS_FD.to_string(),
                "--ack-fd".to_owned(),
                ACK_FD.to_string(),
                "--required".to_owned(),
                "--profile".to_owned(),
                "offline_read_only_exec_v1".to_owned(),
                "--cwd".to_owned(),
                "crates/example".to_owned(),
                "--".to_owned(),
                "/toolchain/bin/rustc".to_owned(),
                "--version".to_owned(),
            ];
            let parsed = parse_invocation(&args).unwrap();
            assert_eq!(parsed.profile_id, "offline_read_only_exec_v1");
            assert_eq!(parsed.executable, "/toolchain/bin/rustc");
            assert_eq!(parsed.working_directory, "crates/example");
            assert_eq!(parsed.payload_arguments, ["--version"]);

            args[6] = "--optional".to_owned();
            assert!(parse_invocation(&args).is_none());
            args[6] = "--required".to_owned();
            args[12] = "/tmp/rustc".to_owned();
            assert!(parse_invocation(&args).is_none());
            args[12] = "/toolchain/bin/rustc".to_owned();
            args[10] = "../outside".to_owned();
            assert!(parse_invocation(&args).is_none());
        }

        #[test]
        fn rejects_changes_to_fixed_runner_and_cargo_arguments() {
            let mut args = invocation("--optional", &[]);
            args[3] = "203".to_owned();
            assert_eq!(parse_invocation(&args), None);

            let mut args = invocation("--optional", &[]);
            args[9] = "build".to_owned();
            assert_eq!(parse_invocation(&args), None);

            let mut args = invocation("--optional", &[]);
            args[8] = "/tmp/cargo".to_owned();
            assert_eq!(parse_invocation(&args), None);
        }

        #[test]
        fn rejects_invalid_filters_and_extra_arguments() {
            assert_eq!(
                parse_invocation(&invocation("--optional", &["--", "-bad-filter"])),
                None
            );
            assert_eq!(
                parse_invocation(&invocation("--optional", &["--", "valid", "unexpected"])),
                None
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::run());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    std::process::exit(126);
}
