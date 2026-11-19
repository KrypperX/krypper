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

**Do not create a second execution engine or duplicate provider adapters.** Refactor current provider construction into one reusable resolver/factory used by the existing CLI/config paths; future interfaces may reuse it without a UI dependency.

## Scope boundary

The product's complete future feature set is **not yet decided**. Implement only the shared integration foundations needed by existing CLI/config/provider paths. Avoid overengineering for imagined TUI workflows, speculative routing strategies, or unrequested features. Future TUI design and feature prioritization require a separate product decision.

## Proposed work packages (implementation PRs)

### A. Provider descriptor and adapter registry
- Provide one authoritative enumeration of *shipped* provider IDs, display names, protocol, credential requirements, supported capability flags and adapter factory mapping.
- Keep compatibility with existing `ModelProvider` / `ModelDecisionSource`; avoid a plugin loader or arbitrary executable adapters.
- Test registry-to-factory parity and reject unshipped provider identifiers with actionable errors.

### B. Versioned central configuration
- Extend the existing configuration parser, preserving currently supported `model` / `provider` settings and CLI precedence.
- Define the smallest compatible schema needed for named provider registrations, selected model and explicit provider selection. Defer adapter defaults and routing profiles until concrete use cases require them.
- A registration references a supported provider ID and a credential source; it does not add a new provider implementation.
- Validate unknown fields/IDs, incompatible settings, duplicate registrations and missing required credentials before applying.
- If programmatic config writes are introduced, use atomic writes and restrictive permissions. Preserve current CLI-over-config precedence; add profiles or migration only if required by the implemented scope.
- Keep secrets out of committed config and logs; use environment variable references or OS secret storage where available. Never echo secret values in diagnostics.

### C. Router strategy registry
- Preserve current explicit provider selection as the default behavior.
- Introduce strategy identifiers only for implemented strategies; begin with explicit/manual selection.
- Defer priority/fallback, health-aware, cost-aware and automatic routing entirely until separately approved and implemented.
- Maintain execution budgets, existing tool permissions and harness safety checks on all paths. No hidden provider switching.

### D. UI-neutral integration boundary (no TUI development)
- Keep configuration read/validate/resolve logic independent of CLI presentation so future interfaces can reuse it.
- Expose only the minimal typed settings and registry query interfaces required by the current CLI/config use cases; avoid speculative UI APIs, settings screens, navigation or feature inventories.
- Treat the config file as the single persisted source of truth; future TUI design is deliberately undecided.
- Do not implement a TUI, TUI-specific state, UI framework, or unused UI abstractions.

## Example (illustrative only; NOT an implemented schema)

```toml
version = 1
provider = "openai"
model = "example-model"
credential_env = "OPENAI_API_KEY"
```

## Compatibility and acceptance tests

1. Existing CLI provider/model flags and existing config remain functional without migration.
2. All currently shipped providers resolve to the same protocol/endpoint/auth behavior as before.
3. Unknown provider/adapter/router IDs fail validation; known provider with user API credential succeeds without source changes.
4. A catalog miss does not automatically make an otherwise valid model ID unusable.
5. No secret values appear in logs, errors, snapshots, or serialized config.
6. The shared resolver produces the same validated runtime configuration regardless of its future caller; no TUI implementation is required now.
7. Existing harness budgets, permissions, error handling and recovery behavior remain intact.
8. Add unit tests for backward compatibility, precedence, registration, adapter dispatch and invalid combinations; run formatting, tests and CI.

## Out of scope for this design PR

- No implementation of a TUI or runtime registry in this design PR; no TUI implementation is included in the current development scope.
- No new provider integration, auto-selection algorithm, secret-storage backend, or arbitrary third-party adapter loading.
- No change to current default routing behavior.

## Recommended implementation order

1. Registry descriptors and factory extraction (A).
2. Shared configuration schema, validation, credential references and migration (B).
3. Reuse existing explicit routing through the shared resolver; do not add unused strategies (C).
4. Keep a minimal UI-neutral boundary that future interfaces can call (D); defer all TUI feature and screen planning.

Each implementation PR should include passing exact-head CI and a focused migration/compatibility review before merge.
