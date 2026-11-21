# Extension bridge safety

PR 3 exposes host-registered extension tools through the existing Brain schema
adapters and Harness dispatch. The `krypper::extensions` module provides the
host-facing declaration and turn APIs. A provider supplies only a registered
tool name and a JSON argument object. The provider cannot install a handler,
mint a grant, or choose an arbitrary executor.

## Threat model

- Only host-validated descriptors may be advertised. Names that collide with a
  built-in action or another extension are rejected before advertisement.
- Capability grants, turn ownership and JSON schemas are rechecked at dispatch,
  not only when a tool is advertised. Denials fail closed.
- Raw argument values are never persisted in observations or logs. Successful
  observations retain the tool name and the host handler's result; denials
  retain only the tool name, matching existing unsupported-tool behaviour.
- This bridge admits only `ReadWorkspace` and `MutateWorkspace` effects.
  `ExecuteProcess`, `Network` and `ExternalService` effects require the separate
  verified-supervisor/sandbox path and are rejected here.
- Extensions use the existing task lifecycle and capability boundary. No
  extension-owned execution loop or provider-issued authority is introduced.
  Executors are trusted host code selected by registration name, never by
  model-supplied code or paths; the host must implement each declared effect.
- A successful extension produces an ordinary observation and does not complete
  the turn by itself; the model still decides when to complete or continue.
- The CLI currently registers no extension handlers, so default CLI behaviour
  remains built-in-only. The production host registry is a separate concern.

## Backward compatibility

- Adding `WorkspaceAction::Extension` and
  `WorkspaceObservation::ExtensionCompleted` is an explicitly scoped public API
  migration. Downstream exhaustive matches must add a wildcard arm or the new
  variants.
- The `krypper::extensions` module adds host-facing declaration, request-builder,
  decision-source and dispatch APIs; these are additive and require no changes
  to built-in provider configuration.
- With an empty host registry, existing built-in turn flows and observations are
  unchanged.
- Unsupported provider tool calls that carry JSON arguments now map to the new
  extension action instead of a generic error string, so a handler can receive
  them only when the host has explicitly registered and granted that name.

## MCP stdio transport (PR 4)

- `tools::mcp_transport` is opt-in and host-owned. The host supplies an absolute
  executable, literal arguments and a bounded timeout; validation runs before
  any process is spawned. Model output cannot create or broaden this config.
- Connection performs only `initialize`, protocol-version negotiation and
  `notifications/initialized`. It never issues `tools/list` or `tools/call`,
  never creates capability grants, and never registers an executable handler.
- Only `2025-06-18` and `2024-11-05` are accepted. An unknown version, malformed
  JSON-RPC envelope, mismatched id or server error fails closed before the
  session is returned.
- stdout is bounded to 64 KiB per response, stderr is drained with a 4 KiB cap,
  and every read, write and wait obeys the host timeout. Cancellation is checked
  on each loop iteration and terminates the child on every failure path.
- Spawn failure, I/O failure, timeout, cancellation, disconnect, protocol
  violation, unsupported version, server error, output-limit breach and failed
  cleanup are mapped to distinct lifecycle outcomes. Cleanup is confirmed or
  reported as `CleanupFailed`; it is never silently claimed.
- The transport uses no shell, no network permission and no new dependency. It
  is a separate crate-private module with no CLI, provider or registry changes.

### Backward compatibility

- Adding `tools::mcp_transport` is additive. With no host call to
  `connect_stdio`, no child process is launched and all existing behaviour is
  unchanged.
- The transport is not re-exported through `krypper::extensions`; discovery and
  invocation remain later subtasks with their own grants and schemas.
