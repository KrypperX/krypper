# Context model evaluation

`scripts/context_model_evaluation.py` compares the existing bounded search with the opt-in Rust module map on the same four small, held-out fixture tasks. It needs Python 3.10 or newer and only uses the standard library. Both arms use the same task goal, model, and action limit; each receives retrieved context under the same 4 KiB maximum context budget. The exact provider token counts are measured per run because the two contexts can use different numbers of tokens. Arm order is randomized from a recorded seed for each repeat.

## Offline preflight

Build Krypper and inspect retrieval without contacting a model provider:

```sh
cargo build --locked --bin krypper
python3 scripts/context_model_evaluation.py
```

This first verifies in disposable workspaces that each fixture shows its declared baseline failure: two fail the intended Rust tests, and two have the expected compile-time field mismatch. It then checks both retrieval arms and prints a JSON report with baseline outcomes, context size, gold-path recall, skipped stale or oversized results, and fixture and binary digests. It makes no Cloudflare request. Live mode performs the same baseline check before sending any model request.

## Cloudflare live run

From the repository root, load credentials into the current Bash session. The helper prompts for the account ID and hides token input; it does not save either value:

```sh
source scripts/cloudflare-workers-ai-env.sh
```

Then run at least three repeats with the same Cloudflare model for both arms. The default provider is direct Workers AI (`cloudflare`):

```sh
python3 scripts/context_model_evaluation.py \
  --live \
  --model '<cloudflare-model-id>' \
  --repeats 3 \
  --action-budget 10 \
  --output /tmp/krypper-context-evaluation.json
```

Choose a text-generation model with function-calling support from Cloudflare's [Workers AI model catalog](https://developers.cloudflare.com/workers-ai/models/); tool support depends on the selected model. To route the same evaluation through AI Gateway, add `--provider cloudflare-ai-gateway`; use a third-party `author/model` ID or a Workers AI `@cf/...` ID from `krypper models --provider cloudflare-ai-gateway`.

Live mode sends each fixture task and its retrieved repository context to the selected Cloudflare endpoint. The context is passed as a separate untrusted advisory block with `--repository-context-json`, keeping the task goal unchanged so the task's normal bounded search receives the same query in both arms. With four tasks, two arms, three repeats, and a 10-action budget, it can make up to 240 model requests and incur provider charges; runs that finish early make fewer requests. The runner uses disposable workspaces, passes `--no-config`, and does not include credentials, raw prompts, or model responses in the report. Afterward, clear the variables from the current shell:

```sh
unset CLOUDFLARE_API_TOKEN CLOUDFLARE_ACCOUNT_ID CLOUDFLARE_AI_GATEWAY_ID
```

The Cloudflare API token used by these REST endpoints needs the Account `Workers AI - Read` permission. Gateway management permissions are for configuring gateways and reading their logs; the REST inference call uses the Workers AI permission. See Cloudflare's [Workers AI REST API guide](https://developers.cloudflare.com/workers-ai/get-started/rest-api/) and [AI Gateway REST API guide](https://developers.cloudflare.com/ai-gateway/usage/rest-api/) for account setup, gateway selection, supported models, and model ID formats.

## Report interpretation

The report records per-run task outcome, expected and unexpected changed paths, independent `cargo test --offline` outcome, retrieval recall and size, token counts, latency, skipped stale retrieval, denied operations, unsafe path attempts, duplicate file effects, and an outside-workspace canary. Current retrieval reads the workspace directly rather than using a persistent index, so `stale_skipped` is expected to be zero; this suite records stale skips but does not simulate an outdated index. Arm summaries include task success rate with a Wilson 95% interval, mean gold-path recall, token totals, and latency percentiles.

`ready_for_issue_acceptance` means the configured live sample is complete: four fixture tasks, both arms, at least three repeats, and token counts reported for every provider decision. It signals that the evidence is ready for review; it does not mean either arm passed or that the module map is better. Reports include the pinned base revision, current source revision, a digest of the tracked diff from that base, executable, runner, and fixture digests so a run can be tied to the tested artifacts. The fixtures are deliberately small, so results should be treated as initial evidence rather than a broad claim about arbitrary repositories.
