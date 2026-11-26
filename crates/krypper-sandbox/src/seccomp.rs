//! The immutable syscall profile for the offline Cargo sandbox.

use std::io;

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
use seccompiler::{
    sock_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
fn compile_program() -> io::Result<BpfProgram> {
    use std::collections::BTreeMap;

    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(syscall, Vec::new());
    }

    // The syscall ABI treats the domain as an int. Comparing only its low 32 bits also
    // prevents a caller from hiding AF_VSOCK in an argument's upper half.
    let vsock_condition = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_VSOCK as u64,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let vsock_rule = SeccompRule::new(vec![vsock_condition])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    for syscall in [libc::SYS_socket, libc::SYS_socketpair] {
        rules.insert(syscall, vec![vsock_rule.clone()]);
    }

    #[cfg(target_arch = "x86_64")]
    let target_arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let target_arch = TargetArch::aarch64;
    #[cfg(target_arch = "riscv64")]
    let target_arch = TargetArch::riscv64;

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let mut program = BpfProgram::try_from(filter)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    #[cfg(target_arch = "x86_64")]
    prepend_x86_64_abi_guard(&mut program)?;

    Ok(program)
}

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
fn serialize_program(program: &[sock_filter]) -> io::Result<Vec<u8>> {
    const MAX_BPF_INSTRUCTIONS: usize = 4096;
    const BPF_INSTRUCTION_BYTES: usize = 8;

    if program.is_empty() || program.len() > MAX_BPF_INSTRUCTIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixed syscall filter has an invalid instruction count",
        ));
    }
    let mut bytes = Vec::with_capacity(program.len() * BPF_INSTRUCTION_BYTES);
    for instruction in program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.push(instruction.jt);
        bytes.push(instruction.jf);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    Ok(bytes)
}

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
pub(crate) fn compile_filter() -> io::Result<Vec<u8>> {
    serialize_program(&compile_program()?)
}

#[cfg(not(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
pub(crate) fn compile_filter() -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "offline Cargo syscall filtering is unsupported on this Linux architecture or ABI",
    ))
}

#[cfg(all(target_arch = "x86_64", target_endian = "little"))]
fn prepend_x86_64_abi_guard(program: &mut BpfProgram) -> io::Result<()> {
    const ARCH_GUARD_INSTRUCTIONS: usize = 3;
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_JMP_JGE_K: u16 = 0x35;
    const BPF_JMP_JGT_K: u16 = 0x25;
    const BPF_RET_K: u16 = 0x06;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    const X86_64_RESERVED_SYSCALL_START: u32 = 512;
    const X86_64_RESERVED_SYSCALL_END: u32 = 547;
    const X86_64_AUDIT_ARCH: u32 = 0xc000_003e;
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;

    // The compiler emits an architecture guard as its first three instructions. Refuse to
    // alter an unexpected layout so a dependency change cannot silently weaken ABI checks.
    if program.len() < ARCH_GUARD_INSTRUCTIONS
        || program[0].code != BPF_LD_W_ABS
        || program[0].k != 4
        || program[1].code != BPF_JMP_JEQ_K
        || program[1].jt != 1
        || program[1].jf != 0
        || program[1].k != X86_64_AUDIT_ARCH
        || program[2].code != BPF_RET_K
        || program[2].k != libc::SECCOMP_RET_KILL_PROCESS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixed syscall filter architecture guard layout changed",
        ));
    }

    let _ = program.splice(
        ARCH_GUARD_INSTRUCTIONS..ARCH_GUARD_INSTRUCTIONS,
        [
            sock_filter {
                code: BPF_LD_W_ABS,
                jt: 0,
                jf: 0,
                k: 0,
            },
            sock_filter {
                code: BPF_JMP_JSET_K,
                jt: 0,
                jf: 1,
                k: X32_SYSCALL_BIT,
            },
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: deny,
            },
            sock_filter {
                code: BPF_JMP_JGE_K,
                jt: 0,
                jf: 2,
                k: X86_64_RESERVED_SYSCALL_START,
            },
            sock_filter {
                code: BPF_JMP_JGT_K,
                jt: 1,
                jf: 0,
                k: X86_64_RESERVED_SYSCALL_END,
            },
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: deny,
            },
        ],
    );
    Ok(())
}

#[cfg(all(
    test,
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
mod tests {
    use super::{compile_program, serialize_program};
    use seccompiler::sock_filter;

    const SECCOMP_DATA_ARCH_OFFSET: usize = 4;
    const SECCOMP_DATA_NR_OFFSET: usize = 0;
    const SECCOMP_DATA_ARGS_OFFSET: usize = 16;

    fn evaluate(program: &[sock_filter], number: u32, arch: u32, args: [u64; 6]) -> u32 {
        let mut accumulator = 0u32;
        let mut pc = 0usize;
        let mut steps = 0usize;
        loop {
            steps += 1;
            assert!(steps <= program.len() * 4, "BPF program did not terminate");
            let instruction = program.get(pc).expect("BPF jump must remain in program");
            let next = pc + 1;
            match instruction.code {
                0x20 => {
                    let offset = instruction.k as usize;
                    accumulator = if offset == SECCOMP_DATA_NR_OFFSET {
                        number
                    } else if offset == SECCOMP_DATA_ARCH_OFFSET {
                        arch
                    } else if offset >= SECCOMP_DATA_ARGS_OFFSET {
                        let argument = (offset - SECCOMP_DATA_ARGS_OFFSET) / 8;
                        let upper_word = (offset - SECCOMP_DATA_ARGS_OFFSET) % 8 == 4;
                        let value = *args.get(argument).expect("valid seccomp argument offset");
                        if upper_word {
                            (value >> 32) as u32
                        } else {
                            value as u32
                        }
                    } else {
                        panic!("unexpected seccomp data offset {offset}");
                    };
                    pc = next;
                }
                0x05 => pc = next + instruction.k as usize,
                0x15 => {
                    pc = next
                        + if accumulator == instruction.k {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                }
                0x25 => {
                    pc = next
                        + if accumulator > instruction.k {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                }
                0x35 => {
                    pc = next
                        + if accumulator >= instruction.k {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                }
                0x45 => {
                    pc = next
                        + if accumulator & instruction.k != 0 {
                            instruction.jt as usize
                        } else {
                            instruction.jf as usize
                        };
                }
                0x54 => {
                    accumulator &= instruction.k;
                    pc = next;
                }
                0x06 => return instruction.k,
                opcode => panic!("unexpected BPF opcode {opcode:#x}"),
            }
        }
    }

    fn action(program: &[sock_filter], number: u32, args: [u64; 6]) -> u32 {
        let native_arch = program[1].k;
        evaluate(program, number, native_arch, args)
    }

    fn denied() -> u32 {
        libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
    }

    #[test]
    fn fixed_policy_denies_sensitive_syscalls_and_preserves_unix_sockets() {
        let program = compile_program().unwrap();
        for syscall in [
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_request_key,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            assert_eq!(
                action(&program, syscall as u32, [0; 6]),
                denied(),
                "syscall {syscall} must return EPERM"
            );
        }
        for syscall in [libc::SYS_socket, libc::SYS_socketpair] {
            assert_eq!(
                action(
                    &program,
                    syscall as u32,
                    [libc::AF_VSOCK as u64, 0, 0, 0, 0, 0]
                ),
                denied()
            );
            assert_eq!(
                action(
                    &program,
                    syscall as u32,
                    [libc::AF_UNIX as u64, 0, 0, 0, 0, 0]
                ),
                libc::SECCOMP_RET_ALLOW
            );
            assert_eq!(
                action(
                    &program,
                    syscall as u32,
                    [libc::AF_VSOCK as u64 | (1u64 << 32), 0, 0, 0, 0, 0],
                ),
                denied(),
                "AF_VSOCK in the low word must not be hidden by argument upper bits"
            );
        }
    }

    #[test]
    fn architecture_and_abi_guards_fail_closed() {
        let program = compile_program().unwrap();
        let wrong_arch = program[1].k ^ 1;
        assert_eq!(
            evaluate(&program, libc::SYS_getpid as u32, wrong_arch, [0; 6]),
            libc::SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            action(&program, libc::SYS_getpid as u32, [0; 6]),
            libc::SECCOMP_RET_ALLOW
        );
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(
                action(&program, libc::SYS_getpid as u32 | 0x4000_0000, [0; 6]),
                denied(),
                "tagged x32 ABI calls must return EPERM"
            );
            assert_eq!(action(&program, 511, [0; 6]), libc::SECCOMP_RET_ALLOW);
            assert_eq!(action(&program, 512, [0; 6]), denied());
            assert_eq!(action(&program, 547, [0; 6]), denied());
            assert_eq!(action(&program, 548, [0; 6]), libc::SECCOMP_RET_ALLOW);
        }

        let bytes = serialize_program(&program).unwrap();
        assert_eq!(bytes.len(), program.len() * 8);
    }
}

#[cfg(all(
    test,
    not(all(
        target_endian = "little",
        any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )
    ))
))]
mod unsupported_tests {
    #[test]
    fn unsupported_architectures_fail_before_filter_use() {
        assert_eq!(
            super::compile_filter().unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}
