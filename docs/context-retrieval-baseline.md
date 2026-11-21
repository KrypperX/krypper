# Context retrieval baseline (#231, slice 1)

This note records what the context path actually does today, before any repository map, symbol
navigation or compaction work lands. It is an audit of code at baseline `ef75809`, not a proposal
and not a measurement of model quality. Where a number appears, it is a constant in the cited
module; no benefit is claimed for anything not yet implemented.

## Call path

1. `src/harness/context.rs` builds prompts from observations, goal text and optional project
   guidance, and appends repository excerpts and, when supplied, memory.
2. `src/harness/skill_context.rs` inserts activated skills between project guidance and repository
   excerpts.
3. `src/harness/memory_context.rs` appends opt-in memory last.
4. `src/brain/request.rs` selects the plan or task builder and hands the prompt to the provider
   adapter.
5. Retrieval authority stays in `src/tools/workspace_safety/` and `TaskSession`; prompts only
   carry bounded, labelled text.

Nothing in this path grants a tool, capability or execution profile. Model output and retrieved
content remain untrusted input.

## Bounds in force

| Layer | Constant | Value | Source |
| --- | --- | --- | --- |
| Repository excerpts | `MAX_REPOSITORY_CONTEXT_BYTES` | 4 KiB | `src/harness/context.rs` |
| Repository queries per plan | `MAX_REPOSITORY_QUERIES` | 3 | `src/harness/context.rs` |
| Repository snippets | `MAX_REPOSITORY_SNIPPETS` | 4 | `src/harness/context.rs` |
| Snippet length | `MAX_SNIPPET_LINES` / `MAX_SNIPPET_LINE_BYTES` | 3 lines / 160 B | `src/harness/context.rs` |
| Query token floor | `MIN_QUERY_BYTES` | 3 bytes | `src/harness/context.rs` |
| Observation payload | `MAX_OBSERVATION_BYTES` | 2 MiB | `src/harness/context.rs` |
| Activated skills | `MAX_ACTIVE_SKILLS` | 4 | `src/tools/skills/activation.rs` |
| Skill context | `MAX_SKILL_CONTEXT_BYTES` | 2 KiB, split evenly across selected skills | `src/tools/skills/activation.rs` |
| Memory render | `MAX_MEMORY_CONTEXT_BYTES` | 4 KiB | `src/memory/retrieval.rs` |
| Memory recall | `MemoryRecallPolicy` default | 16 records / 4 KiB | `src/memory/integration.rs` |
| Memory query | `MAX_QUERY_RECORDS` / `MAX_QUERY_BYTES` | 64 / 32 KiB | `src/memory/contract.rs` |
| Search query | `MAX_SEARCH_QUERY_BYTES` | 4 KiB | `src/tools/workspace_safety/search.rs` |
| Search scan | `MAX_SEARCH_BYTES` | 16 MiB | `src/tools/workspace_safety/search.rs` |
| Tree walk | `MAX_TREE_DIRECTORY_DEPTH` / entries | 16 / 4096 per directory | `src/tools/workspace_safety/search.rs` |
| File read | `MAX_FILE_SIZE` | 1 MiB | `src/tools/workspace_safety/types.rs` |
| Task checkpoint | `MAX_TASK_SESSION_CHECKPOINT_BYTES` | 1 MiB | `src/core/lifecycle/checkpoint.rs` |

Budgets are per layer. There is no shared total across guidance, skills, repository excerpts and
memory, so a prompt can carry the sum of all four ceilings.

## Deterministic attribution

- Excerpts carry `path`, `start_line`, `end_line`, `query` and `revision`
  (`RepositorySnippet`). `revision` is the first six bytes of the file's SHA-256 rendered as 12 hex
  characters, so an excerpt is attributable to exact content.
- Memory excerpts carry `id`, `kind`, `tags`, `provenance` and `retention`; provenance includes the
  source kind, the recording time and an optional revision.
- Skills carry `name`, `description`, `manifest_path` and capped `instructions`.
- Ordering is deterministic: repository queries come from goal tokens in order, de-duplicated and
  capped at three; memory is ordered by newest provenance then record id.

## Stale, missing and unknown content

- A search hit is dropped as stale when the file is unreadable, the line no longer contains the
  query, or the line number is zero. Stale hits increment `stale_skipped`.
- A search error that reports an oversized file increments `oversized_skipped` instead of failing.
  Any other search error fails the context build.
- A non-running task turn fails closed with `TaskUnavailable`; context is never built from a
  guessed workspace state.
- Repository lines containing any of five directive markers are replaced with
  `[neutralized directive-like line]` and counted in `neutralized_lines`.
- When the snippet budget is reached, the last snippet is removed and `truncated` is set. Empty
  results render an explicit "no bounded matches were available" line, never invented content.
- Memory recall reports `truncated` when a query bound or the render budget dropped eligible
  records, and `skipped_expired` for records that expired before the query clock.

## What exists versus missing

Exists and is reused: bounded goal-token search, range reads, snippet neutralisation, provenance
revisions, skill activation with a shared byte budget, opt-in memory recall with isolation and
evaluation fixtures, metadata-only checkpoints.

Missing, in the order the issue proposes:

1. A shared total context budget across the four layers (today each caps itself).
2. A repository map, and any measured comparison of it against bounded search. No such measurement
   exists yet, so no symbol-navigation work starts before it.
3. Opt-in long-session compaction that preserves goal, authorization, constraints, tool outcomes and
   provenance while keeping raw lifecycle evidence authoritative.
4. Held-out bug-location and multi-file comparison fixtures at matched budget.

## Next slices

- Slice 2: a bounded, deterministic repository map behind an opt-in flag, plus the measurement
  harness that compares it with bounded search on representative Rust repositories.
- Slice 3: opt-in compaction with explicit provenance and stale-summary rejection.
- Slice 4: matched-budget evaluation report covering success, recall, tokens, latency, stale
  retrieval and safety regressions.

Each slice is a separate PR and must keep default CLI and provider behaviour unchanged.

## Limits of this note

No performance, token-saving or success-rate claim is made here. The numbers above are constants
read from the cited modules at `ef75809`; changing a constant requires updating this table in the
same PR.
