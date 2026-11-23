# Offline model catalog

Krypper uses [models.dev](https://models.dev) as its central, cross-provider model metadata source. The vendored snapshot in `snapshots/models-dev/api.json` keeps only fields used by `ModelSpec`, with its MIT license and provenance in `snapshots/models-dev/metadata.json`. The `sha256` field records the original upstream bytes; `snapshot_sha256` and `generator_version` identify the slim artifact. `src/catalog_data.rs` exposes `models()` and `lookup(provider, id)` using `include_str!`; no network request is made at runtime.

The central catalog distinguishes Workers AI (`cloudflare-workers-ai`) from Cloudflare AI Gateway (`cloudflare-ai-gateway`). `krypper models --provider cloudflare` selects Workers AI records. `krypper models --provider cloudflare-ai-gateway` selects AI Gateway records and Workers AI records, because the unified Gateway API can route both. The output retains the upstream provider ID. Keep Workers AI model IDs in their full `@cf/...` form; third-party models keep their `author/model` IDs. Pass either unchanged with the matching `--provider` to `krypper task`. AI Gateway calls send `cf-aig-gateway-id` and use `CLOUDFLARE_AI_GATEWAY_ID` or the default gateway ID `default`.

This central snapshot is advisory metadata and may lag Cloudflare's live catalog. Check Cloudflare's [Workers AI model catalog](https://developers.cloudflare.com/workers-ai/models/) for current models and lifecycle details. The account-specific [Model Search API](https://developers.cloudflare.com/api/resources/ai/subresources/models/methods/list/) requires a Cloudflare account and an API token with Workers AI access; it is not used by the offline CLI. A catalog entry does not guarantee that a particular account can run that model.

Model specs are **advisory metadata**, not an allowlist. An unknown model is not rejected by catalog lookup. Existing provider selection, user configuration and model overrides remain authoritative. A model appearing here does not imply that Krypper has credentials, an adapter or permission to call it.

To regenerate the optional normalized JSON artifact from the slim snapshot:

    cargo run --locked --example generate_model_catalog
    cargo run --locked --example generate_model_catalog -- --check
    cargo test

To refresh the snapshot, download the original upstream JSON as an explicit developer action, then pass that file to the generator:

    curl -fsS https://models.dev/api.json -o /tmp/models-dev-api.json
    cargo run --locked --example generate_model_catalog -- --from-upstream /tmp/models-dev-api.json
    cargo run --locked --example generate_model_catalog -- --check

The --from-upstream mode emits the slim snapshot and updates metadata.json with the SHA-256 of the supplied upstream bytes, the generated snapshot SHA-256, generator version, and provider/model counts. Provider count covers providers represented by models in the slim artifact. It does not fetch data itself. Update retrieved_at, preserve the upstream license notice, and review the generated diff. Increment the generator version when the record schema or field mapping changes. The --check mode verifies the artifact hash, generator version, counts, and parity with the Rust runtime reader. The original upstream JSON is not checked in; the upstream hash is retained as provenance and can be rechecked when the source file is available. The normalized src/catalog_data.json artifact remains optional and is not committed.

Do not add implicit network syncing to normal Krypper execution.

The catalog is not a provider allowlist. The CLI and provider router continue to accept caller-supplied model IDs that are absent from this snapshot.
