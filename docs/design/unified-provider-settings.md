# Unified Provider, Model, Adapter & Router Configuration — Implementation Plan

Status: design proposal (no runtime behavior changes in this PR)

## Product contract

Krypper ships the supported providers, protocols, adapters and routing strategies. Users can independently choose among these supported integrations, register their own provider credentials, and select a supported model through either a future TUI or the central configuration file. Registration is a user-level credential/configuration action, **not** a request to install arbitrary adapter code. Development can add or revise integrations in subsequent releases.

Do not conflate the informational models.dev snapshot with an allowlist of usable models. The catalog can provide descriptions and suggestions, while actual usability is governed by the selected provider's supported protocol and validation. Preserve existing user-selected model IDs where supported by the adapter; do not silently reject unknown catalog entries.

## Current code / reuse

- `src/catalog_data.rs`: offline models.dev metadata, descriptive only.
- `src/brain/providers.rs`: `ModelProvider`, `OpenAiCompatibleProvider`, `DecisionProtocol`, `ProviderCapabilities`, and `ModelDecisionSource` already implement provider/protocol dispatch.
- `src/cli.rs`: `--provider` / `--model` parsing and construction of concrete decision sources; 11 provider identifiers currently accepted.
- `src/cli/config.rs`: model/provider defaults from user configuration.
- `src/harness/execution.rs`: existing decision-source abstraction and task safety boundary.

**Do not create a second execution engine or duplicate provider adapters.** Refactor current provider construction into one reusable resolver/factory used by CLI and future TUI.

## Proposed work packages (implementation PRs)

### A. Provider descriptor and adapter registry
- Provide one authoritative enumeration of *shipped* provider IDs, display names, protocol, credential requirements, supported capability flags and adapter factory mapping.
- Keep compatibility with existing `ModelProvider` / `ModelDecisionSource`; avoid a plugin loader or arbitrary executable adapters.
- Test registry-to-factory parity and reject unshipped provider identifiers with actionable errors.

### B. Versioned central configuration
- Extend the existing configuration parser, preserving currently supported `model` / `provider` settings and CLI precedence.
- Define versioned schema for named provider registrations, selected model, adapter defaults and an optional routing profile.
- A registration references a supported provider ID and a credential source; it does not add a new provider implementation.
- Validate unknown fields/IDs, incompatible settings, duplicate registrations and missing required credentials before applying.
- Use safe atomic writes, restrictive permissions, and deterministic precedence: CLI override > selected profile > defaults (document exact migration).
- Keep secrets out of committed config and logs; use environment variable references or OS secret storage where available. Never echo secret values in diagnostics.

### C. Router strategy registry
- Preserve current explicit provider selection as the default behavior.
- Introduce strategy identifiers only for implemented strategies; begin with explicit/manual selection.
- Add priority/fallback or health-aware strategies **only** alongside tested implementations; avoid advertising nonfunctional options.
- Maintain execution budgets, existing tool permissions and harness safety checks on all paths. No hidden provider switching.

### D. TUI integration contract
- Define a settings service/API used by both TUI and config-file editing, with read, validate, list supported providers/models, register credential reference, select profile, and save operations.
- The TUI is a presentation layer, not a second state store.
- Display only shipped capabilities; allow selection of models supported by the adapter, with catalog metadata as hints.
- TUI delivery can be a separate PR; the service must be usable headlessly first.

## Example (illustrative only; NOT an implemented schema)

```toml
version = 1
active_profile = "work"

[registrations.work-openai]
provider = "openai"
credential_env = "OPENAI_API_KEY"

[profiles.work]
registration = "work-openai"
model = "example-model"
router = "explicit"
```

## Compatibility and acceptance tests

1. Existing CLI provider/model flags and existing config remain functional without migration.
2. All currently shipped providers resolve to the same protocol/endpoint/auth behavior as before.
3. Unknown provider/adapter/router IDs fail validation; known provider with user API credential succeeds without source changes.
4. A catalog miss does not automatically make an otherwise valid model ID unusable.
5. No secret values appear in logs, errors, snapshots, or serialized config.
6. Both future TUI and direct config editing produce identical validated runtime configuration.
7. Existing harness budgets, permissions, error handling and recovery behavior remain intact.
8. Add unit tests for schema migrations, precedence, registration, adapter dispatch and invalid combinations; run formatting, tests and CI.

## Out of scope for this design PR

- No implementation of a TUI or runtime registry in this PR.
- No new provider integration, auto-selection algorithm, secret-storage backend, or arbitrary third-party adapter loading.
- No change to current default routing behavior.

## Recommended implementation order

1. Registry descriptors and factory extraction (A).
2. Shared configuration schema, validation, credential references and migration (B).
3. Explicit router registry, then optional new strategies (C).
4. Shared settings service and TUI screen (D).

Each implementation PR should include passing exact-head CI and a focused migration/compatibility review before merge.
