//! Applies the fixed optional filesystem-write policy before the fixed Cargo payload.

#[cfg(target_os = "linux")]
mod linux {
    use krypper_sandbox::{apply_landlock_write_policy, LandlockSetupStatus};
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    const STATUS_FD: i32 = 204;
    const ACK_FD: i32 = 205;
    const PROTOCOL_VERSION: u16 = 1;
    const STATUS_FRAME_LEN: usize = 20;
    const STATUS_MAGIC: &[u8; 8] = b"KRYLLND1";
    const ACK_FRAME: &[u8; 8] = b"KRYACK1\n";
    const CARGO_EXECUTABLE: &str = "/toolchain/bin/cargo";
    const CARGO_FIXED_ARGUMENTS: &[&str] = &["test", "--offline", "--locked", "--all-targets"];
    const EXPECTED_CWD: &str = "/workspace";

    fn run() -> i32 {
        let Ok(arguments) = std::env::args_os()
            .map(|argument| argument.into_string())
            .collect::<Result<Vec<_>, _>>()
        else {
            return 126;
        };
        let Some(requirement) = parse_invocation(&arguments) else {
            return 126;
        };
        if !valid_execution_context() {
            return report_failure(libc::EPERM);
        }

        let status = match apply_landlock_write_policy() {
            Ok(LandlockSetupStatus::Applied { abi }) => (1, abi, 0),
            Ok(LandlockSetupStatus::Unavailable { abi }) => (2, abi.unwrap_or(0), 0),
            Err(error) => (3, 0, error.raw_os_error().unwrap_or(libc::EIO)),
        };
        if status.0 == 2 && requirement == Requirement::Required {
            return report_status(status);
        }

        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let Some(mut acknowledgement) = take_fd(ACK_FD) else {
            let _ = write_status(&mut status_pipe, (3, 0, libc::EBADF));
            return 125;
        };
        if status.0 == 3 {
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

        let mut cargo = Command::new(CARGO_EXECUTABLE);
        cargo.args(&arguments[9..]);
        let error = cargo.exec();
        eprintln!("fixed offline Cargo exec failed: {error}");
        126
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Requirement {
        Optional,
        Required,
    }

    fn parse_invocation(arguments: &[String]) -> Option<Requirement> {
        if arguments.len() < 13
            || arguments[0] != "/run/krypper-sandbox-runner"
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
        if arguments[7] != "--" || arguments[8] != CARGO_EXECUTABLE {
            return None;
        }
        let cargo_arguments = &arguments[9..];
        if cargo_arguments.len() < CARGO_FIXED_ARGUMENTS.len()
            || !cargo_arguments
                .iter()
                .take(CARGO_FIXED_ARGUMENTS.len())
                .map(String::as_str)
                .eq(CARGO_FIXED_ARGUMENTS.iter().copied())
        {
            return None;
        }
        match cargo_arguments.len() - CARGO_FIXED_ARGUMENTS.len() {
            0 => Some(requirement),
            2 if cargo_arguments[4] == "--" && valid_filter(&cargo_arguments[5]) => {
                Some(requirement)
            }
            _ => None,
        }
    }

    fn valid_filter(filter: &str) -> bool {
        !filter.is_empty()
            && filter.len() <= 256
            && !filter.chars().any(char::is_control)
            && !filter.trim_start().starts_with('-')
    }

    fn valid_execution_context() -> bool {
        if std::env::current_exe().ok().as_deref() != Some(Path::new("/run/krypper-sandbox-runner"))
            || std::env::current_dir().ok().as_deref() != Some(Path::new(EXPECTED_CWD))
        {
            return false;
        }
        let actual = std::env::vars_os()
            .map(|(name, value)| (name.into_string().ok()?, value.into_string().ok()?))
            .collect::<Option<BTreeMap<_, _>>>();
        let Some(actual) = actual else {
            return false;
        };
        let expected = krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        actual == expected
    }

    fn take_fd(fd: i32) -> Option<File> {
        // Check ownership before converting the descriptor into an owning File.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return None;
        }
        // SAFETY: this process inherited the fixed descriptor from the sandbox supervisor and
        // `fcntl(F_GETFD)` confirmed it is open. The returned File owns it from this point onward.
        Some(unsafe { File::from_raw_fd(fd) })
    }

    fn report_failure(errno: i32) -> i32 {
        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let _ = write_status(&mut status_pipe, (3, 0, errno));
        125
    }

    fn report_status(status: (u8, i32, i32)) -> i32 {
        let Some(mut status_pipe) = take_fd(STATUS_FD) else {
            return 126;
        };
        let _ = write_status(&mut status_pipe, status);
        125
    }

    fn write_status(pipe: &mut File, (kind, abi, errno): (u8, i32, i32)) -> std::io::Result<()> {
        let mut frame = [0u8; STATUS_FRAME_LEN];
        frame[..8].copy_from_slice(STATUS_MAGIC);
        frame[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        frame[10] = kind;
        frame[12..16].copy_from_slice(&(abi as u32).to_be_bytes());
        frame[16..20].copy_from_slice(&errno.to_be_bytes());
        pipe.write_all(&frame)
    }

    #[cfg(test)]
    mod tests {
        use super::{parse_invocation, Requirement, ACK_FD, CARGO_EXECUTABLE, STATUS_FD};

        fn invocation(requirement: &str, cargo_tail: &[&str]) -> Vec<String> {
            let mut args = vec![
                "/run/krypper-sandbox-runner".to_owned(),
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
                parse_invocation(&invocation("--optional", &[])),
                Some(Requirement::Optional)
            );
            assert_eq!(
                parse_invocation(&invocation("--required", &["--", "fixture::case"])),
                Some(Requirement::Required)
            );
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
