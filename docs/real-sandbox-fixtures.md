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

`KRYPPER_TEST_TOOLCHAIN` and `KRYPPER_TEST_REGISTRY` must also point at preprovisioned
directories; `OfflineCargoPolicy::new` validates that `bin/cargo` and `bin/rustc` are real ELF
executables. These variables configure the test harness only. They are never product request
fields and are never forwarded into the sandbox.

## Running them

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
| `real_sandbox_host_reports_supported_prerequisites` | The host really provides all five primitives (printed per run). |
| `real_sandbox_isolation_blocks_host_paths_and_namespaces` | A workspace symlink to an outside canary is unreadable and unwritable; `/home` and `/run/user` are absent; cwd is `/workspace`; `NSpid` has a single entry; host-side create-new canaries confirm all three mount sources are writable before launch; `/proc/self/fd` is inspected and writes through mount source descriptors 200–202 are denied. |
| `real_sandbox_environment_and_network_carry_no_host_secrets` | Parent-provided `GITHUB_TOKEN` and a per-run secret canary are absent inside; a live host loopback listener is unreachable inside and still reachable outside. |
| `real_sandbox_cleanup_releases_state_for_the_next_run` | Cleanup is confirmed, no recovery hold is raised, and an immediately following run is not degraded. |
| `real_offline_cargo_behavior_is_preserved_through_the_harness` | The registered profile still runs through `WorkspacePlan` and Harness with a literal filter: the selected test executes, the unrelated test is filtered. |
| `real_sandbox_limits_and_cancellation_stay_bounded` | Zero matches, nonzero exit, wall timeout, output ceiling, CPU/memory/process ceilings and cancellation all return their typed outcome with confirmed cleanup. |

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
`pre_exec` cgroup attachment, ambient descriptor hygiene beyond the explicit 200–202 write probes,
delegated-subtree checks, crash-time lease recovery, or cross-process cleanup holds. Those remain
source-review claims until a gated run on a provisioned host supplies evidence. macOS and Windows
behaviour is unknown.

This fixture does not evaluate syscall filtering. The fixed Bubblewrap command installs no seccomp
profile, so passing its mount-source probes provides no seccomp evidence.
