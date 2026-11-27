# Provider routing migration guide

This guide covers the staged provider and model routing changes through P10.
Existing direct CLI usage and per-user model settings remain compatible.

## Keep manual routing

Explicit task flags remain authoritative and need no migration:

```sh
krypper task --goal "Review this change" --max-actions 8 \
  --provider openai --model '<model-id>'
```

Existing `config.toml` files with `provider` and `model` continue to use manual
routing when `routing_mode` is absent. Saved credentials remain in the same
per-user credential store, now encrypted with a key held in the system
credential vault. Environment precedence and provider selectors are unchanged.
Existing plaintext credential files require explicit approval in `krypper
setup` before migration.

## Enable automatic routing

Automatic routing is per-user and opt-in. First complete the interactive setup
and accept a successful connection test for each model you want eligible. Then
enable automatic selection:

```sh
krypper setup --routing-mode automatic
krypper setup --status --json
```

Automatic routing considers only configured models with fresh successful probe
evidence and required catalog capabilities. The saved provider is the default
scope anchor. Cross-provider task-data transfer stays disabled unless you
explicitly add `--allow-cross-provider`; fallback stays disabled unless you
also set a finite `--fallback-budget-usd` ceiling.

Return to explicit manual routing at any time:

```sh
krypper setup --routing-mode manual
```

The change updates only routing preferences. It does not remove credentials,
catalog data, probe evidence, or task history.

## Model catalog JSON

Plain `krypper models --json` keeps its model-array contract. Scripts that need
catalog provenance, model source IDs, pricing, or declared feature metadata
should opt into the envelope:

```sh
krypper models --json --provenance
```

Prices and feature fields are optional. Missing or stale prices must remain
unknown; a token-based calculation is an estimate and is not an invoice amount.
See the [routing evaluation guide](model-routing-evaluation.md) for the
distinction between usage-based price estimates, router-reserved estimates, and
actual billed cost.

## Rollback

Set routing mode to `manual`, or remove the optional `routing_mode`,
`allow_cross_provider_data_transfer`, and `fallback_budget_micro_usd` lines from
`config.toml`. The original `provider` and `model` values remain usable. Do not
delete the credential store or evidence files unless you intend to remove those
settings as well.
