# Extension bridge safety

PR 3 exposes host-registered extension tools through the existing Brain schema
adapters and Harness dispatch. The `krypper::extensions` module provides the
host-facing declaration and turn APIs. A provider supplies only a registered
tool name and a JSON argument object. The provider cannot install a handler,
mint a grant, or choose an arbitrary executor.

A reusable `CapabilityGrant` is only a prerequisite. A registered effect runs
only through the prompt-aware dispatch/turn APIs, which ask the host to approve
an immutable request bound to the current task, turn, operation, schema,
arguments, workspace identity and managed revision. The host must provide a
current capability-context callback; the grant is rechecked against its fresh
generation after approval.

The approved extension APIs always request exact approval for every effect.
They do not accept a permission mode or let a reusable grant skip that prompt.
This matches the current Ask and Risk-based policy, where external or
unclassified effects require approval; only the fixed offline process profile
is automatically available in Risk-based mode. The Full Access host-process
mode does not authorize extensions or bypass their exact approval.

## Threat model

- Only host-validated descriptors may be advertised. Names that collide with a
  built-in action or another extension are rejected before advertisement.
- Capability grants, turn ownership and JSON schemas are checked before the
  prompt and again at the effect boundary. The one-shot approval expires 30
  seconds after the prompt request is created and binds the canonical request
  digest. Denials, unavailable
  authority and callback errors fail closed.
- The final workspace identity/revision comparison and synchronous handler call
  hold TaskSession's existing read lease, so managed Krypper workspace
  mutations cannot interleave with that effect. Direct writes by trusted host
  handlers or unrelated processes are outside this managed revision fence.
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
  decision-source and dispatch APIs; the approved dispatch and turn APIs are
  additive and require no changes to built-in provider configuration. Existing
  direct dispatch APIs without a prompt remain source-compatible and fail closed
  with `ApprovalRequired`; existing turn wrappers without the exact prompt gate
  return `PermissionPromptUnavailable` for registered extension actions.
- With an empty host registry, existing built-in turn flows and observations are
  unchanged.
- Unsupported provider tool calls that carry JSON arguments now map to the new
  extension action instead of a generic error string, so a handler can receive
  them only when the host has explicitly registered and granted that name and
  the host approves the exact effect for this turn.

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
- stdout is bounded to 64 KiB per JSON-RPC line. A dedicated reader drains stderr
  throughout the child lifetime, retains at most 4 KiB per session, and discards
  excess bytes without failing an MCP request. The host timeout starts before
  spawn and bounds the spawn result plus later read, write and wait; after an
  aborted spawn, cleanup confirmation may add a bounded grace. Cancellation is
  checked on each loop iteration and inside the spawn worker before launch.
  Timeout or cancellation
  waits for the worker, terminates any child it created and confirms cleanup
  before returning `Timeout` or `Cancelled`; an unconfirmed cleanup returns
  `CleanupFailed`. A `Command::spawn()` call that never returns remains a bounded
  residual.
- The server runs in its own process group. Cleanup, including a failed
  handshake, checks the group signal result, kills the whole group, reaps the
  direct child and polls until the group is empty within the bounded grace;
  otherwise it reports `CleanupFailed`. Descendants that deliberately leave the
  group are outside this guarantee.
- The child starts with a cleared, minimal environment, but the transport does
  not create a filesystem or network sandbox. The configured server runs with
  the Krypper process's OS-user permissions, subject to external OS policy.
  Process grouping limits cleanup scope; it does not reduce the server's
  authority. Windows currently returns `PlatformUnavailable` before spawning.
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
  invocation are separate opt-in host calls with their own grants and schemas.

## MCP tool discovery and invocation (PR 5)

- A connected session stays inert. Discovery requires its own host-issued,
  task-scoped `mcp/discover_tools` grant with `ExecuteProcess`; invocation
  requires a current per-tool grant with `ReadWorkspace` or `MutateWorkspace`.
  A connection grant never authorizes either operation. Missing, wrong-task,
  stale-generation, wrong-operation, wrong-effect and unauthorized-task grants
  deny before any request is written.
- The host supplies a validated scope. Advertised names are deterministic
  `mcp_<scope>_<name>` identifiers; invalid identifiers, duplicate qualified
  names and collisions with built-in registry names are rejected before a tool
  can be advertised. Only tools with a matching host grant are returned, so an
  ungranted tool has no callable name.
- Server metadata is untrusted data. Names must be short ASCII identifiers;
  schemas must pass the existing provider-bridge JSON Schema subset; descriptions
  are bounded to 512 bytes, have control characters removed, and are prefixed as
  untrusted data that must not be followed as instructions.
- A discovered grant's `ReadWorkspace` or `MutateWorkspace` effect describes
  which host-side invocation policy is checked. It does not constrain the
  separate MCP server process or sandbox its implementation.
- Invocation arguments are revalidated against the discovered schema through
  the existing extension authorization gate before any `tools/call` request.
  The wire request uses only the remote tool name, never the provider-facing
  qualified name. The resulting `content` must be bounded text blocks (at most
  32 blocks and 16 KiB); control characters are removed and output is prefixed
  as untrusted data. A valid `isError: true` result is returned with an explicit
  tool-execution-error marker so the model can adjust its call; it does not
  invalidate the session. Non-text content and malformed results fail closed.
- Both operations reuse the same bounded stdio transport with correlated ids,
  a 16-notification cap, a 64-page discovery cap, a 64-tool aggregate cap,
  a 4 KiB cursor cap, repeated-cursor rejection, the existing 64 KiB response
  bound and the host timeout and cancellation signal. Malformed JSON-RPC,
  mismatched ids, timeouts, cancellation and disconnects fail closed without
  retries or fallback execution. No second executor, permission model, CLI
  default, dependency or network transport is introduced.

### Backward compatibility

- Discovery and invocation add crate-internal MCP transport APIs only. Existing
  connect behavior, the built-in registry, provider schemas, CLI defaults and
  all other tool effects are unchanged when the host makes no discovery or
  invocation call.

## Skills manifest loading (PR 6)

- `tools::skills` reads explicit JSON manifests from `.krypper/skills/*.json` and
  host-supplied workspace-relative paths. It is crate-private and read-only, with
  no CLI default and no tool registration.
- Bounds: 64 manifests per load, 16 KiB per manifest, 8 KiB of instructions,
  512 B of description, 4 KiB per path and 16 path components. Names are bounded
  ASCII identifiers. Duplicate names, file-name/content mismatches and unknown
  JSON fields fail closed. Results are sorted by name, then manifest path.
- Discovery and reads use the existing workspace containment walk. Absolute
  paths, parent components, symlinks, escapes, oversize inputs, invalid UTF-8 and
  schema-invalid manifests fail closed. A syntactically malformed JSON file is
  skipped individually and reported to the host by relative path; file contents
  are not included in the diagnostic.
- Instructions are inert, untrusted text. Manifests cannot declare tools,
  commands, grants or policy; loading never activates a skill, injects context,
  downloads content or changes authority. Bounded selection and injection are a
  separate later subtask.
- Backward compatibility: additive crate-private module with no new dependency,
  grant, registry entry, provider schema or CLI change.

## Skills activation and context (PR 7)

- Activation is host-owned. `tools::skills::activate_skills` loads the PR 6
  manifests and activates only the skill names the host supplies, in the host's
  order. Task text, model output, repository excerpts and manifest content never
  select or add a skill, so activation cannot expand authority.
- Bounds: at most 4 active skills per request and at most 2 KiB of combined
  description and instruction text, divided evenly across selected skills.
  Descriptions use at most min(512 bytes, one quarter of each skill's share),
  with the remaining three quarters reserved for instructions. Unused description
  allowance is not redistributed. Truncation
  happens on a character boundary, is marked in the text and is reported as
  `truncated`. An unknown name or an over-limit request fails closed instead of
  activating a partial set; an empty request activates nothing.
- Trailing whitespace in description and instruction lines is trimmed, control
  characters are replaced and directive-like lines neutralized (`neutralized_lines`). Substring checks
  on those lines are text-sanitization defense in depth only; activation uses
  exact host-supplied skill-name matches. Malformed-JSON diagnostics are host
  metadata and are omitted from serialized model context. The injected block is
  labeled as untrusted advisory text, not a command, tool grant or policy.
- Precedence is explicit and tested: the task goal and Harness bounds stay
  highest, then root/AGENTS.md project guidance, then activated skills, then
  bounded repository excerpts. Injection reuses the existing plan and task
  prompt builders; there is no new agent loop, verification path or executor.
- Compatibility: `cargo`/CLI defaults, the registry, provider schemas and
  existing prompts are unchanged. When no skill context is supplied (or the
  context is empty) prompts are byte-identical to the current builders. No new
  dependency, grant or tool is introduced.
