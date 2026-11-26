# Opt-in Linux offline Cargo profile

## Registered operation effects

The current workspace registry exposes nine filesystem Tools and one process
Tool, `offline_cargo_test`. The five read-only filesystem Tools are
`read_file`, `read_range`, `list_directory`, `search_text`, and
`search_tree_text`; each has the `read-only workspace access` effect. The four
filesystem Tools that mutate the workspace are `create_directory`,
`create_file`, `replace_text`, and `apply_patch`; each has the
`workspace mutation` effect. The process Tool has the
`fixed sandboxed offline Cargo test` effect.

These provider-facing descriptions are informational. Every workspace
operation still requires its existing task authorization, while
`offline_cargo_test` additionally requires its existing host authorization and
verified Linux confinement. Runtime authorization checks remain authoritative
and fail closed; descriptive metadata and model output do not grant access.

## Fixed profile

Issue #178 implements `offline_cargo_test`. Its request may omit `test_filter`
or include one literal test-name filter of at most 256 UTF-8 bytes. Empty values,
control characters, option-like values and over-limit values are rejected
before process setup. The filter uses Rust's test harness literal substring
matching; it is passed unchanged as one argument after Cargo's fixed `--`
separator and cannot add Cargo or test-harness flags.
Without a filter it runs exactly `cargo test --offline --locked --all-targets`.
Requests cannot supply an executable, other arguments, cwd, environment,
sandbox options, or higher limits. This is an embedding API capability; no CLI
syntax or default CLI permission has been added.

The host profile carries a version 2 validated requirements contract. It records
the fixed Cargo executable and argv prefix, workspace-relative cwd, read-only
`Workspace`, `Toolchain`, and `CargoRegistry` resource roles, environment names,
profile ceilings, deny-all network policy, the fixed `OfflineCargoTestV1`
syscall-profile identifier, and an ephemeral process tree. The environment
contract contains names only; the sandbox supplies its fixed values. Logical
resource roles are mapped to the corresponding host-captured directory
descriptors. No host path or environment value is accepted in the request.
The optional test filter remains a separate, validated literal argument after
Cargo's fixed `--` separator.

Legacy version 1 `ProcessProfile` values remain metadata-only. Only the fixed
version 2 profile has execution requirements and can be passed by the Tools
adapter to `SandboxRunRequest`. The common runner boundary validates the exact
contract, captured resource roles, and effective limits, then compares the
profile's required controls with the selected backend's declared capabilities.
If any required control is missing, or the platform has no backend, the request
returns `SandboxUnavailable` before backend setup or payload launch. A declared
capability means the backend can attempt to establish that control; it does not
prove that the current host provides it or that setup succeeded.

The Linux backend still performs its host prerequisite checks, exact Bubblewrap
readiness probe, and cgroup readback before launching Cargo. A setup failure
returns a typed non-success result and never falls back to a less restricted
launch. `SandboxIsolationEvidence` remains separate from backend capabilities:
it records per-run setup status, while behavioral enforcement still requires
fixtures on a provisioned host.

The Product adapter requests optional Landlock filesystem-write confinement for
this fixed Linux profile. The pinned in-sandbox runner applies the policy after
Bubblewrap creates its filesystem view and before Cargo starts. Writes are
allowed only under `/tmp/home`, `/tmp/target`, and `/tmp/tmp`. `CARGO_HOME` is
the `/tmp/home` scratch directory; its `registry` entry points to the separate
read-only `/cargo/registry` mount, which is outside Landlock's write allowlist.
The fixed `TMPDIR` is `/tmp/tmp`. If the kernel does not provide a supported Landlock ABI, the
result reports `unavailable` and Cargo may continue only after the existing
required controls pass. A setup error after ABI support is detected fails the
run before Cargo starts. `applied` means the runner completed setup; only the
gated host fixture can report behavioral evidence.

`ProfileLimits` are hard profile ceilings: wall time is stored in seconds and
CPU time in milliseconds. Each request carries separate effective `RunLimits`
in milliseconds and microseconds. Conversion uses checked multiplication, and
zero, overflow, runner-bound excess, or any effective value above its profile
ceiling is denied before dispatch. The public profile always uses its fixed
limits; only lower effective limits are allowed for private test fixtures.

These requirements describe requested policy. They are not sandbox result
evidence and do not show that a backend applied or behaviorally enforced a
control. `SandboxIsolationEvidence` remains attached to the result as a
separate record of setup status. A `requested` contract field cannot be read as
`applied` or `observed`; host enforcement claims still require behavioral
fixtures on a provisioned host.

The bounded process observation records the selected filter, whether any tests
ran, and the count of Rust test-harness per-test result lines. Aggregate summary
lines do not establish that tests ran. A filtered run is successful only when
at least one test ran, Cargo exited zero, output was complete, and cleanup was
confirmed. A zero-match filter has the typed `no_tests_matched` outcome even
when Cargo itself exits zero. Incomplete output does not provide successful
execution evidence.

Each process result also carries bounded audit metadata in the typed
observation: observed elapsed milliseconds, retained and enforced output byte
counts, kernel limit events in a fixed precedence order, and a recovery-hold
flag. It contains no paths, host identities, raw output, or authorization data.
Outcome selection is deterministic: unconfirmed cleanup wins, then kernel limit
events, then the observed stop condition (output cap, cancellation, wall or CPU
budget, exit, or signal). A recovery hold is a non-success state and blocks the
next run until host cleanup is established.

## Authorization and dispatch

```text
Brain / caller: OfflineCargoTest
  Harness: existing action budget and ordered observations
    TaskSession: owning task + running turn + explicit task grant
      Tools: host lease -> verified supervisor -> verified sandbox -> fixed profile
```

Trusted embedding code creates `OfflineCargoPolicy::new(toolchain, registry)`
and grants it to one existing task with
`TaskSession::authorize_offline_cargo_tests(task_id, policy)`. Policy
construction validates the fixed `ProcessProfile::offline_cargo_test()`
descriptor and uses its executable, fixed argument prefix and limits; cwd,
environment values, sandbox flags and enforcement remain owned by the policy and
supervisor. The toolchain
must contain real ELF `bin/cargo` and `bin/rustc`, their libraries, and any
native compiler/linker needed by the workload (in `bin/` or the trusted system
runtime). These are preprovisioned, trusted directories outside task
workspaces. The registry contains only offline dependency data, not Cargo
credentials or host configuration. The host policy owner is responsible for
their provenance, integrity and immutability, just as for `/usr` and the kernel;
an ELF header alone is not an authenticity check. No installer or downloader
runs through this API.

The public Tools and `ProcessResult` imports remain in the Product crate. Its
adapter passes the profile's validated requirements, captured directory
handles, effective limits, test filter and cancellation state to the private
`krypper-sandbox` workspace member, which owns Bubblewrap setup, cgroup
supervision and cleanup. The crate rejects any request that differs from the
fixed profile; it does not accept arbitrary commands, environment values,
mount paths or authorization grants. This contract does not change the
TaskSession/Tools launch authority or establish behavioral enforcement by
itself.

Process grants are not checkpointed. A restored task requires a fresh host
grant. `policy.cancellation()` returns a one-way cancellation handle for the
trusted caller; a cancelled grant must be replaced explicitly to run again.
The regular Harness failure path records failed or unknown attempts and stops
that turn; it does not retry a process automatically. Activity/checkpoint
records retain only success and output byte-count metadata, never process
output, runtime paths, or the authorization grant.

The legacy `authorize_offline_cargo_tests` method remains an unbounded,
standing grant for the lifetime of its TaskSession. Trusted embedding code may
instead use `authorize_offline_cargo_tests_scoped` for one exact optional
filter and a lifetime from one instant through 24 hours, or
`authorize_offline_cargo_test_once` for one exact optional filter with the same
expiry bound. In those APIs, `None` approves only a run with no filter. Empty,
option-like, control-character, overlong, zero-lifetime, and over-24-hour
requests are rejected. Replacing or revoking a grant advances its task-local
epoch and signals cancellation to the previous policy; epoch exhaustion fails
closed. A one-shot grant is never widened to the standing session grant.

Every authorization is bound to a versioned SHA-256 encoding of the fixed
operation and profile, ordered executable arguments (including a selected
filter in its actual Cargo argument position), cwd, policy and effective
limits, environment names and values, network/syscall/process-tree controls,
task/session/grant identities and epoch, grant mode/scope/expiry, captured
workspace/toolchain/registry directory identities, and the TaskSession
workspace mutation revision. The final Product-owned spawn gate rechecks the
active grant and current intent immediately before spawn. It holds the grant
state lock through the spawn call, so a concurrent revoke or replacement
linearizes before or after that start. A one-shot is consumed just before the
spawn call; setup failures and cancellation before the gate do not consume it,
while a failed spawn does. Cancellation after a process has started is
best-effort.

This digest is an internal consistency binding, not an authenticated approval
token: it is neither signed nor MACed and grants no authority by itself.
Authority comes from the trusted host grant APIs and their active in-memory
grant state.

TaskSession workspace mutation methods share an in-process read/write revision
fence keyed by captured workspace identity, including across tasks and sessions
using the same workspace. A process holds a read lease from intent preparation
through completion; the four mutation methods take the matching write lease and
advance the revision before attempting a change. This invalidates a one-shot
approval after any TaskSession-managed mutation to that workspace, including a
failed mutation attempt. Process authorization, execution, and managed workspace
mutations also require the task to hold the registry-matched fence for its
captured workspace identity; if that fence cannot be established or verified,
those operations fail closed.
Direct host writes that bypass TaskSession are outside this fence and remain the
trusted embedding's responsibility. The digest binds captured directory
identities, not content snapshots; the host must keep the selected workspace,
toolchain, and registry integrity appropriate for its use.

The separate opt-in `run_goal_turn_with_process_failure_correction` Harness
path may start one same-task follow-up turn after a complete, cleanup-confirmed
Cargo nonzero exit or a consistent `NoTestsMatched` result. Brain receives the
original typed process observation and bounded output as untrusted context.
Denied or invalid requests, sandbox or setup failures, spawn failures,
signals, timeouts, cancellation, resource or output limits, truncated output,
and uncertain cleanup remain terminal. A later test verifies only when its
result is complete, successful, ran at least one test, and followed the last
workspace mutation; existing independent completion checks still apply. This
opt-in path uses the existing TaskSession grant and registered workspace tools
and does not alter the process profile or default Harness behavior.

## Required host capabilities

The initial supported deployment is a non-root Linux user service with
cgroup v2 delegation, an accessible user systemd manager, and Bubblewrap
0.10.0 or a compatible build that accepts every option used by the fixed
command, including `--ro-bind-fd`. The implementation requires `cgroup.kill`,
`close_range(CLOSE_RANGE_CLOEXEC)`, mandatory user/mount/PID/network/IPC/UTS/
cgroup namespaces, and disabled nested user namespaces. A historical fixture
run recorded Linux 6.8, systemd 255 and Bubblewrap 0.9.0; it predates the
current seccomp command and is not runtime evidence for the current Product
revision. A version string alone does not establish option compatibility. No
current-revision host fixture result is recorded here; other deployments are
unverified, and missing primitives or options fail closed.

The embedding process must be in the delegated user-manager cgroup subtree.
A login session outside that subtree can create a user unit but cannot
necessarily migrate its child into it; that is a setup failure, not permission
to run without containment. For an already provisioned deployment, launching
the embedding program as a user service (for example with `systemd-run --user`)
provides the appropriate parent context. This feature never edits systemd,
permissions, host configuration, or CI to make a failing preflight pass.

A trusted, inert keeper starts in a unique transient user unit first. Before
workspace code can launch, Tools reads back finite `cpu.max`, `memory.max`,
`memory.swap.max`, and `pids.max` and verifies attachment and whole-tree kill
access. A trusted `/usr/bin/true` probe uses the exact sandbox mounts, namespace
flags, descriptor handling and resource attachment that the Cargo launch uses.
It also detects Bubblewrap builds that reject required command options before
Cargo or workspace code is launched.
There is no optional namespace flag or direct-spawn fallback.

## Confinement and fixed bounds

- The captured workspace descriptor is mounted **read-only** at `/workspace`,
  which is the fixed cwd. Source and Cargo.lock are not modified. Tests that
  require source-tree writes fail rather than obtaining host write access.
- Only trusted `/usr`, `/lib`, `/lib64`, the selected toolchain, and the selected
  registry are mounted read-only. Host home, external workspace paths, control
  sockets and credential directories are absent. Workspace symlinks resolve
  in that sandbox view, not against the host root.
- All mounts of selected directories use pinned descriptors. The child joins
  the verified cgroup before Bubblewrap exec. Ambient descriptors are marked
  close-on-exec; stdin is closed and the payload receives no TTY.
- The root is read-only; `/tmp` is a private 1 GiB tmpfs and `/cargo` a private
  16 MiB tmpfs containing a read-only registry mount. `CARGO_HOME`, the build
  target, HOME and temporary files live in fixed `/tmp` subdirectories. The
  minimal `/dev` is private. Writable tmpfs storage also
  counts against the hard memory ceiling; no host output files are promoted.
- Start from an empty payload environment: only fixed PATH, HOME, TMPDIR,
  CARGO_HOME, CARGO_BUILD_JOBS (one), CARGO_PROFILE_DEV_DEBUG (zero),
  CARGO_PROFILE_TEST_DEBUG (zero), CARGO_INCREMENTAL (zero), CARGO_TARGET_DIR,
  RUSTC and LANG are set. Cargo incremental metadata and
  debug symbols are disabled to keep build artifacts within the per-file limit.
  No provider, GitHub, SSH, proxy, user-manager environment or credential is
  forwarded.
- No network namespace sharing; no capabilities, no privilege elevation, no
  nested user namespaces. Build scripts and tests are untrusted sandbox code.
- Per job: 3 GiB hard memory, zero swap, 128 processes/threads, CPU quota of
  one core, 30 CPU seconds per process, 256 descriptors, 64 MiB per file,
  zero core dumps, 180-second payload wall timeout, and 256 KiB combined raw
  stdout/stderr. Aggregate CPU accounting stops the job at 120 CPU seconds
  (poll-based accounting can overshoot slightly; the kernel quota, per-process
  CPU limit and finite supervisor lifetime remain enforced independently).
- The transient unit independently expires 15 seconds after the payload wall
  timeout, including setup, and uses control-group termination with a one-second
  stop grace and SIGKILL.
  It cannot outlive that bound merely because the caller crashes.

Both pipes are drained without blocking and retain stream identity. Reaching
an output bound terminates the job and explicitly marks truncation. Output is
lossily decoded, bounded untrusted evidence; JSON contexts escape control
characters and CLI rendering uses escaped strings, not raw terminal bytes.

## Cleanup, concurrency, and recovery

A process-local mutex plus a host-wide exclusive lease allow one profile at a
time. The lease in the private user runtime directory records only the unit
and cgroup identities and survives caller crashes. A later caller must verify
that the previous unit is inactive and its recorded cgroup is absent or empty
before reusing the lease. A corrupt record or inaccessible manager denies the
attempt. No duplicate dispatch or blind retry is performed.

Every exit path terminates the cgroup and reaps the direct child. A result is
successful only for Cargo exit zero, complete output and confirmed tree
cleanup. Nonzero exit, signal, setup failure, timeout, cancellation, output or
resource limit, and unknown outcomes remain distinct non-success states.
Memory and process-limit counters prevent an apparently successful exit from
hiding a quota event. When cleanup cannot be confirmed, the result is unknown,
the process retains a recovery hold, and the host lease is preserved. A host
maintainer must establish termination before resuming; deleting the lease
without checking the recorded unit/cgroup is unsafe.

## Verification

Ordinary `cargo test --all-targets --locked --offline` covers schema denial,
missing authorization, lifecycle/ownership checks, required resource readback,
filter validation and fixed argument placement, per-test execution evidence
and zero-match semantics, result semantics, and existing behavior. A disposable
offline Cargo fixture emits summary-shaped build-script warning output for both
matching and unmatched filters; only test-harness per-test result lines count.
It does not
pretend to validate real namespaces on a runner without prerequisites.

The ignored `real_offline_profile_is_confined_and_cleans_up` host fixture is
run explicitly from a delegated user service with `KRYPPER_TEST_TOOLCHAIN`
and `KRYPPER_TEST_REGISTRY` pointing to preprovisioned directories. These two
variables configure only the test harness; they are not product request fields
and are never forwarded to workspace code. The fixture uses a dependency-free
locked Cargo workspace, an outside canary, and the actual production sandbox
and supervisor. It dispatches the registered profile through WorkspacePlan and
Harness with a literal filter, verifies that the selected test ran while an
unrelated test was filtered, checks the selected filter and execution evidence
in the process observation, and confirms that a filter matching no tests is a
typed failure. It also asserts the successful turn outcome, lifecycle state
and metadata record. It covers absent secrets/control sockets, denial of a live
host-loopback listener (reachable from the host before and after the run),
escape denial, nonzero exit, timeout, output bounds,
cancellation, CPU/memory/process ceilings and cleanup. Lower-only private test
bounds make timeout/output tests short; public profile limits are fixed.

The ignored `local_model_task_patches_then_runs_a_filtered_offline_cargo_test`
and `local_model_task_cannot_complete_after_a_zero_match_offline_cargo_test`
fixtures use a loopback scripted model endpoint to drive the normal Harness and
task grant through `apply_patch`, `offline_cargo_test`, and task control. The
matching case checks that the bounded completion prompt contains the literal
filter and one executed test before accepting `complete_task`, while the
companion case checks the typed `no_tests_matched` failure stops before task
control can complete. Both use a disposable dependency-free workspace and
verify an outside-workspace canary remains unchanged. They use the same
preprovisioned toolchain and registry settings as the host fixture above.

The broader [process safety design](process-execution-safety.md) remains a
historical proposal and requirements reference. See
[Current state at dcd22b5](process-execution-safety.md#current-state-at-dcd22b53b176668ae4b5f134d98b21b84ce55f40)
for the implemented architecture and its evidence classes. This implementation
provides no generic shell, interactive process session, new provider, deployment
path, network-enabled profile, macOS/Windows backend, or persistent build cache.

The additional ignored `real_product_workspace_runs_the_fixed_profile` fixture
accepts `KRYPPER_TEST_WORKSPACE` in the trusted test harness only, and runs that
selected repository through the unchanged fixed production profile. It was
also exercised against this implementation's full source workspace, including
all ordinary unit/integration/example targets: Cargo exited zero with complete
output and confirmed cleanup. This is a local host validation result, separate
from required GitHub Verify and independent code review.
