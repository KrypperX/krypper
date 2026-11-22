# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately. Do not open a public issue
or pull request that describes an exploitable problem.

Use GitHub's private vulnerability reporting: open the repository's
**Security** tab and choose **Report a vulnerability**. Include the affected
commit or version, what you observed, and the smallest steps that reproduce it.

You can expect an acknowledgement of your report, an assessment of its impact,
and a coordinated fix and disclosure. Please allow reasonable time for a fix
before publishing details.

## Supported versions

Krypper has no tagged stable release line yet. Security fixes are made on the
`main` branch only.

## Security-sensitive areas

Reports in these areas are especially welcome:

- Workspace confinement and path handling for file tools.
- The process sandbox and command execution boundaries.
- Handling of provider credentials (API keys and tokens), including where they
  are sent and whether they can appear in logs, errors or snapshots.
- Validation of provider endpoints and request headers.
- MCP transport and subprocess lifecycle handling.
