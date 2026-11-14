# Development guide

This guide records implemented code separately from a staged, revisable
direction for future structural changes. `src/core/lifecycle.rs` is the
implemented Core slice; the broader **Core**, **Brain**, and **Tools** layout
remains proposed.

## Current implementation

- [`src/core/lifecycle.rs`](src/core/lifecycle.rs) defines `TaskSession` and
  owns the current in-memory session, task, turn, lifecycle history, and
  checkpoint behavior.
- [`src/task_lifecycle.rs`](src/task_lifecycle.rs) re-exports that lifecycle
  API as a compatibility facade for existing callers.
- [`src/harness/`](src/harness/) contains task-turn execution. It coordinates
  decisions and workspace actions through the current session API.
- [`src/model.rs`](src/model.rs) and its provider submodule turn observations
  into proposed workspace actions through decision-source interfaces.
- [`src/file_safety.rs`](src/file_safety.rs) contains the current private
  workspace helpers that enforce path, containment, and file-operation rules.
- [`src/cli.rs`](src/cli.rs) parses and runs the CLI, drives the current
  session and turn APIs, and presents command output.

Only `core::lifecycle` is implemented from the proposed module layout. The
broader Core boundary, Brain, and Tools are not yet implemented modules. The
current CLI still drives task execution, and workspace safety remains in
`file_safety.rs`; the layout below describes a staged direction for those
responsibilities.

## Proposed boundaries

Treat these boundaries as a revisable direction and introduce them incrementally:

- **Core** is intended to own runtime, task, session, and lifecycle state. Its
  implemented `core::lifecycle` slice remains the single owner of current
  task/session state and supplies read-only snapshots to interfaces.
- **Brain** turns observations into proposed decisions through provider
  interfaces. It proposes actions; it does not execute workspace capabilities.
- **Harness** coordinates context, execution, recovery, and verification. It
  does not keep a second lifecycle store or bypass Tools to perform actions.
- **Tools** registers and executes capabilities and remains the final
  enforcement boundary for workspace access and safety.
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
