//! Real syscall-filter evidence through Cargo and a descendant process.

use super::{gate, policy, workspace};
use crate::tools::capture_workspace_root_at;
use std::fs;
use std::io;
use std::os::raw::c_long;
use std::process::Command;

const LIBC_LOCK: &str = "version = 4\n\n[[package]]\nname = \"real_sandbox_fixture\"\nversion = \"0.1.0\"\ndependencies = [\n \"libc\",\n]\n\n[[package]]\nname = \"libc\"\nversion = \"0.2.191\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"65cd87c9a6d60536580b316ad985726104730a5151c99845f2490428ef91240a\"\n";

fn assert_not_host_denied(name: &str, result: c_long) {
    assert!(
        !(result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)),
        "the host already returned EPERM for {name}; this host cannot verify the sandbox filter"
    );
}

fn host_syscall_baseline() {
    assert!(
        unsafe { libc::syscall(libc::SYS_getpid) } > 0,
        "host getpid probe failed"
    );

    let unix_socket = unsafe {
        libc::syscall(
            libc::SYS_socket,
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert!(unix_socket >= 0, "host socket(AF_UNIX) probe failed");
    assert_eq!(unsafe { libc::close(unix_socket as libc::c_int) }, 0);

    let mut unix_pair = [-1; 2];
    let unix_socketpair = unsafe {
        libc::syscall(
            libc::SYS_socketpair,
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            unix_pair.as_mut_ptr(),
        )
    };
    assert_eq!(unix_socketpair, 0, "host socketpair(AF_UNIX) probe failed");
    assert_eq!(unsafe { libc::close(unix_pair[0]) }, 0);
    assert_eq!(unsafe { libc::close(unix_pair[1]) }, 0);

    let ptrace = unsafe { libc::syscall(libc::SYS_ptrace, -1 as c_long, 0, 0, 0) };
    assert_not_host_denied("ptrace", ptrace);
    let process_vm_readv = unsafe {
        libc::syscall(
            libc::SYS_process_vm_readv,
            0 as libc::pid_t,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    assert_not_host_denied("process_vm_readv", process_vm_readv);
    let process_vm_writev = unsafe {
        libc::syscall(
            libc::SYS_process_vm_writev,
            0 as libc::pid_t,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    assert_not_host_denied("process_vm_writev", process_vm_writev);
    let request_key = unsafe {
        libc::syscall(
            libc::SYS_request_key,
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_char>(),
            0 as libc::c_int,
        )
    };
    assert_not_host_denied("request_key", request_key);
    let io_uring_setup = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            0 as libc::c_uint,
            std::ptr::null::<libc::c_void>(),
        )
    };
    assert_not_host_denied("io_uring_setup", io_uring_setup);
    let io_uring_register = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            -1 as libc::c_int,
            0 as libc::c_uint,
            std::ptr::null::<libc::c_void>(),
            0 as libc::c_uint,
        )
    };
    assert_not_host_denied("io_uring_register", io_uring_register);

    let socket = unsafe {
        libc::syscall(
            libc::SYS_socket,
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert_not_host_denied("socket(AF_VSOCK)", socket);
    if socket >= 0 {
        assert_eq!(unsafe { libc::close(socket as libc::c_int) }, 0);
    }

    let mut pair = [-1; 2];
    let socketpair = unsafe {
        libc::syscall(
            libc::SYS_socketpair,
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    };
    assert_not_host_denied("socketpair(AF_VSOCK)", socketpair);
    if socketpair == 0 {
        assert_eq!(unsafe { libc::close(pair[0]) }, 0);
        assert_eq!(unsafe { libc::close(pair[1]) }, 0);
    }

    let io_uring = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            -1 as libc::c_int,
            0 as libc::c_uint,
            0 as libc::c_uint,
            0 as libc::c_uint,
            std::ptr::null::<libc::sigset_t>(),
            0 as libc::size_t,
        )
    };
    assert_not_host_denied("io_uring_enter", io_uring);

    #[cfg(target_arch = "x86_64")]
    {
        let x32 = unsafe { libc::syscall(libc::SYS_getpid | 0x4000_0000) };
        assert_not_host_denied("x32-tagged getpid", x32);
        let reserved = unsafe { libc::syscall(547 as libc::c_long) };
        assert_not_host_denied("reserved native syscall 547", reserved);
    }
}

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_sandbox_syscall_filter_covers_cargo_payload_and_descendant() {
    let Some(host) = gate("real_sandbox_syscall_filter_covers_cargo_payload_and_descendant") else {
        return;
    };
    host_syscall_baseline();

    let path = workspace();
    fs::write(
        path.join("Cargo.toml"),
        "[package]\nname=\"real_sandbox_fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\nlibc=\"=0.2.191\"\n",
    )
    .unwrap();
    fs::write(path.join("Cargo.lock"), LIBC_LOCK).unwrap();
    fs::write(
        path.join("src/lib.rs"),
        r#"
use std::io;
use std::process::Command;
use std::os::raw::c_long;

fn expect_errno(name: &str, result: c_long, expected: i32) {
    assert_eq!(result, -1, "{name} unexpectedly succeeded with {result}");
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(expected), "{name}");
}

fn assert_policy() {
    assert!(unsafe { libc::syscall(libc::SYS_getpid) } > 0, "getpid must remain allowed");

    let unix_socket = unsafe {
        libc::syscall(
            libc::SYS_socket,
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert!(unix_socket >= 0, "AF_UNIX socket must remain allowed: {unix_socket}");
    assert_eq!(unsafe { libc::close(unix_socket as libc::c_int) }, 0);

    let mut pair = [-1; 2];
    assert_eq!(
        unsafe {
            libc::syscall(
                libc::SYS_socketpair,
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        },
        0,
        "AF_UNIX socketpair must remain allowed"
    );
    assert_eq!(unsafe { libc::close(pair[0]) }, 0);
    assert_eq!(unsafe { libc::close(pair[1]) }, 0);

    let ptrace = unsafe { libc::syscall(libc::SYS_ptrace, -1 as c_long, 0, 0, 0) };
    expect_errno("ptrace", ptrace, libc::EPERM);
    let process_vm_readv = unsafe {
        libc::syscall(
            libc::SYS_process_vm_readv,
            0 as libc::pid_t,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    expect_errno("process_vm_readv", process_vm_readv, libc::EPERM);
    let process_vm_writev = unsafe {
        libc::syscall(
            libc::SYS_process_vm_writev,
            0 as libc::pid_t,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            std::ptr::null::<libc::iovec>(),
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    expect_errno("process_vm_writev", process_vm_writev, libc::EPERM);
    let request_key = unsafe {
        libc::syscall(
            libc::SYS_request_key,
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_char>(),
            0 as libc::c_int,
        )
    };
    expect_errno("request_key", request_key, libc::EPERM);
    let io_uring_setup = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            0 as libc::c_uint,
            std::ptr::null::<libc::c_void>(),
        )
    };
    expect_errno("io_uring_setup", io_uring_setup, libc::EPERM);
    let io_uring_register = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            -1 as libc::c_int,
            0 as libc::c_uint,
            std::ptr::null::<libc::c_void>(),
            0 as libc::c_uint,
        )
    };
    expect_errno("io_uring_register", io_uring_register, libc::EPERM);

    let vsock = unsafe {
        libc::syscall(
            libc::SYS_socket,
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
        )
    };
    expect_errno("socket(AF_VSOCK)", vsock, libc::EPERM);

    let mut vsock_pair = [-1; 2];
    let vsock_pair_result = unsafe {
        libc::syscall(
            libc::SYS_socketpair,
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            vsock_pair.as_mut_ptr(),
        )
    };
    expect_errno("socketpair(AF_VSOCK)", vsock_pair_result, libc::EPERM);

    let io_uring = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            -1 as libc::c_int,
            0 as libc::c_uint,
            0 as libc::c_uint,
            0 as libc::c_uint,
            std::ptr::null::<libc::sigset_t>(),
            0 as libc::size_t,
        )
    };
    expect_errno("io_uring_enter", io_uring, libc::EPERM);

    #[cfg(target_arch = "x86_64")]
    {
        let x32 = unsafe { libc::syscall(libc::SYS_getpid | 0x4000_0000) };
        expect_errno("x32-tagged getpid", x32, libc::EPERM);
        let reserved = unsafe { libc::syscall(547 as libc::c_long) };
        expect_errno("reserved native syscall 547", reserved, libc::EPERM);
    }
}

#[test]
fn syscall_descendant_probe() {
    if std::env::var_os("KRYPPER_SYSCALL_DESCENDANT").is_some() {
        assert_policy();
    }
}

#[test]
fn fixed_policy_and_descendant() {
    assert_policy();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "syscall_descendant_probe"])
        .env("KRYPPER_SYSCALL_DESCENDANT", "1")
        .status()
        .unwrap();
    assert!(status.success(), "seccomp policy did not reach the descendant: {status}");
}
"#,
    )
    .unwrap();

    let root = capture_workspace_root_at(&path).unwrap();
    let result =
        crate::tools::process::run(&root, &policy(&host), Some("fixed_policy_and_descendant"));
    assert!(
        result.succeeded(),
        "syscall-filter Cargo fixture failed: {result:?}"
    );
    assert_eq!(result.executed_tests, Some(1));
    assert!(result.cleanup_confirmed, "{result:?}");
    assert!(!result.recovery_hold, "{result:?}");
}
