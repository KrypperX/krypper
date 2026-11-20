# Offline models.dev catalog

The vendored upstream snapshot is in `snapshots/models-dev/api.json`, with its MIT license and a SHA-256/date record in `metadata.json`. `src/catalog_data.rs` exposes `models()` and `lookup(provider, id)` using `include_str!`; no network request is made at runtime.

Model specs are **advisory metadata**, not an allowlist. An unknown model is not rejected by catalog lookup. Existing provider selection, user configuration and model overrides remain authoritative. A model appearing here does not imply that Krypper has credentials, an adapter or permission to call it.

To regenerate the optional normalized JSON artifact from the pinned snapshot:

```sh
cargo run --locked --example generate_model_catalog
cargo run --locked --example generate_model_catalog -- --check
cargo test
```

The Rust generator checks the original bytes against the recorded SHA-256 and provider count, sorts providers and model IDs, and maps `limit.context`, `limit.output`, `tool_call`, `reasoning`, `modalities`, `name`, and `status`. Missing fields remain null rather than guessed. The `--check` mode compares generated records with the authoritative Rust runtime reader; the normalized `src/catalog_data.json` artifact is optional and is not committed.

Refreshing the snapshot is an explicit developer action. Replace `api.json`, update `metadata.json` with its new SHA-256 and provider count, preserve the upstream license notice, regenerate and review the diff. Do not add implicit network syncing to normal Krypper execution.

The CLI and provider router are not changed by this metadata module. A future CLI listing feature should reuse `catalog_data::models()` and leave existing user-configured models working.
