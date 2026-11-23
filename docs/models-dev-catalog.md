# Offline model catalog

Krypper uses [models.dev](https://models.dev) as its central, cross-provider model metadata source. The vendored snapshot is in `snapshots/models-dev/api.json`, with its MIT license and a SHA-256/date record in `metadata.json`. `src/catalog_data.rs` exposes `models()` and `lookup(provider, id)` using `include_str!`; no network request is made at runtime.

The central catalog distinguishes Workers AI (`cloudflare-workers-ai`) from Cloudflare AI Gateway (`cloudflare-ai-gateway`). `krypper models --provider cloudflare` selects Workers AI records. `krypper models --provider cloudflare-ai-gateway` selects AI Gateway records and Workers AI records, because the unified Gateway API can route both. The output retains the upstream provider ID. Keep Workers AI model IDs in their full `@cf/...` form; third-party models keep their `author/model` IDs. Pass either unchanged with the matching `--provider` to `krypper task`. AI Gateway calls send `cf-aig-gateway-id` and use `CLOUDFLARE_AI_GATEWAY_ID` or the default gateway ID `default`.

This central snapshot is advisory metadata and may lag Cloudflare's live catalog. Check Cloudflare's [Workers AI model catalog](https://developers.cloudflare.com/workers-ai/models/) for current models and lifecycle details. The account-specific [Model Search API](https://developers.cloudflare.com/api/resources/ai/subresources/models/methods/list/) requires a Cloudflare account and an API token with Workers AI access; it is not used by the offline CLI. A catalog entry does not guarantee that a particular account can run that model.

Model specs are **advisory metadata**, not an allowlist. An unknown model is not rejected by catalog lookup. Existing provider selection, user configuration and model overrides remain authoritative. A model appearing here does not imply that Krypper has credentials, an adapter or permission to call it.

To regenerate the optional normalized JSON artifact from the pinned snapshot:

```sh
cargo run --locked --example generate_model_catalog
cargo run --locked --example generate_model_catalog -- --check
cargo test
```

The Rust generator checks the original bytes against the recorded SHA-256 and provider count, sorts providers and model IDs, and maps `limit.context`, `limit.output`, `tool_call`, `reasoning`, `modalities`, `name`, and `status`. Missing fields remain null rather than guessed. The `--check` mode compares generated records with the authoritative Rust runtime reader; the normalized `src/catalog_data.json` artifact is optional and is not committed.

Refreshing the snapshot is an explicit developer action. Replace `api.json`, update `metadata.json` with its new SHA-256 and provider count, preserve the upstream license notice, regenerate and review the diff. Do not add implicit network syncing to normal Krypper execution.

The catalog is not a provider allowlist. The CLI and provider router continue to accept caller-supplied model IDs that are absent from this snapshot.
