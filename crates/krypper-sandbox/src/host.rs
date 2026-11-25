//! Fixed host environment for Linux sandbox helper processes.

use std::process::{Command, Stdio};

pub(super) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub(super) fn host_command(executable: &str) -> Command {
    // All helper commands use a fixed executable, bounded output and a hard wait.
    // No caller-provided command, bus address, environment, or shell is accepted.
    let mut command = Command::new(executable);
    let uid = effective_uid();
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{uid}/bus"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}
