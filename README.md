# Krypper

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
| `krypper models` | Browse the offline model catalog |

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

## Architecture

<!-- architecture:generated:start -->
```mermaid
flowchart TD
    root["Krypper crate"] --> m_agent_loop["agent_loop"]
    root["Krypper crate"] --> m_brain["brain"]
    root["Krypper crate"] --> m_catalog_data["catalog_data"]
    root["Krypper crate"] --> m_cli["cli"]
    root["Krypper crate"] --> m_core["core"]
    root["Krypper crate"] --> m_harness["harness"]
    root["Krypper crate"] --> m_streaming["streaming"]
    root["Krypper crate"] --> m_task_lifecycle["task_lifecycle"]
    root["Krypper crate"] --> m_tools["tools"]
    m_tools --> registry["Typed workspace registry"]
    registry --> op_offline_cargo_test["offline_cargo_test"]
    registry --> op_read_file["read_file"]
    registry --> op_read_range["read_range"]
    registry --> op_list_directory["list_directory"]
    registry --> op_create_directory["create_directory"]
    registry --> op_search_text["search_text"]
    registry --> op_search_tree_text["search_tree_text"]
    registry --> op_create_file["create_file"]
    registry --> op_replace_text["replace_text"]
    registry --> op_apply_patch["apply_patch"]
```
<!-- architecture:generated:end -->
Generated from declared crate modules and the typed workspace operation registry. This inventory does not represent runtime call edges. Update with `cargo run --example generate_architecture`. See [Development guide](DEVELOPMENT.md) for the detailed implementation and proposed changes.

## Documentation

- [CLI and runtime reference](docs/CLI_REFERENCE.md) — commands, model providers, checkpoints, streaming, and safety constraints
- [Development guide](DEVELOPMENT.md) — code layout, architectural boundaries, and migration notes
- [Documentation directory](docs/) — focused technical guides

## Contributing

See the repository's existing development guidance and pull requests before proposing changes. Run `cargo test` locally when changing Rust code.
