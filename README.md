# Krypper

- **Created by:** Bom Thatchakorn
- **Organization:** KrypperX
- **Origin:** Thailand (TH) 🇹🇭

A local-first Rust CLI and in-process agent task runtime. Work with files in a bounded workspace, run task turns, and optionally connect to supported AI model providers.

## Install

**Build from source** (requires Rust and Cargo):

```sh
git clone https://github.com/KrypperX/krypper.git
cd krypper
cargo build --release
```

The executable is `target/release/krypper`. Run it from the directory you want to use as your workspace.

## Commands at a glance

| Command | Purpose |
| --- | --- |
| `krypper list [path]` | List entries in a workspace directory |
| `krypper show <path>` | Display a UTF-8 file |
| `krypper write <path> <text>` | Create a new file |
| `krypper task --goal <goal> --max-actions <n>` | Run a bounded task |
| `krypper models` | Browse the offline catalog; refresh and show provenance explicitly |
| `krypper context --goal <goal>` | Show bounded offline repository context, or an opt-in Rust module map |
| `krypper setup` | Choose a provider and model for this user |

## Quick start

```sh
# List files in the current workspace
./target/release/krypper list

# Read a file
./target/release/krypper show README.md

# Start a bounded, caller-directed task
./target/release/krypper task --goal "Summarize the brief" --max-actions 3
```

Task actions can also be selected by an explicitly configured model. See the [CLI and runtime reference](docs/CLI_REFERENCE.md) for provider setup, task controls, and safety limits.

## Platform support

- **Linux:** workspace operations, MCP stdio connections, and the opt-in sandboxed Cargo test profile are supported.
- **macOS:** workspace operations and MCP stdio connections are supported. Sandboxed Cargo tests are unavailable and return a typed `SandboxUnavailable` result because the Linux confinement backend is not used there.
- **Windows:** only the compile-checked platform foundation is available. Native workspace containment is not implemented, and `TaskSession::new_in_workspace` reports `TaskSessionWorkspaceErrorKind::PlatformUnavailable`; MCP stdio and sandboxed Cargo tests are unavailable. Use WSL2 for the Linux workflow until the Windows backend is implemented.

Run `krypper setup` from any directory to choose a provider, search the offline model catalog, review the selection, and save your default model. Credential input is hidden. On Unix, the local credential directory and file are restricted to your user; other platforms inherit permissions from the per-user config directory. The credential file is not encrypted; environment variables override saved credentials. Use `krypper setup --status` or `krypper setup --status --json` to inspect routing/readiness without showing credential values. Automatic routing is opt-in with `krypper setup --routing-mode automatic`; cross-provider data transfer and bounded fallback require separate flags. `krypper setup --reset` removes local settings and routing evidence.

For a temporary Cloudflare Workers AI or AI Gateway setup that keeps credentials only in the current shell, source the Bash helper from the repository root:

```sh
source scripts/cloudflare-workers-ai-env.sh
```

It prompts for the account ID, a hidden API token, and an optional AI Gateway ID, then exports them in the current shell. Use `--provider cloudflare` for Workers AI or `--provider cloudflare-ai-gateway` for Gateway routing. The token remains available to child processes until you run `unset CLOUDFLARE_API_TOKEN CLOUDFLARE_ACCOUNT_ID CLOUDFLARE_AI_GATEWAY_ID` or close that shell.

## Documentation

- [CLI and runtime reference](docs/CLI_REFERENCE.md) — commands, model providers, checkpoints, streaming, and safety constraints
- [Context model evaluation](docs/context-model-evaluation.md) — offline preflight and opt-in same-model comparison runner
- [Model routing evaluation](docs/model-routing-evaluation.md) — offline preflight and opt-in manual/automatic route comparison
- [Provider routing migration](docs/provider-routing-migration.md) — compatibility, automatic routing setup, and rollback
- [Development guide](DEVELOPMENT.md) — code layout, architectural boundaries, and migration notes
- [Documentation directory](docs/) — focused technical guides

## Contributing

See the repository's existing development guidance and pull requests before proposing changes. Run `cargo test` locally when changing Rust code.

## License

Krypper is licensed under the [Apache License, Version 2.0](LICENSE).

Third-party components and vendored data retain their own licenses. In particular, the checked-in [models.dev snapshot](snapshots/models-dev/) is MIT-licensed; its upstream copyright and license notice are preserved in [`snapshots/models-dev/LICENSE`](snapshots/models-dev/LICENSE). See [third-party licensing notes](docs/third-party-licenses.md) for review scope and limitations.
