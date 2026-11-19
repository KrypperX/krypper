# Reference study: OpenCode and OpenMausBot

Research date: 2026-11-19. This is a comparison of published upstream behavior, not an implementation claim about Krypper.

## OpenCode

Primary references:
- https://opencode.ai/docs/providers
- https://github.com/anomalyco/opencode/blob/dev/packages/web/src/content/docs/providers.mdx

OpenCode separates provider authentication (e.g. `/connect`) from model selection (`/models`). It uses models.dev as catalog input and provider-specific SDK/protocol packages for execution. Its config can describe provider IDs, credentials/environment variables, endpoint settings and models. It also supports user-defined custom provider packages, which is **not** in Krypper's agreed scope.

**Adopt:** clear separation of provider metadata, credentials, model selection and protocol dispatch. Use the catalog for hints, not proof of runtime support. Reuse the already implemented Rust adapters.

**Do not adopt now:** npm/SDK package loading, arbitrary user-provided adapters, TUI commands or dynamic plugin installation.

## OpenMausBot

Primary references:
- https://github.com/milind-soni/OpenMausBot/blob/main/apps/docs/content/docs/providers/index.mdx
- https://github.com/milind-soni/OpenMausBot/blob/main/apps/docs/content/docs/providers/model-providers.mdx
- https://github.com/milind-soni/OpenMausBot/blob/main/docs/custom-engines.md

OpenMausBot delegates many integrations to existing agent engines/CLIs. For example, its OpenCode integration reuses OpenCode authentication and retrieves model choices through an ACP session; engines can have their own model discovery. That delegation avoids reimplementing every upstream provider, but it adds an engine/ACP integration layer.

**Adopt:** distinguish configured integration availability from model metadata; keep discovery/selection behind a stable core interface if needed by current consumers.

**Do not adopt now:** ACP orchestration, multiple agent engines, model refresh UI, new engine instances or a second runtime. Krypper already has direct provider adapters.

## Krypper decision

Existing relevant files: `src/brain/providers.rs`, `src/cli.rs`, `src/cli/config.rs`, `src/catalog_data.rs`, `src/harness/execution.rs`.

Recommended minimal data flow:

```text
Existing CLI arguments + existing central config
                   |
         Validate / resolve selection
                   |
      Shipped provider definition
          /                  \
Credential reference     Model ID
          \                  /
         Existing adapter factory
                   |
          Existing explicit routing
                   |
       Existing harness / permissions
```

Implementation rules:
1. Preserve existing CLI and config semantics; no mandatory migration.
2. Treat provider support as shipped adapter/protocol support, not as membership in models.dev.
3. Allow users to supply credentials for supported providers without changing source code; keep secrets out of config/logs by using credential references.
4. Do not add arbitrary provider definitions or executable adapter packages through user config.
5. Refactor only duplicated provider dispatch that is actually present; keep a single source of truth for shipped IDs and capabilities.
6. Do not add TUI, ACP, fallback strategies or speculative extensibility abstractions in this milestone.
7. Verify existing provider endpoint/auth/protocol behavior and run unit/integration tests and CI for runtime changes.

This research is intended to guide subsequent implementation PRs; it does not imply runtime code has been changed.
