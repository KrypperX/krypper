# Development guide

This guide records implemented code separately from a staged, revisable
direction for future structural changes. `src/core/task.rs` and
`src/core/lifecycle.rs` are implemented Core slices. `src/tools.rs` and its
workspace-safety modules are the implemented Tools boundary. The model-backed
decision-source boundary in `src/brain/` is implemented; broader **Core** and
**Brain** layout remains proposed.

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
  the existing workspace-operation names, descriptions, and request schemas.
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
  [`verification.rs`](src/harness/verification.rs)
  module reconciles completed-turn stop reasons to terminal task states through
  `TaskSession`'s existing transition API; the CLI invokes it before rendering
  results. Its [`recovery.rs`](src/harness/recovery.rs) module
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
- [`src/cli.rs`](src/cli.rs) parses and runs the CLI, reads and writes
  checkpoint files, presents command output, and asks Harness to prepare
  checkpoint bytes and resumed sessions.

The current CLI still drives task execution. The Tools module provides the
workspace-safety implementation used by both task-turn operations and CLI
filesystem commands, with the existing operations described by the typed
registry. Moving CLI orchestration remains part of the staged direction below.

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
