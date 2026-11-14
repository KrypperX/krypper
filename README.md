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
mkdir -p "task workspace"
cargo run -- task --goal "Summarize the brief" --max-actions 3 --workspace "task workspace"
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

`task --goal <goal> --max-actions <positive-number> [--workspace PATH]`
runs one task across bounded turns in-process. The action limit applies to
that invocation and is shared across its continued turns. Without
`--workspace`, the current directory is its workspace. With `--workspace PATH`,
Krypper validates and captures that existing directory before task history
starts; it does not change the process current directory. The same captured
workspace is used for caller-directed and model-selected actions. It reads one
caller decision per line from standard input when `--model` is omitted. Enter `read <path>`,
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

### Pause and resume a task

`--checkpoint-out PATH` opts into one bounded turn for this invocation. If that
turn completes with `continue`, Krypper writes a checkpoint at the safe turn
boundary and reports `Task state: running` and `Task outcome: paused (still running)`.
It does not report task success. The output path must name a new
file; Krypper refuses to overwrite an existing file. If the turn completes the
task or fails, no checkpoint is written.

The turn must complete with `continue` to reach that save boundary. A failed or
interrupted turn may already have changed workspace files while leaving the
most recent checkpoint unchanged or the requested new checkpoint absent.
Inspect and reconcile the current workspace before resuming from an older
checkpoint after such a turn.

Resume one running task with `--resume PATH --max-actions N`. Krypper restores
the stored task description and starts a fresh turn. Supply `--workspace PATH`
to restore from another process current directory; it must identify the same
captured workspace. Without it, the process current directory must match the
checkpoint workspace. `--goal` is omitted when resuming. The invocation can
also specify `--model NAME` to opt into the existing local Ollama decision path.
Combine `--resume` with `--checkpoint-out` to save a later pause to a separate,
new file.

Checkpoints retain session and lifecycle metadata, including metadata-only
activity summaries, but not prior observations, workspace action inputs, or
file contents. Restored tasks do not replay earlier workspace actions; resumed
decisions receive fresh observations from the current workspace. Resume from
the newest checkpoint and choose a new output path for every pause.

For example, run a caller-directed task and enter `create first.txt saved`
followed by `continue`:

```sh
cargo run -- task --goal "Save work between turns" --max-actions 3 --checkpoint-out "task pause.json"
```

In a later process, resume it and choose a different checkpoint path for any
subsequent pause:

```sh
cargo run -- task --resume "task pause.json" --max-actions 3 --checkpoint-out "task pause 2.json"
```

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

Model selection is explicitly opt in with `--model NAME`. Without `--provider`,
this keeps the existing local Ollama path:

```sh
cargo run -- task --goal "Read the brief and create a short summary file" \
  --max-actions 6 --model qwen3
```

Combine `--workspace PATH` with `--model NAME` to keep model-selected actions
inside the selected directory. The workspace path may contain spaces:

```sh
mkdir -p "task workspace"
cargo run -- task --goal "Read the brief and create a short summary file" \
  --max-actions 6 --workspace "task workspace" --model qwen3
```

To choose a hosted API, pass both `--provider` and `--model`, and supply its key
in the environment. Each explicit provider is a network opt-in:

```sh
GEMINI_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider gemini --model gemini-2.5-flash
```

```sh
OPENAI_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider openai --model MODEL

ANTHROPIC_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider anthropic --model MODEL

CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... cargo run -- task \
  --goal "Read the brief" --max-actions 6 --provider cloudflare --model MODEL

GROQ_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider groq --model MODEL

OPENROUTER_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider openrouter --model MODEL

DEEPSEEK_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider deepseek --model MODEL

ZAI_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider zai --model MODEL

MISTRAL_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider mistral --model mistral-small-latest
```

Krypper uses fixed HTTPS endpoints and does not accept custom API URLs:

| Provider | Required environment variables | Fixed endpoint |
| --- | --- | --- |
| Gemini | `GEMINI_API_KEY` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` |
| OpenAI | `OPENAI_API_KEY` | `https://api.openai.com/v1/responses` |
| Anthropic | `ANTHROPIC_API_KEY` | `https://api.anthropic.com/v1/messages` |
| Cloudflare Workers AI | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN` | `https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/v1/chat/completions` |
| Groq | `GROQ_API_KEY` | `https://api.groq.com/openai/v1/chat/completions` |
| OpenRouter | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1/chat/completions` |
| DeepSeek | `DEEPSEEK_API_KEY` | `https://api.deepseek.com/chat/completions` |
| Z.AI Standard API | `ZAI_API_KEY` | `https://api.z.ai/api/paas/v4/chat/completions` |
| Mistral | `MISTRAL_API_KEY` | `https://api.mistral.ai/v1/chat/completions` |

The account ID is validated as one path component before a task starts. Krypper
reads credentials only for the selected provider and does not write keys to
output, errors, logs, lifecycle data, or checkpoints. Gemini uses its native
GenerateContent API and the `x-goog-api-key` header; its model name must be one
safe path component, and no custom API URL is accepted. See Google's
[GenerateContent API](https://ai.google.dev/api/generate-content),
[function calling guide](https://ai.google.dev/gemini-api/docs/generate-content/function-calling),
and [API key guide](https://ai.google.dev/gemini-api/docs/api-key). OpenAI uses
the Responses API, Anthropic uses the Messages API, and Cloudflare Workers AI, Groq,
OpenRouter, DeepSeek, Z.AI, and Mistral share the OpenAI Chat Completions-compatible task adapter.
DeepSeek, Z.AI, and Mistral use `tool_choice: auto`; Mistral also disables parallel tool calls.
Krypper still requires exactly one supported call and validates its arguments locally. DeepSeek
and Z.AI accept function arguments as either JSON text or a JSON object; Mistral uses JSON text.
Z.AI uses the Standard API endpoint shown above; Coding Plan endpoints are not supported. See the
[Cloudflare Workers AI compatibility guide](https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/),
[Groq OpenAI compatibility guide](https://console.groq.com/docs/openai), and
[OpenRouter Chat Completions API](https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion),
[DeepSeek Chat Completions API](https://api-docs.deepseek.com/api/create-chat-completion/),
[DeepSeek function calling](https://api-docs.deepseek.com/guides/function_calling),
[Z.AI Chat Completion API](https://docs.z.ai/api-reference/llm/chat-completion), and
[Z.AI function calling](https://docs.z.ai/guides/capabilities/function-calling),
[Mistral Chat Completion API](https://docs.mistral.ai/api/endpoint/chat),
[Mistral function calling](https://docs.mistral.ai/inference/conversations/function-calling), and
[Mistral API keys](https://docs.mistral.ai/admin/identity-access/api-keys) for provider request details.

OpenRouter requests disable provider fallback where supported. OpenRouter can
still select an upstream service that serves the requested model; Krypper does
not choose or reroute that upstream service.

Hosted requests include the task goal, supported action schemas, remaining
action budget, and prior observations. Observations can include workspace file
contents, so explicitly selecting Mistral or another hosted provider sends that
task data to the provider and may incur provider charges. Review the goal and
workspace data before opting in. `--model NAME` without `--provider` keeps
inference local through Ollama at `127.0.0.1:11434`; Ollama needs no API key.

Without `--model` or `--provider`, `task` stays caller-directed and makes no
model request. `--model NAME` without `--provider` continues to select local
Ollama. `--provider` requires `--model`. All providers use the same task action
budget and expose only the existing bounded actions. Missing credentials,
failed requests, invalid responses, unsupported tool calls, or an exhausted
budget fail visibly; Krypper does not fall back to caller input or another
provider. Ollama model names ending in `:cloud` or `-cloud` are rejected so
workspace observations stay with local inference.

Each model request includes the selected model name, task goal, supported tool
schemas, remaining action budget, and prior workspace observations. Read
observations can include full workspace file text; range reads include their
excerpt. Directory listings, search locations, and action outcomes are also
sent to the selected service when model mode is enabled. Requests and
accumulated observations have size limits.

The adapter exposes only the existing task workspace actions and task
completion. Every selected action still passes through Krypper's existing
workspace containment, symlink, UTF-8, size, lifecycle, and action-budget
checks; model output cannot request shell or process execution. See Ollama's
[tool-calling API example](https://github.com/ollama/ollama/blob/main/docs/capabilities/tool-calling.mdx)
for its local chat tool-call format.

## In-process local model decisions (opt in)

In-process callers can explicitly select the same local Ollama adapter used by
`krypper task --model NAME`. Create
`krypper::OllamaDecisionSource::new(model, action_budget)` and pass it to
`run_goal_turn_with_budget`:

```rust
let mut model = OllamaDecisionSource::new("qwen3", max_actions);
let result = run_goal_turn_with_budget(
    &mut session,
    task_id,
    turn_id,
    goal,
    max_actions,
    &mut model,
)?;
```

The caller creates and retains its `TaskSession`, task ID, turn ID, and
lifecycle. The runner returns ordered observations, action count, stop reason,
and turn outcome; the caller updates the task state and starts another turn
after `continue`, as the example does.

Constructing or omitting the adapter makes no request. When explicitly passed
to the bounded runner, it sends requests only to
`http://127.0.0.1:11434/api/chat`; the endpoint cannot be changed. Each request
contains the task goal, tool schemas, remaining action budget, and prior
workspace observations. Read observations can contain full file text or range
excerpts. Model-selected tools and arguments are validated, then run through
the same existing action, containment, symlink, UTF-8, size, lifecycle, and
budget checks as caller-directed turns.

The adapter also works with a fresh turn on a session returned by
`TaskSession::restore_checkpoint_in_workspace`. The caller supplies the
captured workspace path and creates the new running turn; the adapter does not
change the process current directory or replay earlier workspace actions. The
restored-session mock test covers this path without contacting Ollama.

Build and run the in-process model example with a local Ollama service and a
tool-calling model such as `qwen3`:

```sh
cargo build --example bounded_local_model_task_turn --locked
cargo run --example bounded_local_model_task_turn --locked
```

## Bounded task-turn example

The default task-turn library API remains caller-directed and offline. Callers
supply a decision source; the library does not connect to a model unless the
caller explicitly supplies `OllamaDecisionSource`. The caller-directed example
creates a unique temporary workspace, reads a brief through a task-owned turn,
creates a new summary file, explicitly completes the turn, and prints the
result and metadata-only activity history. It removes only the temporary
directory it created.

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

### Select a task session workspace

`TaskSession::new_in_workspace(path)` opts into a selected directory. It captures
and verifies the directory before returning, and every task created by that
session keeps using the captured root even if the process current directory
later changes. The constructor does not change process-wide directory state.
`TaskSession::new()` retains its current behavior and captures the current
directory when each task is created. Invalid and non-directory paths return an
error before a session is created.

```rust
let mut session = TaskSession::new_in_workspace("./project")?;
let task_id = session.create_task("Summarize the project").id();
```

The in-process `bounded_task_turn` example creates a temporary workspace with
this constructor and runs caller-directed actions through the bounded runner.
The `bounded_local_model_task_turn` example uses the same constructor with the
explicit local-model adapter. Both examples leave the process current directory
unchanged.
Use the build and run commands in the Bounded task-turn example section above.

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
