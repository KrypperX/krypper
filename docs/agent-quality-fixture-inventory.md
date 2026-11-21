# Agent quality benchmark fixture inventory

This inventory supports issue #234 and PR #240. The benchmark is offline and scripted:
it does not call a model provider, launch host processes, mutate valuable workspaces,
or add a required CI check. The fixture revision is pinned by
`tests/agent_quality/benchmark.json` and must resolve to the source revision recorded
below.

Fixture revision: `c69b604cb34a536546c2cc4212fa862b8b5b9ce3`

## Reused offline evidence

| Area | Existing offline coverage | Reused by benchmark |
| --- | --- | --- |
| Completion correction | `tests/harness_completion_correction.rs` | reflection/correction fixture and retry path through the existing Harness API |
| Outcome verification | `tests/harness_outcome_verification.rs` | independently checked `VerificationObservation` and evidence failure classes |
| Planned execution | `tests/planned_execution.rs`, `src/harness/execution/planned.rs` | matched-budget planned and reactive arms, plus invalid/over-budget planning behavior |
| Execution compatibility | `tests/harness_execution_compatibility.rs` | ordered actions, denial isolation, budget stop, cross-run workspace isolation |
| Checkpoint/restart | `tests/task_lifecycle/checkpoint.rs` | checkpoint export and workspace-bound restore without repeating a workspace action |
| Process outcomes | `src/tools/process/tests.rs`, `docs/offline-cargo-profile.md` | timeout, cancellation, malformed/truncated output and sandbox-unavailable evidence are scripted, not real process launches |
| CI verification | `.github/workflows/verify.yml` | fmt, check, build, all-target tests and catalog drift check; no new required check |

The benchmark uses the existing `TaskSession`, Harness completion-correction and
independent verification APIs. `tests/agent_quality/support.rs` supplies the bounded
temporary workspace and git helpers needed to pin fixture blobs; it does not introduce
a competing runtime or permission authority.

## Scripted fixture classes

`tests/agent_quality/fixture.rs` implements the classes declared in
`tests/agent_quality/benchmark.json`:

- `artifact_creation`, `bug_fix`, `cross_file_edit`: isolated disposable git workspaces.
- `failed_test`, `interrupted_execution`, `denied_operation`: bounded outcome evidence.
- `timeout`, `malformed_provider_output`, `cancellation`, `sandbox_unavailable`: scripted failure evidence.
- `truncated_evidence`, `checkpoint_restart`, `reflection_correction`: verification and recovery evidence.

Every class records independently checked outcome, safety incidents, retries, duplicate
effects, latency, tokens and cost units. The report publishes distributions, Wilson 95%
intervals where sample sizes allow, thresholds derived from observed baseline runs, and
findings linked to concrete evidence and follow-up actions.

## Limits

Provider-backed and expensive end-to-end tests remain outside ordinary offline CI. The
`provider_calls` field is zero and the scope is `scripted_fixtures`. Real-provider
comparison, host process-launch measurement and general product-quality claims are not
established by this benchmark.
