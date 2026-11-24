# Model routing evaluation

`scripts/model_routing_evaluation.py` compares the saved manual route with the
automatic route on the existing four isolated Rust task fixtures. It runs the
same goal and bounded repository context in both arms, randomizes arm order per
task and repetition, and checks each result with an independent
`cargo test --offline` run plus the fixture's exact expected changed paths.
The suite tags code edits, multi-file reasoning, and workspace tool use. It is
a small initial sample, not a general benchmark of arbitrary projects.

## Offline preflight

Build the CLI and run the default, offline mode:

```sh
cargo build --locked --bin krypper
python3 scripts/model_routing_evaluation.py
```

Offline mode checks that every fixture still has its declared baseline failure,
validates the task categories and pinned source revision, and reports the planned
manual/automatic matrix. It makes no model-provider request and does not need
credentials or saved probe evidence. Deterministic tests for the route parser,
isolated configuration, telemetry cost calculations, and secret scrubbing run
with:

```sh
python3 -m unittest discover -s tests -p 'test_*.py'
```

## Opt-in live evaluation

The runner supports built-in OpenAI-compatible providers that emit the existing
`--model-usage-jsonl` telemetry. First, provide the provider credentials in the
current shell and accept a connection test for the exact provider/model through
`krypper setup`. For Cloudflare:

```sh
source scripts/cloudflare-workers-ai-env.sh
krypper setup
```

After setup records a fresh successful probe for the chosen model, run at least
three repetitions. The model ID below is the manual arm and the scope anchor for
automatic routing:

```sh
python3 scripts/model_routing_evaluation.py \
  --live \
  --provider cloudflare \
  --model '<cloudflare-model-id>' \
  --repeats 3 \
  --action-budget 10 \
  --output /tmp/krypper-routing-evaluation.json
```

Use `--provider cloudflare-ai-gateway` to evaluate Gateway routing. Other
supported compatible providers use the same provider selector and environment
credential variables documented in the [CLI reference](CLI_REFERENCE.md).
Live evaluation sends each fixture goal and retrieved repository context to
the selected provider and may incur charges. The runner reports its selected
manual and automatic routes before beginning. If automatic routing has no
fresh successful probe evidence, it stops before a provider task request.

Each arm uses a separate temporary XDG configuration directory. The runner
copies only the local probe evidence, its private validation key, and the
public model catalog cache when present. It never copies `credentials.json` or
provider profiles; provider secrets must come from environment variables. The
runner removes `KRYPPER_CONFIG` from child environments so user config overrides
cannot redirect the evaluation. Temporary settings and workspaces are removed
when the command exits. The JSON report includes no prompts, model responses,
credential values, or environment values.

Each task uses its own disposable workspace and outside-workspace canary. A run
counts as successful only when the task exits successfully, reports a succeeded
outcome, changes exactly the expected files, and passes an independent offline
test. The report also records route selection and reason, model decision count,
provider token counts, model decision latency, end-to-end latency, safety observations,
fixture/source/binary digests, and paired outcomes. The manual route is read
from that arm's isolated settings; the automatic route is checked against the
route line emitted by each task invocation.

## Reading cost and readiness fields

- `token_usage_cost_estimate_micro_usd` applies catalog input/output rates to
  the token counts actually reported for the run. It is an estimate from usage,
  not a provider invoice.
- `router_reserved_estimate_micro_usd` applies those rates to the catalog's
  context-window and output limits for each model decision. Fallback is
  disabled in both arms, so each decision uses one route; this is the
  conservative per-request estimate used by route budgeting, not observed
  consumption.
- `actual_billed_cost_usd` is `null` because model usage telemetry does not
  report account charges. Missing or stale catalog prices remain unknown.
- `ready_for_issue_acceptance` means the repeated live sample, token telemetry,
  route trace, and independent outcome records are complete. It does not mean
  either arm passed every task or won.
- `matched_model_comparison_ready` additionally requires automatic routing to
  select the same provider/model as the manual arm in every run. If routes
  differ, paired results describe the end-to-end policies but cannot isolate a
  routing effect from a model change.

Report success rates with their Wilson 95% intervals and interpret them with
the small fixture count, provider/model, account, catalog freshness, and
action budget in view. A completed report is evidence for these tasks only; it
does not establish broad intelligence, safety, or quality superiority.
