# Krypper

Krypper is a local Rust command-line tool and in-process task runtime. Runtime
behavior stays on the machine and works offline.

## Build and run

Build the binary from the repository root:

```sh
cargo build --release
cargo run -- show README.md
cargo run -- list
cargo run -- list docs
cargo run -- write notes/today.txt "Remember to review the change."
cargo run -- write --stdin notes/from-pipe.txt < notes/draft.txt
cargo run -- task --goal "Summarize the brief" --max-actions 3
```

The release binary is `target/release/krypper`. Run it with the directory you
want to work in as the process current directory. For example, to list another
workspace, run `/path/to/krypper/target/release/krypper list` after changing to
that workspace.

`show` writes a regular UTF-8 file to stdout without adding output. Its relative
path must resolve inside the current directory. Symlinks may be followed when
their resolved path stays inside it. Absolute paths, missing files, directories,
invalid UTF-8, and files over 1 MiB are rejected.

`list` prints JSON-quoted immediate entry names, sorted by name. It includes
hidden names, escapes control and non-ASCII characters, does not read entry
contents, and does not follow symlink entries. The optional directory path
defaults to the current directory; a symlink used for that path may be followed
only when it resolves inside the current directory.

`write <path> <text>` joins the text arguments with spaces and creates one new
UTF-8 file. `write --stdin <path>` reads UTF-8 text from standard input. Both
forms accept at most 1 MiB after UTF-8 encoding. Paths must stay within the
current directory and have existing directory parents. Write refuses symlink
components and existing targets, creates files exclusively, and removes its
partial file if a write fails. It never creates parent directories, overwrites
files, or contacts a network service.

`task --goal <goal> --max-actions <positive-number>` runs one bounded task turn
in-process with the current directory as its workspace. It reads one caller
decision per line from standard input. Enter `read <path>`, `list [path]`,
`create <path> <text>`, or `complete [result]`; a create path is one token, and
the remaining line is its text. Read and list actions use the same workspace
policy as the existing commands, while create is create-only and follows the
same path and size limits as `write`. The CLI prints each observation before
asking for the next action, then prints the terminal state, outcome, and
metadata-only activity history. Invalid input, end-of-input, an action failure,
or an exhausted action budget fails the turn and exits with status 1. Explicit
completion counts as an action against the limit.

Each task action line is limited to 1,052,672 bytes (1 MiB + 4 KiB), counting
its line ending when present. Oversized lines fail the turn before parsing or
workspace access. This limit leaves room for the command and a relative path
alongside a maximum-size create body.

## Bounded task-turn example

The task-turn library API runs in-process and offline. Callers supply the
decision source; the library does not connect to a model or make decisions for
them. The example creates a unique temporary workspace, reads a brief through a
task-owned turn, creates a new summary file, explicitly completes the turn, and
prints the result and metadata-only activity history. It removes only the
temporary directory it created.

Build and run the example from the repository root:

```sh
cargo build --example bounded_task_turn --locked
cargo run --example bounded_task_turn --locked
```

Command and input errors print usage and a diagnostic and exit with status 2.

## Task lifecycle

`krypper::task_lifecycle` provides `Task`, `TaskSession`, `TaskState`, `Turn`,
and `TurnState` for in-process use. Tasks and turns start as `pending`; valid
transitions append ordered events, and `Task::events()` and `Turn::events()`
expose read-only histories. A task creates turns in creation order, transitions
them by ID, and lists them without exposing mutable turn or event collections;
each turn's events include its task and turn IDs. Sessions find tasks by ID and
list them in creation order.

## Checks

Run formatting, build, check, lint, and the complete Rust test suite with:

```sh
cargo fmt --all -- --check
cargo build --locked
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

The existing [verify workflow](.github/workflows/verify.yml) runs these Rust
checks for pull requests and pushes to `main`.

Contributions should go through reviewed pull requests and the existing required checks.
