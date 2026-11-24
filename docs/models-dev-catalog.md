# Offline model catalog

Krypper uses [models.dev](https://models.dev) as its central, cross-provider model metadata source. The vendored snapshot in `snapshots/models-dev/api.json` keeps only fields used by `ModelSpec`, with its MIT license and provenance in `snapshots/models-dev/metadata.json`. `sha256` records the raw source bytes used for the last generation (currently the original API response); `trusted_input_sha256` identifies the pinned raw input in `upstream-api.json.gz`; `snapshot_sha256` and `generator_version` identify the slim artifact. The checked-in fixture is rendered from immutable `upstream_revision`; CI verifies its raw hash and regenerates the runtime snapshot offline. `src/catalog_data.rs` exposes `models()` and `lookup(provider, id)` using `include_str!`; no network request is made at runtime.

The central catalog distinguishes Workers AI (`cloudflare-workers-ai`) from Cloudflare AI Gateway (`cloudflare-ai-gateway`). `krypper models --provider cloudflare` selects Workers AI records. `krypper models --provider cloudflare-ai-gateway` selects AI Gateway records and Workers AI records, because the unified Gateway API can route both. The output retains the upstream provider ID. Keep Workers AI model IDs in their full `@cf/...` form; third-party models keep their `author/model` IDs. Pass either unchanged with the matching `--provider` to `krypper task`. AI Gateway calls send `cf-aig-gateway-id` and use `CLOUDFLARE_AI_GATEWAY_ID` or the default gateway ID `default`.

This central snapshot is advisory metadata and may lag Cloudflare's live catalog. Check Cloudflare's [Workers AI model catalog](https://developers.cloudflare.com/workers-ai/models/) for current models and lifecycle details. The account-specific [Model Search API](https://developers.cloudflare.com/api/resources/ai/subresources/models/methods/list/) requires a Cloudflare account and an API token with Workers AI access; it is not used by the offline CLI. A catalog entry does not guarantee that a particular account can run that model.

Model specs are **advisory metadata**, not an allowlist. An unknown model is not rejected by catalog lookup. Existing provider selection, user configuration and model overrides remain authoritative. A model appearing here does not imply that Krypper has credentials, an adapter or permission to call it.

To validate or rewrite the normalized artifact used by the runtime:

    cargo run --locked --example generate_model_catalog
    cargo run --locked --example generate_model_catalog -- --check

To refresh the snapshot, select and review an immutable upstream revision, prepare its API JSON, and pass the raw file to the generator:

    # After preparing /tmp/models-dev-api.json from the selected upstream revision:
    gzip -n -c /tmp/models-dev-api.json > snapshots/models-dev/upstream-api.json.gz
    cargo run --locked --example generate_model_catalog -- --from-upstream /tmp/models-dev-api.json
    cargo run --locked --example generate_model_catalog -- --check

The `--from-upstream` mode emits the runtime snapshot and updates `metadata.json` with the SHA-256 of the supplied upstream bytes, generated snapshot hash, generator version, and provider/model counts. Provider count covers providers represented by models in the slim artifact. It does not fetch data itself. For an upstream refresh, replace the compressed fixture, generate the snapshot, set `upstream_revision` to the selected commit, update `retrieved_at`, preserve the upstream license notice, and review the generated diff. When input bytes change, generation clears the old revision pin so a stale revision cannot be retained accidentally. Increment the generator version when normalization rules or the record schema change. `--check-upstream <path>` verifies the pinned raw input hash, regenerates the slim snapshot, validates metadata consistency, and checks parity with the Rust runtime reader. `--check` verifies the embedded artifact hash, generator version, counts, and runtime parity. Writes use staged temporary files, synced contents, rename replacement, and post-write consistency checks. Display strings have terminal control characters removed; identifiers containing them are rejected.

Do not add implicit network syncing to normal Krypper execution.

The catalog is not a provider allowlist. The CLI and provider router continue to accept caller-supplied model IDs that are absent from this snapshot.

## Explicit source adapters and cache

The Rust library exposes a catalog_sources module for callers that explicitly
refresh or import catalog data. ModelsDevSource normalizes the cross-provider
source; CloudflareWorkersAiSource adds Cloudflare-specific task, capability,
and deprecation metadata while preserving full @cf/... model IDs. Optional
prices are represented as USD per million input or output tokens. Missing
prices and features remain unknown.

Each contribution records its fixed source URL, adapter schema version, raw
SHA-256, retrieval time, license, and license URL. Models.dev records retain
the upstream MIT attribution. Cloudflare Workers AI records retain the
Cloudflare docs CC BY 4.0 attribution. Cloudflare's feed is not a versioned API;
unknown feed fields are ignored, while a changed required shape fails the
refresh and leaves the last-known-good cache in place. Provider metadata is
descriptive only: it cannot supply request endpoints, credentials, or routing
authority.

The explicit refresh_catalog and import_catalog_payload APIs validate source
size, HTTPS-only fixed URLs, bounded request time, record identity, schema
version, and checksums before replacing a cache file atomically. A failed
refresh returns the previous verified snapshot, including when it is stale;
callers can inspect the typed failure and age. Invalid imports do not replace
a valid cache. read_catalog_cache rejects noncanonical, corrupt, or unsupported
cache files. The cache is advisory data and is not consulted by
catalog_data::models(), task setup, or ordinary CLI execution. P9 owns the
future CLI presentation of explicit refresh and cache status.

The HTTP transport sends unauthenticated GET requests only to the adapters'
fixed HTTPS URLs, follows no redirects, and has no API-key or custom-header
surface. Tests inject a local transport fixture; routine tests do not contact
either upstream service.
