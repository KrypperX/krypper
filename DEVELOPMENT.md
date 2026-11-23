# Development guide

This guide records implemented code separately from a staged, revisable
direction for future structural changes. `src/core/task.rs` and
`src/core/lifecycle.rs` are implemented Core slices. `src/tools.rs` and its
workspace-safety modules are the implemented Tools boundary. The model-backed
decision-source boundary in `src/brain/` is implemented; broader **Core** and
**Brain** layout remains proposed.

The historical process-execution design is documented in
[`docs/process-execution-safety.md`](docs/process-execution-safety.md). One narrow,
explicitly authorized Linux offline Cargo profile is now implemented through
Tools and TaskSession; see [its contract](docs/offline-cargo-profile.md). The CLI
does not grant process authority, and generic process execution remains absent.

## Platform support

Linux and macOS have supported workspace-containment backends in
`src/tools/workspace_safety/platform/`. The Unix syscall boundary owns native
descriptors, metadata structures, modes, path encoding, and durability calls;
callers above it use `DirHandle`, `FileIdentity`, `FileKind`, `CreateMode`, and
portable path components. Linux also provides the verified bwrap/cgroup Cargo
sandbox. macOS keeps that capability unavailable and returns
`ProcessOutcome::SandboxUnavailable`; MCP stdio remains available through its
POSIX process-group and nonblocking-pipe implementation.

Windows is compile-checked only. Its workspace backend is a typed unavailable
stub; `TaskSession::new_in_workspace` reports
`TaskSessionWorkspaceErrorKind::PlatformUnavailable`, and MCP stdio returns
`McpLifecycleError::PlatformUnavailable` before a child process can be created.
The Linux process sandbox and supervisor are not compiled there. Do not replace
these failures with an unconfined implementation; native containment and process
supervision require a separate reviewed port.
The CI matrix runs the full suite on Linux and macOS and checks all targets on
Windows without running tests.

## Current implementation

- [`src/core/task.rs`](src/core/task.rs) defines the `Task` and `Turn` domain
  types, their state transitions, and their lifecycle and metadata-only
  activity records.
- [`src/core/lifecycle.rs`](src/core/lifecycle.rs) defines `TaskSession` and
  owns session-wide task ordering and lifecycle coordination, snapshots, and
  checkpoint serialization, validation, workspace binding, and lifecycle
  state. It re-exports the task domain types for compatibility.
- [`src/tools.rs`](src/tools.rs) exposes the existing workspace operations to
  task-turn and CLI callers through one crate-internal Tools boundary.
  [`src/tools/registry.rs`](src/tools/registry.rs) provides a typed registry of
  the workspace-operation names and fixed offline Cargo profile, descriptions,
  and request schemas.
  Brain derives provider request definitions from that registry while keeping
  task-control decisions on their existing path. Registry metadata describes
  requests only; `TaskSession` and Tools remain responsible for lifecycle,
  workspace binding, validation, and safety enforcement.
  [`src/tools/workspace_safety.rs`](src/tools/workspace_safety.rs) owns workspace
  root capture and the file and directory safety rules, including containment,
  symlink and file-type handling, limits, and atomic replacement.
  [`src/tools/workspace_safety/search.rs`](src/tools/workspace_safety/search.rs)
  owns bounded text search and tree traversal. `TaskSession` keeps lifecycle,
  identity, ownership, running-turn checks, and workspace binding in Core; its
  workspace operations delegate to Tools after those checks. The CLI uses the
  same Tools operations.
- [`src/task_lifecycle.rs`](src/task_lifecycle.rs) re-exports that lifecycle
  API as a compatibility facade for existing callers, including the types
  exposed by `core::task`.
- [`src/harness/`](src/harness/) contains task-turn execution and session
  preparation. Its `execution.rs` turn loop requests a decision, executes
  supported workspace actions through Tools, and records turn results through
  `TaskSession`. Its [`execution/planned.rs`](src/harness/execution/planned.rs)
  module provides the opt-in `run_goal_turn_with_plan` entry point. It adapts
  typed Brain operations into the existing bounded loop, rechecks each against
  the current Tools registry, and permits only completion, continuation, or
  cancellation from the task-control source. The existing loop executes each
  operation through `TaskSession`, counts actions against the caller's budget,
  and stops after a rejected or failed step. Its
  `run_goal_turn_with_completion_correction` entry point optionally runs one
  correction after independent checks fail or are unknown, using the remaining
  slots from the original action budget. The separate
  `run_goal_turn_with_process_failure_correction` entry point can also run one
  follow-up turn after a complete, cleanup-confirmed Cargo nonzero exit or
  typed zero-match result. It preserves the typed failure for Brain and requires
  a later complete passing test after the last workspace change, in addition to
  the existing independent checks. Its
  [`verification.rs`](src/harness/verification.rs) module retains the existing
  stop-reason reconciliation used by the CLI and adds the opt-in
  `reconcile_task_state_with_checks` and `verify_completion_checks` APIs.
  Callers provide required checks and separate evidence from TaskSession
  observations or deterministic local checks. A task succeeds through
  `TaskSession` only when at least one required check has exactly one passing
  evidence record; missing, duplicate, unknown, or truncated input cannot pass.
  Results include at most 32 checks and 256 bytes per description or evidence
  summary, with omitted input reported as unknown. Verification returns results
  to the caller without invoking Tools, changing permissions, writing workspace
  data, persisting checks, retrying, or repairing work. The CLI continues to use
  the unchanged default reconciliation path. Its
  [`recovery.rs`](src/harness/recovery.rs) module
  prepares checkpoint bytes at a continued-turn pause boundary, coordinates
  checkpoint restoration, validates the single-task resume conditions, and
  starts the next turn through `TaskSession`; Core remains the only lifecycle
  and checkpoint state owner. Its [`context.rs`](src/harness/context.rs) module
  formats each model prompt and serializes the current workspace observations
  under the existing context size limit; it is stateless and uses the current
  goal and decision budget.
- [`src/brain/mod.rs`](src/brain/mod.rs) owns the model-backed Ollama decision
  source, shared tool definitions, request construction, and response
  validation. Its [`plan.rs`](src/brain/plan.rs) module defines the bounded
  workspace-only plan type; the opt-in Ollama plan request is built from the
  same Tools registry and cannot include task-control operations. Its
  [`providers.rs`](src/brain/providers.rs) module implements provider-specific
  reactive sources, request contracts, and response validation; `cli.rs`
  retains the existing flag and environment based provider selection.
  Decision sources return proposed workspace actions through the existing
  decision-source interface; they do not execute those actions. The focused
  tests in [`tests.rs`](src/brain/tests.rs) use local fixtures.
- [`src/cli/task_args.rs`](src/cli/task_args.rs) contains the pure parser for
  task actions, paths, search arguments, and read ranges, with focused local
  regression tests. [`src/cli.rs`](src/cli.rs) dispatches commands and runs
  the CLI, and asks Harness to prepare checkpoint bytes and resumed sessions.
  Its private [`outcome.rs`](src/cli/outcome.rs) module renders task outcomes
  and workspace observations. Its private
  [`checkpoint_files.rs`](src/cli/checkpoint_files.rs) module performs bounded
  checkpoint reads, output-path validation, and create-new writes.
  Its private [`verified_task.rs`](src/cli/verified_task.rs) module owns the
  opt-in planned workflow, TaskSession evidence aggregation, and caller task
  controls without changing the default CLI path.

The current CLI still drives task execution. The Tools module provides the
workspace-safety implementation used by both task-turn operations and CLI
filesystem commands, with the existing operations described by the typed
registry. The opt-in `task --verified` path composes the existing Ollama planner,
completion-correction Harness flow, TaskSession observations, and registered
Tools without changing the default caller-directed or model-selected path. Its
file checks use only current-turn TaskSession evidence to confirm a successful
file operation on an exact path; failed file evidence takes precedence over
success, and directory operations do not count. The checks do not validate
file contents. Task control remains with the caller through `complete`,
`continue`, or `cancel`. The CLI's
`--max-actions` counts Harness workspace and task-control actions, not Ollama
requests. A correction is eligible only after a normally completed turn with
failed or unknown checks and at least three actions remaining. The threshold
lets the bounded correction planner reserve two decision slots and still allow
at least one planned operation; its model requests are not added to the
displayed Harness action count. One correction is allowed per CLI invocation.
With checkpoint output, continuing after the initial uncorrected turn pauses
before correction; continuing after a correction saves the post-correction
boundary and leaves the allowance used. Checkpoints do not retain the mode,
check list, or previous observations, so callers repeat them and establish
fresh evidence on resume.
Moving more CLI orchestration remains part of the staged direction below.

## Decision failure reporting

Brain maps request, provider, response and model-output errors to a stable
`DecisionFailureCode` while preserving the existing
`WorkspaceAction::Unsupported` variant. Classified action labels use the
reserved `decision_failure:<code>` form; callers can use
`WorkspaceAction::decision_failure_code()`,
`GoalTurnError::decision_failure_code()` or
`ModelError::decision_failure_code()` instead of parsing display text. Codes
distinguish provider unavailability, rate limiting, request rejection,
malformed responses, invalid model output, invalid arguments and local
request/configuration failures. Unsupported tool calls keep the existing
extension-dispatch path and return no failure code.
When an HTTP response status is available, its numeric value is preserved as
an additional safe suffix, such as `decision_failure:provider_rejected:401`;
`DecisionFailureCode::http_status_from_action_label()` extracts it.

The `Transport(String)` payload is internal diagnostic input only. Its
`Display` output is fixed and omits that payload, and local transport creation
sites store generic details, so provider text, endpoints or credentials cannot
be copied into task outcomes through this path. Provider status codes are
classified as rate limited for HTTP 429, unavailable for HTTP 408, 425 or 5xx,
and rejected for other status codes. This classification does not add automatic
retries.

## Bounded planning prototype

Call `OllamaDecisionSource::create_plan` and pass its result to
`run_goal_turn_with_plan` to opt into one ordered plan. The plan can contain
only registered workspace operations. Harness revalidates each operation and
delegates it to `TaskSession`; an operation failure or denial stops the rest of
the plan. Task-control decisions stay on the existing path, and a plan does not
contain a success outcome. The offline fixtures compare planned and reactive
execution for an observed successful result and a denied operation, including
their action order, action counts, outcomes, lifecycle/activity records, and
workspace containment. These fixtures do not measure a general quality
improvement.

## Opt-in bounded reflection

`OllamaDecisionSource::create_reflective_plan` accepts a prior Harness result
and its `TaskSession`. Before asking the existing local Ollama endpoint for a
reflection, it checks that the result's ordered observations, stop reason, and
outcome match the turn's recorded activity and outcome. The reflection input
contains those observations and the recorded stop and outcome kinds; it omits
the completion text and any plan or model claim. One response is limited to
256 generated tokens and 2 KiB. Its text is passed to a later bounded
workspace-plan request as untrusted advisory context, alongside the verified
outcome. The method returns a plan only: Harness applies the existing action
budget, `TaskSession` path, and Tools checks when that plan is run. Reflection
does not mutate task or turn records, execute operations, retry requests, or
persist state. The reactive path and the existing `create_plan` method remain
the default behavior. Local fixtures cover recorded success and a denied
outside-workspace operation with a reflection claim that conflicts with the
recorded failure.

## Opt-in completion correction

`run_goal_turn_with_completion_correction` composes one existing bounded initial
turn with independent completion checks and, when needed, one correction cycle.
The initial turn may use the reactive decision source or an existing bounded
plan. The check callback must use TaskSession observations or deterministic
local checks outside Brain. Passing initial checks go directly through
TaskSession reconciliation without requesting a correction. After a normally
completed initial turn with failed or unknown checks, a correction is eligible
only when at least three Harness action slots remain for the correction plan's
workspace operation and task control. The failed or unknown result is supplied
to `OllamaDecisionSource::create_completion_correction_plan` as bounded,
untrusted advisory context. Harness runs that one returned plan through
TaskSession and the existing Tools registry with only the original action-budget
slots remaining. Harness action totals include workspace and task-control
actions; Ollama model requests are outside that count, while each plan is
individually bounded by the slots passed to it. The same `OllamaDecisionSource`
carries its existing decision counter across the initial turn, reflection,
plan, and correction turn. Harness then invokes the independent check callback again.
Invalid plans, unsupported actions, denied workspace paths, failed
operations, and exhausted budgets stop the cycle. TaskSession records success
only after required checks pass on a completed initial turn or the single
correction turn;
missing, duplicate, unknown, and truncated evidence stays non-successful. The
reactive default, CLI, public compatibility facade, and lifecycle behavior
outside this opt-in entry point are unchanged. Offline fixture tests cover
initial pass, failed and unknown checks followed by a passing correction, a
failed post-correction check, a denied correction, exhausted budget, and the
existing reactive and workspace-safety behavior. These tests verify control
flow and safety; they do not measure general task quality. The path adds no
provider, persistence, dependency, CLI change, or user-visible output.

## Opt-in process failure correction

`run_goal_turn_with_process_failure_correction` permits one follow-up turn after
the last initial observation is a complete `Exited(nonzero)` or a consistent
`NoTestsMatched` result. The correction planner receives that original typed
`ProcessResult`, including its bounded output, filter, test counts, truncation
flag, and cleanup status. Harness starts one new turn in the same task and
executes any correction and test rerun through the existing registered
operations and TaskSession grant, using only the remaining action budget.

Denied or invalid requests, sandbox or setup failures, spawn failures,
signals, timeouts, cancellation, resource or output limits, truncated output,
and uncertain cleanup are terminal. A correction attempt cannot establish
test success: verification requires a later complete successful Cargo result
that ran at least one test after the last workspace mutation. Existing
independent completion checks must also pass. The process profile, tool grant,
operation registry, workspace confinement, and default turn path remain
unchanged. Deterministic Harness fixtures script typed process results; the
host-specific execution fixtures remain separate.

## Proposed boundaries

Treat these boundaries as a revisable direction and introduce them incrementally:

- **Core** is intended to own runtime, task, session, and lifecycle state. Its
  implemented `core::task` slice contains the task and turn domain
  implementation; `core::lifecycle` remains the sole owner of session-wide
  task ordering, coordination, snapshots, and checkpoint recovery. The two
  modules share the existing lifecycle, history, and activity records.
- **Brain**, implemented for model-backed decision sources in
  `src/brain/`, turns observations into proposed decisions through provider
  interfaces. It proposes actions; it does not execute workspace capabilities.
- **Harness** coordinates context construction, execution, recovery, and
  verification. Its context is rebuilt per decision from the current goal,
  remaining decision budget, and observations; it does not keep stored model
  context or a second lifecycle store, or bypass Tools to perform actions.
- **Tools** is the final enforcement boundary for workspace access and safety.
  Its implemented workspace-safety operations are shared by task turns and CLI
  filesystem commands. The typed registry describes those operations for Brain
  requests but does not grant permissions or replace local validation.
- **CLI and future interfaces** present data and consume runtime snapshots
  without owning task state. Moving the current CLI to that role must preserve
  its existing syntax and defaults.

## Read-only observability

Use the existing `TaskSession::snapshot()` and typed lifecycle/activity records
as the contract; do not add a parallel event model. A snapshot is an immutable,
on-demand view of stable session, task, and turn IDs and their current states.
It also exposes each task's lifecycle and activity sequence high-water marks.
Each position belongs to its own history; zero means that history is empty.
Creating a snapshot does not consume history or change session state.

Lifecycle and activity positions are independent. Do not combine them into a
synthesized total order. Activity records remain metadata-only, such as an
action category, a count, a byte count, or a line range. Snapshots and metadata
records must not expose raw goals, prompts, provider data, task or turn
outcomes, workspace paths or contents, credentials, or secrets.

This contract applies to read-only runtime snapshots and interfaces that
consume them. It does not redefine the CLI's existing per-invocation output.

## Migration rules

- Move one boundary per reviewed PR.
- Preserve public Rust APIs, CLI syntax and defaults, security policy,
  lifecycle and checkpoint behavior, and observable behavior.
- Keep compatibility facades when module paths move.
- After each code move, add focused regression coverage and run the relevant
  local suite.

Defer event subscriptions, TUI or Web UI, remote transport, and durable event
storage until a concrete consumer and approved scope require them. This guide
does not introduce runtime behavior, dependencies, or a new state or event
system.

## Structured streaming foundation (draft, opt-in)

The `src/streaming.rs` module defines a versioned presentation envelope,
fallible synchronous event sink, stream-local sequence counter, and JSONL
writer adapter. It does **not** subscribe to the runtime or expose a CLI flag.
OpenAI Responses, xAI Responses, and Anthropic Messages have opt-in native
SSE decision-source paths (via `KRYPPER_RESPONSES_STREAM` for OpenAI/xAI and
`KRYPPER_ANTHROPIC_STREAM` for Anthropic). Set either variable to `1` or `true` (case-insensitive) to enable streaming; unset, empty, `0`, `false`, and other values disable it. Previously any present value enabled streaming, so existing `yes`/`on` configurations must be changed to `1` or `true`. These paths assemble and validate
one complete tool action before returning it; they do not execute the action.
Their text-delta callbacks are internal and the normal `choose` entry point
discards text deltas. Therefore this is **not** end-to-end CLI streaming.

All other configured decision sources (Gemini, Ollama, and the
OpenAI-compatible chat-completions providers Cloudflare Workers AI, Groq,
OpenRouter, DeepSeek, Z.AI, and Mistral) continue using their existing
non-streaming requests and parsers. Setting either SSE opt-in variable does
not enable native streaming for these providers. Do not silently switch
their tool-call protocol to an incompatible SSE parser.

Offline local-HTTP integration fixtures exercise successful completion,
truncated streams, and malformed events. Equivalent successful tool calls
are checked against the non-streaming action parsers for OpenAI, xAI, and
Anthropic. These fixtures do not prove live provider interoperability or
CLI rendering. Keep end-to-end streaming claims deferred until runtime
integration and provider smoke tests are completed.

Stream sequence numbers are scoped to a single emitter, not to the persisted
TaskSession lifecycle or activity histories. The independent lifecycle and
activity high-water marks remain authoritative for read-only observability.
A failed delivery does not advance the emitter's sequence, but a partially
written JSONL record may have reached a failed writer; consumers must not
assume that a failed write is atomic. No replay, resumption, transport
reconnection, or automatic cancellation is provided by this foundation.

The streaming module is transport-only and never grants tool authority,
mutates task state, or persists event payloads. Future runtime integration
must preserve the existing TaskSession owner, workspace safety, checkpoints,
CLI defaults, and the metadata-only observability contract. In particular,
model deltas and raw tool output must not be projected into metadata-only
snapshots or durable activity records.
