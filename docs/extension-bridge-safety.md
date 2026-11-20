# Extension bridge safety

PR 3 exposes host-registered extension tools through the existing Brain schema
adapters and Harness dispatch. A provider supplies only a registered tool name
and a JSON argument object. The provider cannot install a handler, mint a grant,
or choose an arbitrary executor.

## Threat model

- Only host-validated descriptors may be advertised. Names that collide with a
  built-in action or another extension are rejected before advertisement.
- Capability grants, turn ownership and JSON schemas are rechecked at dispatch,
  not only when a tool is advertised. Denials fail closed.
- Raw argument values are never persisted in observations or logs. Observations
  retain only the tool name, matching existing unsupported-tool behaviour.
- Extensions use the existing task lifecycle and capability boundary. No
  extension-owned execution loop or provider-issued authority is introduced.
- The CLI currently registers no extension handlers, so default CLI behaviour
  remains built-in-only. The production host registry is a separate concern.

## Backward compatibility

- Adding `WorkspaceAction::Extension` is an explicitly scoped public-action
  migration. Downstream exhaustive matches must add a wildcard arm or the new
  variant.
- With an empty host registry, existing built-in turn flows and observations are
  unchanged.
- Unsupported provider tool calls that carry JSON arguments now map to the new
  extension action instead of a generic error string, so a handler can receive
  them only when the host has explicitly registered and granted that name.
