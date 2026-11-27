# Gated real-sandbox host fixtures (#219 PR 11)

> Ordinary CI does **not** claim host-level confinement. These fixtures are local host evidence
> only, and they are opt-in twice over. This document states exactly what they verify and what
> remains unproven by required GitHub Verify.

## Why they are separated

Required CI (`cargo test --all-targets --locked`) runs on a generic runner with no
`/usr/bin/bwrap`, no delegated cgroup v2 subtree and no user systemd manager. A green CI run
therefore proves the request/authorization/dispatch contracts and typed failures, never that a
child process was actually namespace-isolated. Mixing both kinds of evidence in one suite is what
lets a green check be misread as confinement.

PR 11 puts the real-sandbox evidence in `src/tools/process/host_fixtures/`, one fixture per
boundary, behind a second gate on top of `#[ignore]`.

## The two gates

1. `#[ignore]` — ordinary `cargo test` never runs them.
2. `KRYPPER_REAL_SANDBOX=1` — the fixture returns early and prints one `SKIP` line unless the
   operator sets exactly `1`. Presence of the variable is not enough.

Setting the gate is a claim about the host, so the gate then probes every prerequisite and
**fails** when one is missing rather than skipping:

| Prerequisite | Probe |
| --- | --- |
| Linux | `cfg!(target_os = "linux")` |
| Not root | `geteuid() != 0` |
| Bubblewrap | `/usr/bin/bwrap` exists |
| Unified cgroup v2 | `/sys/fs/cgroup/cgroup.controllers` is readable |
| User systemd manager | `/run/user/<uid>/systemd/private` exists |

The existence check does not validate Bubblewrap's version or option support. The fixed command
requires Bubblewrap 0.10.0 or a compatible build that accepts all options it uses, including
`--ro-bind-fd`. The product readiness probe runs the exact command before starting Cargo and fails
setup if the installed build rejects an option; ordinary CI does not establish host compatibility.

`KRYPPER_TEST_TOOLCHAIN` and `KRYPPER_TEST_REGISTRY` must also point at preprovisioned
directories; `OfflineCargoPolicy::new` validates that `bin/cargo` and `bin/rustc` are real ELF
executables. These variables configure the test harness only. They are never product request
fields and are never forwarded into the sandbox.

## Running them

Build both Product binaries first so the fixed sibling runner is available beside the main
application binary:

```bash
cargo build --bins --locked
```

```bash
KRYPPER_REAL_SANDBOX=1 \
KRYPPER_TEST_TOOLCHAIN=/opt/rust-toolchain \
KRYPPER_TEST_REGISTRY=/opt/cargo-registry \
cargo test --locked --lib real_ -- --ignored --nocapture --test-threads=1
```

`--test-threads=1` is required: the supervisor holds one exclusive host lease, so concurrent
invocations would contend for the same host resource.

## What each fixture proves

| Fixture | Boundary |
| --- | --- |
| `real_sandbox_host_reports_supported_prerequisites` | The host really provides all five required host primitives (printed per run); Landlock support is optional and is reported per run. |
| `real_sandbox_isolation_blocks_host_paths_and_namespaces` | A workspace symlink to an outside canary is unreadable and unwritable; `/home` and `/run/user` are absent; cwd is `/workspace`; `NSpid` has a single entry; host-side create-new canaries confirm all three mount sources are writable before launch; `/proc/self/fd` is inspected and writes through mount source descriptors 200–202 are denied. |
| `real_sandbox_environment_and_network_carry_no_host_secrets` | Parent-provided `GITHUB_TOKEN` and a per-run secret canary are absent inside; a live host loopback listener is unreachable inside and still reachable outside. |
| `real_sandbox_cleanup_releases_state_for_the_next_run` | Cleanup is confirmed, no recovery hold is raised, and an immediately following run is not degraded. |
| `real_sandbox_captured_workspace_path_swap_uses_pinned_root_or_fails_closed` | Replaces the captured workspace pathname with a symlink to a distinct canary root before launch; execution uses the captured directory or fails before tests, and the outside canary remains unchanged. |
| `real_sandbox_caller_crash_retains_and_recovers_execution_lease` | Kills the Product test caller after its selected payload starts; observes that the selected payload exits while the transient systemd service remains active and its cgroup remains populated; verifies the durable receipt and service/cgroup identity; a new call reports unknown with cleanup unconfirmed and a recovery hold without stopping that service; then a fresh call succeeds after the fixture-owned service is stopped and its cgroup is empty. |
| `real_offline_cargo_behavior_is_preserved_through_the_harness` | The registered profile still runs through `WorkspacePlan` and Harness with a literal filter: the selected test executes, the unrelated test is filtered. |
| `real_sandbox_syscall_filter_covers_cargo_payload_and_descendant` | After checking the host does not already return `EPERM` for the probes, Cargo and an exec'd test descendant can call `getpid` and use `AF_UNIX`; `io_uring_enter` and `AF_VSOCK` socket calls return `EPERM`; x86_64 also checks tagged x32 and reserved syscall handling. Cargo succeeds and the supervisor confirms cleanup. |
| `real_sandbox_landlock_limits_writes_and_reaches_descendants` | Requires a supported Landlock ABI. Confirms the writable Bubblewrap `/tmp` tmpfs is Landlock-restricted, profile scratch directories remain writable, an unlisted `/tmp` path is denied, descendants inherit the denial, subprocesses can redirect output to the private `/dev/null`, and result evidence reports `applied`. |
| `real_sandbox_limits_and_cancellation_stay_bounded` | Zero matches, nonzero exit, wall timeout, output ceiling, CPU/memory/process ceilings and cancellation all return their typed outcome with confirmed cleanup. |
| `real_offline_read_only_exec_preserves_nested_cwd_and_confinement` | Exercises the exact permission prompt, one-shot grant and Harness dispatch. Direct Cargo requests launch with cwd `crates/member` and `.` (workspace root), covering both the nested-cwd FD 207 mount and the root route; the payload verifies workspace visibility, workspace-relative parents, FD hygiene, no-new-privileges, empty effective capabilities, PID namespace state, hidden host paths, offline network, read-only workspace, and seccomp denial in an exec descendant. |
| `real_offline_read_only_exec_limits_cancellation_and_cleanup` | Runs a permitted direct Cargo argv that exceeds the output ceiling, then authorizes one exact request and cancels its held policy during execution; checks the typed outcomes, confirmed cleanup and no recovery hold. |

The C3 fixtures require the same host prerequisites and pinned toolchain/registry above, with the
locked `libc` 0.2.191 crate available in the offline registry. The confinement fixture also needs
a reachable host loopback canary and a host ptrace baseline that does not already return `EPERM`;
otherwise it cannot attribute the denial to the sandbox policy. Run only these fixtures with:

```bash
KRYPPER_REAL_SANDBOX=1 \
KRYPPER_TEST_TOOLCHAIN=/opt/rust-toolchain \
KRYPPER_TEST_REGISTRY=/opt/cargo-registry \
cargo test --locked --lib real_offline_read_only_exec -- --ignored --nocapture --test-threads=1
```

These results describe the one provisioned host, kernel and Bubblewrap build where they ran.
Ordinary CI checks the typed request, authorization, generated command and failure handling; it
does not run payload-controlled code or prove host namespace, seccomp, descriptor or filesystem
enforcement. No C4 umbrella completion is claimed by these C3 fixtures.

The caller-crash fixture requires an isolated run with no pre-existing cleanup receipt and no
concurrent Krypper sandbox execution. The retained cleanup state is the durable receipt plus the
active transient systemd unit and cgroup; that state lets a later caller detect uncertain cleanup
after the original caller dies. The fixture validates the receipt's UUID, systemd unit and cgroup
against systemd before stopping only that fixture-owned unit. It leaves the receipt for Product's
normal recovery path to reconcile.

Nothing in this module grants authority, adds a profile, changes the CLI, or weakens the fixed
policy. A failing fixture is a host or product regression, never something to skip.

## Legacy fixtures

`src/tools/process/tests.rs` still contains the earlier monolithic
`real_offline_profile_is_confined_and_cleans_up` and `real_product_workspace_runs_the_fixed_profile`
fixtures. They remain `#[ignore]`d and unchanged as historical local evidence; they do not read
`KRYPPER_REAL_SANDBOX`. Prefer the fixtures above for new host evidence, and follow up by removing
the legacy pair once the gated suite has been exercised on a provisioned host.

## What is still unproven

Ordinary CI cannot establish: Bubblewrap flag semantics, in-payload namespace behaviour,
`pre_exec` cgroup attachment, seccomp descriptor propagation and kernel enforcement, Landlock
write enforcement, ambient descriptor hygiene beyond the explicit mount-source probes, or
delegated-subtree checks. The caller-crash fixture covers payload termination and receipt-based
recovery holds for its tested caller/service path on the host where it ran; other crash points
and host configurations remain unproven. The
Landlock fixture specifically requires a supported ABI; its absence does not make the optional
control report `applied`. macOS and Windows behaviour is unknown; Linux architectures outside the
fixed profile fail before workspace code can launch.

The fixed syscall filter is defense in depth alongside the private namespaces, read-only mounts,
offline network namespace and cgroup limits. It denies a small immutable set of sensitive calls
and the vsock domain; it does not replace filesystem confinement or make arbitrary syscalls safe.
Ordinary CI evaluates the generated BPF policy and Bubblewrap command construction, but cannot establish
that the host Bubblewrap version accepts the descriptor or that the kernel enforces the filter.
