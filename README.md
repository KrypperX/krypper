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

`task --goal <goal> --max-actions <positive-number>` runs one task across
bounded turns in-process with the current directory as its workspace. It reads
one caller decision per line from standard input. Enter `read <path>`,
`read-range <path> <start-line> <max-lines>`, `list [path]`, `mkdir <path>`,
`create <path> <text>`,
`replace <path> <match text> => <replacement text>`,
`search <directory> <query>`, `search-tree <directory> <query>`, `continue`,
`complete [result]`, or `cancel`.

The unquoted path forms remain supported. `read`, `list`, and `mkdir` take the
remaining line as the path; `list` defaults to `.`. `read-range` treats the
final two whitespace-separated fields as the start line and maximum line
count. For `create`, `replace`, `search`, and `search-tree`, an unquoted path
is the first whitespace-delimited token because the rest of the line is
content, `MATCH_TEXT => REPLACEMENT_TEXT`, or a query. Each of these eight
actions also accepts a JSON double-quoted path token, which allows spaces in
the path.

Quoted path tokens accept literal valid Unicode and JSON escapes: `\\`, `\"`,
`\/`, `\b`, `\f`, `\n`, `\r`, `\t`, and `\uHHHH` (a supplementary
character uses a valid surrogate pair). On Unix, `\xNN` with exactly two
hexadecimal digits represents one raw path byte, including non-UTF-8 bytes.
`search` and `search-tree` print each result as a reusable quoted path token
followed by `:<line>`. Query and file-content text after the path keeps its
existing syntax; only the path is decoded as a token.

For example, after `search . needle` prints:

```text
Observation 1: search in . found 1 locations
  "guides with spaces/quote \"example\"/back\\slash.txt":12
```

reuse the result path in the next action:

```text
read-range "guides with spaces/quote \"example\"/back\\slash.txt" 12 4
```

Create and replace also preserve their existing limits and behavior. Replace
requires one non-empty exact match in an existing UTF-8 file. It rejects
missing or ambiguous matches, symlink paths, non-regular files, and
replacements that would exceed 1 MiB.
Replacement output and retained activity history contain only path and byte
count metadata. Read and list actions use the same workspace policy as the
existing commands, while create is create-only and follows the same path and
size limits as `write`. `mkdir` creates exactly one directory beneath the
captured workspace and requires its parent to exist; its retained activity
summary records only the action category. `read-range` uses one-based line numbers, requires a
positive start and count, accepts at most 200 requested lines, and returns at
most 64 KiB of excerpt text. It reads the same safe UTF-8 files as `read`.
Empty files contain no lines; a file containing only a newline has one empty
line; a final newline does not add an extra line. Returned lines are joined with
`\n` and omit the final line ending. A start beyond the available lines fails,
while a range extending past end-of-file returns the remaining lines and their
actual start and end numbers. Excerpt text appears only in the action
observation; retained activity and lifecycle summaries contain the actual range
and excerpt byte count only.
Search checks immediate regular UTF-8 files in the selected directory, includes
hidden names, skips entry symlinks and non-regular files, and does not recurse.
Its case-sensitive literal query is limited to 4 KiB, each file to 1 MiB, total
bytes scanned to 16 MiB, and returned file-line locations to 10,000. The
existing 1,024-entry directory limit applies too. Results contain relative paths
and one-based line numbers, never matching line text; invalid UTF-8 and oversized
files fail the whole action.
`search-tree` uses the same literal-match and result format while recursively
searching regular UTF-8 files beneath the selected directory. It includes hidden
names, skips symlink and non-regular entries, and limits traversal to depth 16
and 4,096 directory entries total. Query, per-file size, cumulative bytes, and
result locations keep the same 4 KiB, 1 MiB, 16 MiB, and 10,000 limits. Any
limit or invalid UTF-8 failure rejects the action without partial results.
`continue` ends the active turn and starts another for the same running task.
The action limit is shared by the whole invocation, and each `continue`
selection consumes one action. The CLI prints observations before asking for
the next action, then reports every turn result, the aggregate action count,
the final task outcome, ordered lifecycle transitions, and metadata-only
activity history. Invalid input, end-of-input, an action failure, or an
exhausted action budget fails the active turn and task and exits with status 1.
`complete` makes the task terminal and counts as an action against the limit.
Caller cancellation also consumes one action, cancels the task and active turn,
and exits with status 130.

Each task action line is limited to 1,052,672 bytes (1 MiB + 4 KiB), counting
its line ending when present. Oversized lines fail the active turn and task
before parsing or workspace access. This limit leaves room for the command and
a relative path alongside a maximum-size create body.

## Model-selected task actions (opt in)

The task CLI can ask a local Ollama model to choose the next bounded action.
Install Ollama using its [official instructions](https://ollama.com/download),
then pull a locally served model that supports tool calls. Ollama's tool-call
examples use `qwen3`:

```sh
ollama pull qwen3
ollama serve
```

Run `ollama serve` in a terminal only when the Ollama service is not already
running. The first pull downloads the model; the task command itself uses the
local service at `http://127.0.0.1:11434/api/chat`.

Model selection is explicitly opt in with `--model NAME`:

```sh
cargo run -- task --goal "Read the brief and create a short summary file" \
  --max-actions 6 --model qwen3
```

Without `--model`, `task` stays caller-directed, remains offline, and does not
contact Ollama. Model mode uses the same action budget as caller-directed mode;
completion also uses one action. A missing local service, failed request,
invalid response, unsupported tool call, or exhausted budget fails the task
with visible task and turn outcomes. Krypper does not fall back to caller input
or another model. Model names ending in `:cloud` are rejected.

Each model request includes the selected model name, task goal, supported tool
schemas, remaining action budget, and prior workspace observations. Read
observations can include full workspace file text; range reads include their
excerpt. Directory listings, search locations, and action outcomes are also
sent to the local service when model mode is enabled. Requests and accumulated
observations have size limits. No remote endpoint or cloud-provider option is
available.

The adapter exposes only the existing task workspace actions and task
completion. Every selected action still passes through Krypper's existing
workspace containment, symlink, UTF-8, size, lifecycle, and action-budget
checks; model output cannot request shell or process execution. See Ollama's
[tool-calling API example](https://github.com/ollama/ollama/blob/main/docs/capabilities/tool-calling.mdx)
for its local chat tool-call format.

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

The checkpoint recovery example completes one turn after creating a file, then
exports checkpoint bytes at the safe boundary. Caller-side `std::fs` code writes
them to the explicitly chosen `task-session.checkpoint.json` file in the
temporary workspace. The example drops the original session, reads that file,
and restores in the same captured workspace. Restore leaves the first file
unchanged; the caller explicitly selects the file created by the fresh turn.
The TaskSession API returns checkpoint bytes but does not write files or add a
storage backend. Restore requires the original captured workspace, and the
example removes its temporary workspace and checkpoint file when it finishes.

Build and run it from the repository root:

```sh
cargo build --example task_session_checkpoint_recovery --locked
cargo run --example task_session_checkpoint_recovery --locked
```

## Restore a task session in a later process

The process-restore example has separate `save` and `resume` invocations. Save
requires a state-directory path that does not already exist. It creates a
`workspace` directory there, completes one turn that creates `first-turn.txt`,
and writes checkpoint bytes to the sibling
`task-session.checkpoint.json` file using exclusive file creation. The
checkpoint stays outside the captured workspace.

Build and run the two invocations separately from the repository root:

```sh
cargo build --example task_session_process_restore --locked
STATE_DIR="/path/to/krypper saved task"
cargo run --example task_session_process_restore --locked -- save "$STATE_DIR"
cargo run --example task_session_process_restore --locked -- resume "$STATE_DIR" "selected after restore.txt" "Created by the fresh turn."
```

Keep the same `workspace` directory in place between invocations so restore
can validate the captured workspace identity. Resume supplies that workspace
path explicitly, so it can run from a different process current directory
without changing it. Resume restores lifecycle state without replaying the
first file action, then creates only the file and content selected in its
command. The caller controls the state directory and checkpoint location and
is responsible for cleanup; the example does not delete caller state.

## Task lifecycle

`krypper::task_lifecycle` provides `Task`, `TaskSession`, `TaskState`, `Turn`,
and `TurnState` for in-process use. Tasks and turns start as `pending`; valid
transitions append ordered events, and `Task::events()` and `Turn::events()`
expose read-only histories. A task creates turns in creation order, transitions
them by ID, and lists them without exposing mutable turn or event collections;
each turn's events include its task and turn IDs. Sessions find tasks by ID and
list them in creation order.

`TaskSession::export_checkpoint()` returns versioned JSON bytes for in-process
persistence. Export is allowed only when no turn is running, all tasks share the
same captured workspace, and the encoded checkpoint is at most 1 MiB. It retains
task and turn IDs, descriptions, states, outcomes, ordered lifecycle histories,
workspace binding, and the existing metadata-only activity summaries. It omits
workspace action inputs and observations. The caller chooses whether and where to
store the bytes; export does not write a file.

`TaskSession::restore_checkpoint(&bytes)` validates the version, size, IDs,
histories, outcomes, activity summaries, and workspace binding before returning
a session. This existing API uses the process current directory as the workspace.
`TaskSession::restore_checkpoint_in_workspace(&bytes, workspace_path)` accepts
the captured workspace explicitly, allowing restore from a different process
current directory without changing it. Both APIs validate the recorded
canonical path and device/inode identity for task-bearing checkpoints; a
missing, different, or replaced workspace is rejected. Restore rebuilds
in-memory lifecycle state and never replays workspace actions. A restored
running task with no running turn can create a fresh turn.

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
