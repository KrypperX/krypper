# Offline model catalog

Krypper uses [models.dev](https://models.dev) as its central, cross-provider model metadata source. The vendored snapshot is in `snapshots/models-dev/api.json`, with its MIT license and a SHA-256/date record in `metadata.json`. `src/catalog_data.rs` exposes `models()` and `lookup(provider, id)` using `include_str!`; no network request is made at runtime.

The upstream provider ID for Cloudflare Workers AI is `cloudflare-workers-ai`, while Krypper's supported provider selector is `cloudflare`. Therefore `krypper models --provider cloudflare` and `catalog_data::lookup("cloudflare", id)` select those records and retain the upstream ID in the returned metadata. Cloudflare Workers AI model IDs keep their full `@cf/...` form, so they can be passed unchanged to `krypper task --provider cloudflare --model MODEL_ID`. Cloudflare AI Gateway records are a separate upstream provider and are not selected by this alias.

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
