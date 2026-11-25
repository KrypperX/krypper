# Safe multi-file plan impact analysis

Krypper exposes an opt-in Harness path that analyzes a workspace plan before applying it. The
verified task workflow uses this path for its initial and correction plans. Plans that edit
multiple paths, change a detected public Rust API, have references that remain or may remain after
the plan (including in another part of a mutation target), or contain actionable unknown or
truncated evidence receive an additional review request from the same local Ollama model. If review
changes the impact report, the final report is sent back for one confirmation; the plan must remain
identical and the report must still match before execution.

Library callers can use the same flow:

```rust,ignore
let checked_plan = create_impact_checked_plan(
    &session,
    task_id,
    turn_id,
    &mut model,
    goal,
    &observations,
    action_budget,
)?;

let result = run_goal_turn_with_impact_checked_plan(
    &mut session,
    task_id,
    turn_id,
    goal,
    action_budget,
    Ok(checked_plan),
    &mut task_control,
)?;
```

The caller supplies the existing `TaskSession`, running turn, `OllamaDecisionSource`, and action
budget. Each impact review consumes one decision slot; one slot remains reserved for existing task control. The flow preserves the existing workspace Tools
registry, path policy, and patch executor.

## Baseline audit

Baseline: `4932833911f888f5dbfad0cba4e8d9041b1b1c04`.

- `src/tools/workspace_safety/patch.rs` already validates bounded, single-file unified patches and
  rejects malformed or overlapping hunks. `src/tools/workspace_safety/replace.rs` owns confined
  existing-file replacement, and `src/tools/registry.rs` owns the registered workspace and offline
  Cargo operations.
- `src/harness/execution/planned.rs` already runs bounded plans, deterministic completion checks,
  and limited correction. `src/cli/verified_task.rs` already wires those checks into `--verified`.
- The baseline has no pre-mutation report of changed symbols, bounded source references, affected
  tests, or public API uncertainty. Reviewed plans also do not carry full-content revisions to the
  atomic replacement boundary, and task-level test evidence does not preserve filter scope and
  workspace changes across continuation or checkpoint restore.

This change fills those gaps through the existing Tools, Harness, Brain, and CLI seams. It adds no
second patch executor, shell runner, process authority, or workspace grant.

## Evidence and bounds

- At most 16 mutation paths, four searched identifiers, and 64 reported reference ranges are
  retained. Each existing workspace search uses the Tools layer's 16 MiB bound.
- Existing-file patch and replacement previews use the same transformation rules as writes. Each
  normalized workspace-relative target may be mutated only once per plan; duplicate paths and
  lexical aliases fail before writes. Windows case-equivalent aliases are also rejected.
- Reference ranges include a workspace-relative path, one-based line, one-based byte columns,
  classification, confidence, and whether the reference remains or may remain after the plan.
  Test paths and public API references are called out separately.
- Public Rust API changes use conservative lexical checks. Changed public item names are reported
  as changes; a changed file with stable public names is reported as unknown because signatures,
  fields, variants, and other members are not parsed. Symbol references use bounded literal search.
  `coverage` describes whether those bounded searches hit a limit within the workspace search
  scope. Search follows applicable `.gitignore` rules and skips `.git` metadata directories, so
  references in ignored paths remain outside the report. A separate `limitations` field says that
  searches do not resolve Rust semantics, macros, generated code, or indirect dependencies.
- Search failures, range changes, symbol limits, and result limits are reported as unknown or
  truncated evidence. They are never represented as proof that no references exist.
- A final read-only analysis is repeated before execution. Existing-file writes recheck the full
  SHA-256 revision immediately before atomic replacement and verify the committed bytes. This
  narrows stale-write exposure but does not lock out a concurrent external writer between the
  revision check and rename. Create operations continue to use their existing create-only or
  confined path policy.

Files are atomic individually. A multi-file plan is not a cross-file transaction: if a later
operation fails, earlier successful edits remain and the turn records both success and failure
observations. Callers must inspect that evidence and run recovery explicitly; Harness does not
claim rollback.

The analyzer is an impact aid, not a measured quality claim. This change is based on
`4932833911f888f5dbfad0cba4e8d9041b1b1c04`. A reproducible comparison against the existing planner
using the same model, task fixtures, and action budget is still required before claiming improved
success rate, cost, or safety. That live comparison requires a configured model runtime; offline
fixtures verify the report, review request, limits, and stale-write behavior only.
