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
  JSON-RPC envelope, mismatched id, server error, malformed Initialize Result, or
  more than 16 notifications before the response fails closed.
- stdout is bounded to 64 KiB per response and stderr to 4 KiB per session across
  all handshake reads. The host timeout starts before spawn and bounds the spawn
  result plus later read, write and wait; after an aborted spawn, cleanup
  confirmation may add a bounded grace. Cancellation is checked on each loop
  iteration and inside the spawn worker before launch. Timeout or cancellation
  waits for the worker, terminates any child it created and confirms cleanup
  before returning `Timeout` or `Cancelled`; an unconfirmed cleanup returns
  `CleanupFailed`. A `Command::spawn()` call that never returns remains a bounded
  residual.
- The server runs in its own process group. Cleanup, including a failed
  handshake, checks the group signal result, kills the whole group, reaps the
  direct child and polls until the group is empty within the bounded grace;
  otherwise it reports `CleanupFailed`. Descendants that deliberately leave the
  group are outside this guarantee.
- Config validation is not execution authorization. The only exported connect
  entry point is `connect_stdio_authorized`; it requires a host-owned,
  task-scoped `mcp/connect_stdio` grant with `ExecuteProcess` before validation
  or spawn, and the raw handshake is module-private. The host must bind each
  grant to the exact executable and configuration it supplies. A connection
  grant never authorizes discovered MCP tools.
- The transport uses no shell, no network permission and no new dependency. It
  is a separate crate-private module with no CLI, provider or registry changes.

### Backward compatibility

- Adding `tools::mcp_transport` is additive. With no host call to
  `connect_stdio_authorized`, no child process is launched and all existing
  behaviour is unchanged. The raw handshake is crate-module-private, so callers
  cannot bypass the authorized entry point.
- The transport is not re-exported through `krypper::extensions`; discovery and
  invocation remain later subtasks with their own grants and schemas.
