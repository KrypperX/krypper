# Krypper CLI and Runtime Reference

Krypper is a local Rust command-line tool and in-process task runtime. Workspace operations run locally; explicitly selected hosted model providers require network access.

See [DEVELOPMENT.md](../DEVELOPMENT.md) for the current code layout, proposed
module boundaries, observability contract, and migration rules.

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

### Permission modes

The default permission mode is `ask`. Save a per-user preference or override it
for one task:

```sh
krypper setup --permission-mode ask
krypper setup --permission-mode risk-based
krypper setup --permission-mode full-access
krypper setup --offline-process --toolchain-root /opt/krypper/toolchain --registry-root /opt/krypper/cargo/registry
krypper task --goal "Review this workspace" --max-actions 5 --permission-mode ask
krypper task --goal "Review and test this workspace" --max-actions 5 --sandbox-profile offline
```

The permission preference is stored in a separate per-user file, independent of
provider credentials and model defaults. `KRYPPER_CONFIG`, workspace files,
model output, and checkpoints cannot select or elevate a permission mode.
`task --no-config` uses Ask unless the task has an explicit `--permission-mode`.
The active mode is printed before the task runs. It is chosen for each
invocation; a checkpoint does not restore an older mode.

Both Ask and Risk-based keep the existing workspace boundary for file actions.
The host policy asks before process or external effects in Ask mode. Risk-based
automatically permits bounded workspace actions and the fixed offline Cargo
profile only when its trusted host configuration is present. Ask requires a
trusted exact-action prompt where an effect supports one; unknown Full Access
effects remain unavailable. Offline process tools stay unavailable unless the
task opts in with `--sandbox-profile offline`. Configure private user-level
toolchain and offline registry roots with `krypper setup --offline-process
--toolchain-root PATH --registry-root PATH`; setup validates existing roots but
does not install files. The configuration ignores `KRYPPER_CONFIG` and
workspace contents. The task workspace and both host roots must be disjoint,
and neither host root may overlap the private per-user settings directory that
stores credentials and provider profiles. The private settings file must also
be outside the selected workspace. Mounted toolchain and registry contents are
read-only but remain readable by sandboxed Cargo/Rustc and model-controlled
build code; do not put secrets in those roots. A
workspace rooted at the user's home directory is therefore rejected when it
contains that settings file. `krypper setup --status` reports whether the
profile is configured and describes the settings-directory and per-task
workspace checks. This process profile is Linux-only.
Here, `offline` applies only to Krypper process actions; model/provider
requests still use the selected provider and may send task data.

With the offline profile selected, each `offline_read_only_exec` request is
shown as a closed executable identity, separately escaped argv items, and a
workspace-relative working directory. Ask and Risk-based both require a fresh
controlling-terminal `yes` for each direct-argv action. The fixed
`offline_cargo_test` keeps its mode behavior: Ask prompts for that exact filter,
while Risk-based permits only the fixed offline profile when the user roots
are configured. Missing TTY or denied approval prevents dispatch. Full Access
does not auto-grant either sandbox process action: fixed Cargo still requires
its exact per-action prompt, and direct argv remains unavailable in Full Access.
A resumed task needs the profile flag again and receives new one-shot grants.
`krypper setup --status`
continues to report the selected permission mode and Linux host-process path.

Full Access is Linux-only and applies to the registered direct-argv host
process action. Before provider startup, the CLI displays the risk and requires
the user to type `yes` in the controlling terminal for that task. A saved
preference or `--permission-mode full-access` alone is not authorization. If
the terminal is unavailable or the user declines, the task stops before the
provider starts. The action runs as the current OS user with inherited
environment, filesystem and network access; Krypper provides no process
sandbox, workspace boundary, or egress restriction for it. Outer OS/container
policy still applies, and Full Access does not grant root. Checkpoint restore
does not restore this confirmation; resume prompts again.
The reported `process_group_cleanup` value checks only the original process
group and captured output pipes; detached descendants can escape that check.

Offline Cargo keeps its existing separate host authorization. The Full Access
selection does not widen it or the bounded offline read-only profile.
Captured host-process output may be sent to the selected AI provider in the
next request, so commands that read local secrets can expose them to that
provider or over the process's unrestricted network access.

### Verified planning and correction

Opt in to a local planned workflow by adding `--verified` and one or more
`--verify-file PATH` values. Paths must be distinct, UTF-8, workspace-relative
file paths no longer than 192 UTF-8 bytes, with no absolute or parent-traversal
components or control characters; at most 32 are accepted. The mode requires a
model from `--model` or saved model defaults, the Ollama provider, and at least
two actions.
Hosted providers are rejected before credentials are read. Without these flags,
the existing task behavior is unchanged.

The task description, current-turn observations, and applicable workspace
guidance from root or nested `AGENTS.md` files go to the local Ollama service.
These can include workspace text, so use a local Ollama service you trust.

```sh
cargo run -- task \
  --goal "Create a short summary file" \
  --max-actions 5 --model qwen3 --verified \
  --verify-file "output/summary.txt"
```

The local model proposes an ordered plan. Harness runs it through the existing
TaskSession and Tools boundary, then checks each required file against the
current turn's TaskSession observations. A successful file read, range read,
create, replace, or patch is evidence for that exact path; model completion text
is not. This confirms that a file operation succeeded in the current turn, not
that its contents match an expected value. A recorded failed file operation is
`Failed`, while missing evidence is `Unknown`. Task success requires a completed
turn and passing evidence for every required file.

Before the initial or correction verified plan writes files, Harness previews
proposed edits and searches bounded references. Multi-file plans, detected
public Rust API changes, references outside the mutation targets, and actionable
unknown or truncated evidence receive a review request to the same local model
with the impact report. If the reviewed plan changes that report, the model must
confirm the final report and keep the plan unchanged. Existing-file writes
recheck a full content revision immediately before atomic replacement, though a
concurrent external writer can race that check. This evidence is lexical and
bounded; it does not resolve macros or guarantee a consistent snapshot across
files. Files commit atomically one at a time, so a later failure can leave earlier
successful edits in place. See the
[impact analysis guide](safe-impact-analysis.md) for bounds and recovery details.

After the planned operations, the CLI prompts for task control. Enter
`complete [result]` to run the checks, `continue` to start another bounded turn,
or `cancel` to stop. These control actions still run through the existing
Harness and TaskSession lifecycle.

`--max-actions` caps Harness actions (planned workspace operations and caller
task-control actions) across all turns in this invocation; Ollama requests are
not included in the displayed action count. If a normally completed turn has
failed or unknown checks and at least five action slots remain, Krypper may
request one reflection-informed correction plan per CLI invocation. A confirmed
offline test failure requires at least six slots so the correction can also rerun
tests after its edits. These thresholds reserve the reflection and plan requests,
room for impact review, and the required workspace operations; model requests do
not add to the displayed Harness action count. Krypper does not retry a denied or
failed workspace action or a turn ended by cancellation,
continuation, or insufficient budget. The correction uses the same local model
source and only the remaining `--max-actions` slots. Output reports each check
and evidence summary, whether correction ran or was skipped, the
Harness stop reason, and remaining actions. If a turn records both successful
and failed file operations for a required path, `Failed` takes precedence.
Directory creation does not count as file evidence.

With `--checkpoint-out`, entering `continue` after the initial, uncorrected
turn saves a checkpoint at the existing safe turn boundary and defers
correction. If a correction has already run and its turn ends with `continue`,
the checkpoint saves after that correction and its one-correction allowance
remains used. Resume with `--verified`, repeat
each `--verify-file`, select the local Ollama model, and supply a fresh
`--max-actions` value. Checkpoints retain lifecycle metadata but do not store
verified-mode settings or prior observations, so the resumed turn must establish
file evidence again through TaskSession.

### Task activity and event JSONL

`--activity-jsonl` writes the read-only TaskSession lifecycle and activity
positions to stderr as JSONL after each turn. The records contain positions,
not prompts, workspace paths, or tool arguments. `--stream-events-jsonl` writes
versioned JSONL event envelopes to stderr after each turn, with monotonically
increasing sequence numbers and deduplicated activity-position updates. This
CLI option exports task metadata; it does not stream model response text or
tool output. Use `--model-usage-jsonl` separately for provider usage records.

### Observation context compaction

Model-backed tasks can opt in to a smaller serialized observation payload:

```sh
cargo run -- task \
  --goal "Inspect the large build log" \
  --max-actions 4 \
  --observation-context-budget-bytes 262144
```

The value must be between 1 byte and 2 MiB. It applies to the current turn's
observation JSON only; it does not cap the goal, system instructions, project
guidance, retrieved context, provider request overhead, or token count. The
default leaves current model input unchanged. It is not available with
`--verified`, whose planning and review requests use a separate flow.
The opt-in projection rejects source observation histories above its 16 MiB
input ceiling before making a model-facing copy. The value is invocation-scoped
and is not stored in checkpoints; pass it again when resuming if you want the
same limit on the new turn.

When the observation JSON exceeds the selected value, Krypper shortens older
text fields first so newer evidence has priority. It keeps structured event
types, paths, ranges, and process outcomes. Each shortened field includes an
explicit note with the original byte count and SHA-256 digest; if all eligible
text is removed and the structured observations still exceed the limit, the
model decision fails with `context_too_large`. This creates a model-facing
copy and does not alter the authoritative TaskSession observations or workspace output.
For each request that was compacted, stderr receives a
`KRYPPER_CONTEXT_COMPACTION` JSON record with original and sent byte counts,
compacted field count, and omitted byte count. The record contains no prompt or
workspace text.

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

Replace and patch stage new contents in a hidden file in the target directory,
then atomically rename it over the target. The staged file is created with
requested mode `0600` and is never more permissive than that before the rename;
Krypper then applies the original file's ordinary `rwx` permission bits and
syncs that metadata. If this post-rename step fails, the
operation is reported as committed but unverified; the target may remain mode
`0600` if permission restoration was not reached. The new file gets owner and
group according to process and directory rules. The original file's timestamps,
special permission bits, ACLs, and extended attributes are not copied; directory
creation policies may still add metadata to the new file. Other names that were
hard links to the original continue to refer to its old contents.

Krypper serializes replacements of the same inode within one process and takes
a nonblocking advisory lock on the existing file before staging across
processes. This prevents two cooperating Krypper replacements from silently
overwriting each other's changes on filesystems that honor the lock. A held lock
rejects the operation without changing the target; a filesystem that cannot
provide the lock also rejects replacement before staging. Other programs can
ignore advisory locks, so Krypper also rechecks the original file identity and
contents immediately before rename; an external writer can still race that
final check.

Before commit, Krypper rechecks a staged file's inode before removing it and
reports when cleanup fails. An external process with write access to the target
directory may still race the check and unlink. Abrupt termination may leave a
hidden `.krypper-replace-<UUID>` file in the target directory. Krypper does not
sweep matching names automatically because a name alone cannot prove that a file
is stale or owned by Krypper; inspect such files before removing them.

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
hidden names unless an applicable `.gitignore` rule excludes them, skips entry
symlinks and non-regular files, and does not recurse. It applies root and nested
`.gitignore` files with slash-aware glob patterns (including `*`, `?`, bracket
classes, `**`, leading `/` anchors, and trailing `/` directory rules). Later
matching lines and deeper `.gitignore` files take precedence; `!` patterns
re-include paths. Unreadable, invalid UTF-8, or malformed applicable ignore
files fail the whole action without partial results. Ignore files are read only
inside the captured workspace. `.git` metadata directories are not traversed.
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

XAI_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider xai --model MODEL

ANTHROPIC_API_KEY=... cargo run -- task --goal "Read the brief" \
  --max-actions 6 --provider anthropic --model MODEL

CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... cargo run -- task \
  --goal "Read the brief" --max-actions 6 --provider cloudflare --model MODEL

CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... \
  CLOUDFLARE_AI_GATEWAY_ID=default cargo run -- task \
  --goal "Read the brief" --max-actions 6 --provider cloudflare-ai-gateway --model openai/MODEL

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

Use `--provider cloudflare` for direct Workers AI calls with `@cf/...` model IDs. Use `--provider cloudflare-ai-gateway` to route third-party `author/model` IDs or Workers AI `@cf/...` IDs through a gateway. AI Gateway routing adds the `cf-aig-gateway-id` header; when `CLOUDFLARE_AI_GATEWAY_ID` is unset, Krypper uses `default`. Choose a model that supports function calling; availability depends on the model.

To enter Cloudflare values without putting the token in shell history or writing it to a file, run `source scripts/cloudflare-workers-ai-env.sh` from the repository root. The helper exports credentials and the optional gateway ID in the current Bash session; run `unset CLOUDFLARE_API_TOKEN CLOUDFLARE_ACCOUNT_ID CLOUDFLARE_AI_GATEWAY_ID` when finished. The inference endpoint requires an Account API token with `Workers AI - Read`. AI Gateway management permissions apply to gateway configuration and logs, not model inference. See Cloudflare's [Workers AI REST API guide](https://developers.cloudflare.com/workers-ai/get-started/rest-api/) and [AI Gateway REST API guide](https://developers.cloudflare.com/ai-gateway/usage/rest-api/) for setup and model ID formats.

Pass `--model-usage-jsonl` to a model-backed task to emit one `KRYPPER_MODEL_USAGE` JSON record per decision to stderr. Compatible providers can report prompt, completion and total token counts; absent fields are `null`. The record also includes decision latency and contains no prompt, response text or credentials. This option is useful for evaluations and requires an OpenAI-compatible provider such as Cloudflare Workers AI.

For controlled evaluations, pass `--repository-context-json PATH` with a JSON file written by `krypper context`. Krypper adds it as a separate, untrusted advisory block without changing the task goal. It accepts context objects containing a `snippets` or `modules` array, caps the declared context at 4 KiB and the encoded JSON at 8 KiB, and supports OpenAI-compatible providers. The file's context is sent with the task request. This option cannot be combined with checkpoint or resume options.

To compare saved manual routing with automatic routing on isolated fixture
tasks, see the [model routing evaluation guide](model-routing-evaluation.md).
Its default mode is offline; hosted requests require the explicit `--live`
flag, provider credentials in the environment, and a fresh accepted provider
probe. The report distinguishes token-usage price estimates and router-reserved
estimates from actual billed cost, which model usage telemetry does not provide.

Built-in providers use fixed HTTPS endpoints and do not accept custom API URLs:

| Provider | Required environment variables | Fixed endpoint |
| --- | --- | --- |
| Gemini | `GEMINI_API_KEY` | `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` |
| OpenAI | `OPENAI_API_KEY` | `https://api.openai.com/v1/responses` |
| xAI | `XAI_API_KEY` | `https://api.x.ai/v1/responses` |
| Anthropic | `ANTHROPIC_API_KEY` (optional: `ANTHROPIC_WORKSPACE_ID`) | `https://api.anthropic.com/v1/messages` |
| Cloudflare Workers AI | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN` | `https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/v1/chat/completions` |
| Cloudflare AI Gateway | `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN`, optional `CLOUDFLARE_AI_GATEWAY_ID` (default: `default`) | Same Cloudflare AI endpoint with `cf-aig-gateway-id` header |
| Groq | `GROQ_API_KEY` | `https://api.groq.com/openai/v1/chat/completions` |
| OpenRouter | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1/chat/completions` |
| DeepSeek | `DEEPSEEK_API_KEY` | `https://api.deepseek.com/chat/completions` |
| Z.AI Standard API | `ZAI_API_KEY` | `https://api.z.ai/api/paas/v4/chat/completions` |
| Mistral | `MISTRAL_API_KEY` | `https://api.mistral.ai/v1/chat/completions` |

Built-in hosted model requests connect directly to the fixed endpoints above;
provider profiles connect directly to their configured endpoints. Krypper
disables environment-configured proxies, so `HTTP_PROXY`, `HTTPS_PROXY`,
`ALL_PROXY`, and `NO_PROXY` do not change model traffic. Networks that require
an HTTP proxy cannot use hosted providers with this version. Connection setup is
limited to 10 seconds for hosted endpoints and 2 seconds for local Ollama; each
request has a 120-second total limit. Krypper retries at most once for a known
DNS or connection-setup failure before sending request bytes. It does not replay
requests after transmission begins, since a provider may already have processed
or billed an ambiguous request. Automatic route fallback is a separate,
explicitly configured routing policy.

Anthropic requires the `anthropic-workspace-id` request header for API keys that
are not scoped to a workspace; it is optional for workspace-scoped keys. When
`ANTHROPIC_WORKSPACE_ID` is set, Krypper sends it as that header on both
streaming and non-streaming requests (for example `wrkspc_01JwQvzr7rXLA5AGx3HKfFUJ`).
The value is validated before a task starts (1-128 ASCII letters, digits, `-`,
`_` or `.`); an invalid or empty value is an error rather than being ignored.
When the variable is unset, no such header is sent. See Anthropic's
[API overview](https://platform.claude.com/docs/en/api/overview).

The account ID is validated as one path component before a task starts. Krypper
reads credentials only for the selected provider and does not write keys to
output, errors, logs, lifecycle data, or checkpoints. Gemini uses its native
GenerateContent API and the `x-goog-api-key` header; its model name must be one
safe path component, and no custom API URL is accepted. See Google's
[GenerateContent API](https://ai.google.dev/api/generate-content),
[function calling guide](https://ai.google.dev/gemini-api/docs/generate-content/function-calling),
and [API key guide](https://ai.google.dev/gemini-api/docs/api-key). OpenAI and xAI
use the Responses API; xAI is selected explicitly, reads `XAI_API_KEY` only for
that selection, and sends bearer-authenticated requests only to the fixed endpoint
shown above. `XAI_API_KEY` must be an Inference API key; Management API keys and
endpoints are not used. It uses Krypper's supported function tools with one
required call, parallel calls disabled, and response storage disabled. See the [xAI Responses API](https://docs.x.ai/developers/rest-api-reference/inference/responses),
[function calling guide](https://docs.x.ai/developers/tools/function-calling), and
[API authentication](https://docs.x.ai/developers/rest-api-reference/inference).
Anthropic uses the Messages API, and Cloudflare Workers AI, Cloudflare AI Gateway,
Groq, OpenRouter, DeepSeek, Z.AI, and Mistral share the OpenAI Chat Completions-compatible task adapter.
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
contents, so explicitly selecting xAI or another hosted provider sends that
task data to the provider and may incur provider charges. Review the goal and
workspace data before opting in. Model-backed task turns include the captured
workspace root's `AGENTS.md` and, for proposed edits, applicable nested ancestor
guidance. Those contents are sent to the selected hosted provider too.
`--model NAME` without `--provider` keeps inference local through Ollama at
`127.0.0.1:11434`; Ollama needs no API key.

Without `--model` or `--provider`, `task` stays caller-directed and makes no
model request, unless you created the optional user configuration file
described below. `--model NAME` without `--provider` continues to select local
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

For model-backed edits, Krypper first receives a non-mutating proposal, then
reads `AGENTS.md` files from the captured workspace root through the directories
that contain the proposed targets. It sends applicable guidance in deterministic
root-to-target order before the final edit decision. Sibling and unrelated
directory guidance is not read or included. Reads use no-follow descriptor
checks and stay inside the captured workspace. Each file and the combined
guidance context are limited to 16 KiB of valid UTF-8. Guidance is untrusted,
lower-priority context for repository conventions; it cannot change tool
authorization, workspace confinement, process permissions, budgets, or
completion evidence. With no nested guidance, the existing prompt and action
flow are preserved. Invalid target paths, symlinks, oversized or invalid UTF-8
files, and guidance changed during a safe read fail before a write.

### Opt-in repository context

Embedding code may call
`krypper::harness::execution::create_plan_with_repository_context` to add one
bounded retrieval step before that existing planning flow. Retrieval derives at
most three literal goal queries, reuses `TaskSession` tree search and safe file
reads, and includes at most four snippets, three lines each. Every snippet
carries its workspace-relative path, line range, query and a short SHA-256
content revision. Stale matches and oversized sources are skipped and counted;
directive-like lines are neutralized. The prompt labels all excerpts untrusted
and advisory. This context cannot change tool authorization, workspace
confinement, budgets, completion evidence, or the executor; without the opt-in
call, prompts and behavior are unchanged.

To inspect the same retrieval offline, run:

```sh
cargo run -- context --goal "Locate parse_goal in src/parser.rs" --workspace "task workspace"
```

`context` makes no model request, changes no file, and prints bounded JSON with
the queries, attributed snippets, counters and truncation flag.

For an opt-in structural view of out-of-line Rust modules, pass
`--repository-map`. It reuses the same bounded workspace tree search and safe
file reads, examines at most 64 source files, accepts up to 2 MiB of source,
and caps module entries at 4 KiB. It may read one additional file, bounded by
the existing 1 MiB safe-read limit, to detect the source cap. The lightweight
lexical filter skips comments, strings, and nested inline modules; it is not a
Rust parser and may omit declarations in macros or unusual formatting. The
reported conventional path candidates are advisory and may not exist; verify
them with a workspace read before use. A `#[path = ...]` declaration is
reported without conventional candidates. The map does not index functions or
other symbols and is not added to planner prompts. The default `context` output
and planner behavior are unchanged. See the
[bounded retrieval comparison](context-module-map-evaluation.md) for the
current hand-authored evaluation and its limits.

```sh
cargo run -- context --goal "Locate parser tokens" --repository-map --workspace "task workspace"
```

The adapter exposes the existing task workspace actions and task completion.
When an embedding host registers extension tools, the adapter can also advertise
those host-authorized declarations; the CLI itself registers none, so its default
behaviour is unchanged. Built-in actions pass through Krypper's existing workspace
containment, symlink, UTF-8, size, lifecycle, and action-budget checks; model output
cannot request arbitrary shell or process execution. Host-registered extensions are
trusted host handlers rechecked against their grant, schema and active turn at
dispatch; they do not add a model-controlled path to the Tools registry.
See [extension bridge safety](extension-bridge-safety.md) for the threat-model
and backward-compatibility notes.
The registered `offline_cargo_test` and `offline_read_only_exec` profiles are
denied by default. The CLI grants access only after explicit task opt-in and
the existing one-shot authorization path.
See [the opt-in Linux profile](offline-cargo-profile.md). See Ollama's
[tool-calling API example](https://github.com/ollama/ollama/blob/main/docs/capabilities/tool-calling.mdx)
for its local chat tool-call format.

### Guided per-user setup

Run `krypper setup` from any directory to choose a provider, search the offline
model catalog, enter credentials, review the selection, and save a default
model without a TUI.
The provider list accepts its displayed name or number and marks the saved
choice. Press Enter to keep the saved provider and model; catalog matches show
both the model name and ID. When a credential already exists, keeping it is
the default; replace it only when needed. Environment values still take
precedence over the encrypted saved value.
Cloudflare Workers AI direct and Cloudflare AI Gateway are separate choices.
The catalog is informational and offline; a model ID can also be entered
manually.

`krypper models` remains offline and uses the embedded snapshot or the user's
last refreshed cache. `krypper models --refresh` explicitly contacts the
configured public catalog feeds and atomically saves a verified cache.
Use `--refresh` to update optional reference prices and feature metadata when
the sources report them; the ordinary command makes no network requests.
Human-readable `krypper models` output groups rows into provider tables with
model names and IDs, context/output limits, declared tool/reasoning support,
modalities, reference prices, available features, and status. Unknown values
stay marked as unknown. `--provenance` also shows catalog origin, age, source
license, and retrieval metadata. `--json --provenance` emits
`{ "catalog": ..., "models": [...] }`, with optional per-model source IDs,
prices, and declared features. Plain `--json` retains its existing model-array
format. Missing catalog prices remain unknown; catalog rates support estimates
and do not represent provider invoices. See the
[provider routing migration guide](provider-routing-migration.md) for the
backward-compatible config changes.

Use `krypper setup --status` to show provider readiness, the selected route and
reason, and catalog provenance without printing credentials. Add `--json` for
a machine-readable status object.

Automatic model choice is per-user and opt-in:

```sh
krypper setup --routing-mode automatic
```

The saved provider/model remains the provider scope anchor. By default, route
choices stay within that provider. To permit routing across configured
providers, explicitly add `--allow-cross-provider`; this also authorizes
sending task context to those providers. Automatic choices require a current
successful probe for the exact provider, model, account configuration, and
credential revision, plus known tool-call support. Explicit `task --model` or
`task --provider` always selects that exact route.

Bounded fallback is a separate opt-in and needs an aggregate estimated-cost
ceiling:

```sh
krypper setup --routing-mode automatic --fallback-budget-usd 0.25
```

The CLI bounds a fallback chain to two additional routes and three provider
requests per task. Cost estimates use the maximum declared input and output
limits with catalog prices, so a missing price or token limit blocks fallback
before the first request. Only explicit rate-limit and server-error statuses
can trigger a switch; ambiguous or partial replies require reconciliation.
Each fallback task stores only its route, policy, and aggregate request/cost
counters under the user's private settings directory. Safe pause/resume requires
the routing snapshot to match exactly. `setup --reset` removes those task
routing records and the model catalog cache.

Credential prompts do not echo the value. Krypper stores credentials separately
from `config.toml`, in an authenticated encrypted envelope in `credentials.json`
in the user's Krypper configuration directory. The encryption key is held in
the current user's system credential vault: Secret Service on Linux, Keychain
on macOS, or Windows Credential Manager. The file and its parent directory keep
their private-permission checks. If the system vault is locked, missing, or
unavailable, saved credentials fail closed without a plaintext fallback. A
legacy plaintext file is migrated only after the user explicitly approves the
prompt in `krypper setup`; replacement is atomic, while old backups and
filesystem snapshots are outside Krypper's control. Credentials are never
written to the project workspace, printed, or logged.

Environment variables override each matching saved credential. A set but
invalid environment value does not fall back to the saved value. `--no-config`
ignores saved model defaults and saved credentials for one run; provider
environment variables remain available. Explicit `--model` or `--provider`
flags ignore the saved model/provider pair but can still use saved credentials.
`KRYPPER_CONFIG` overrides the model defaults read by `task`; `krypper setup`
always writes to the ordinary per-user configuration directory.

### Trusted provider profiles

To connect an endpoint that uses one of Krypper's existing protocols, create
`provider-profiles.json` in the ordinary per-user configuration directory
(`$XDG_CONFIG_HOME/krypper/` or `$HOME/.config/krypper/`). This file is never
read from `KRYPPER_CONFIG` or the task workspace. On Unix, keep it private with
mode `0600`; `krypper setup --status` reports the profile count, and
`krypper setup --reset` removes profiles, defaults, saved credentials, and local provider evidence.

Example OpenAI-compatible profile:

```json
{
  "version": 1,
  "profiles": [
    {
      "id": "my-worker",
      "label": "My Worker",
      "enabled": true,
      "protocol": "openai-compatible-chat",
      "endpoint": "https://example.invalid/v1/chat/completions",
      "credential_ref": "profile:my-worker:api-key"
    }
  ]
}
```

Profiles can use `openai-compatible-chat`, `openai-responses`,
`anthropic-messages`, `gemini-generate-content`, or `ollama-chat`. The first
three hosted endpoint fields are complete request URLs. Gemini uses an API
base ending at its version path, such as `https://example.invalid/v1beta`; the
adapter adds the validated model route. Ollama uses its server base, such as
`http://127.0.0.1:11434`; the adapter adds `/api/chat`. Endpoints must use
HTTPS or HTTP on a loopback host, and cannot contain userinfo, query strings,
fragments, or control characters. Proxies are disabled and redirects are not
followed.

Hosted profiles use the API key format already defined by their protocol
adapter. For `id: "my-worker"`, set `KRYPPER_PROVIDER_MY_WORKER_API_KEY` in the
environment, or save a value under that exact name in the separate per-user
`credentials.json`. Environment values override saved values. The profile
file stores only the fixed credential reference; it cannot define headers,
authentication schemes, aliases, endpoint templates, or code.

Select a profile explicitly with `--provider my-worker` and a model. A saved
`provider = "my-worker"` default in the existing model configuration is also
supported when it is in the ordinary per-user model configuration. A
`KRYPPER_CONFIG` override can select built-in providers but cannot select a
custom profile; use `--provider` or the per-user model configuration. Either
choice is the user's explicit opt-in to send task data to that profile.
`--no-config` ignores saved model defaults and saved credentials;
an explicit `--provider my-worker` still loads its per-user profile and can use
the profile's environment variable. Profile editing is file-based in this
phase; interactive setup management is planned for the later CLI/TUI phase.

The JSON schema is versioned and strict. It supports at most 16 profiles and
64 KiB, rejects unknown fields and built-in selector conflicts, and fails
before a provider request when a profile is invalid or disabled.

Use `krypper setup --status` to show the saved provider/model and whether a
credential file exists. It never displays credential values. Use
`krypper setup --reset` to confirm and remove the per-user defaults, provider
profiles, credential file, and local provider evidence. Removing a file does not erase copies retained
by filesystem snapshots or backups.

Setup offers a connection test only after saving the settings. If accepted, it
sends one short request using an empty temporary workspace. Hosted requests may
incur provider charges. Krypper records bounded local health, latency, typed
failure, rate-limit, and available token-usage evidence for that provider,
model, and configuration revision. A keyed fingerprint distinguishes account
or credential changes without writing credential values into the evidence.
Evidence expires for readiness decisions after one hour. It contains no prompt
or response text. This phase records the 429 rate-limit indicator but leaves
retry delay unknown. `setup --reset` removes the evidence and its local revision key.
Compatible providers can report token counts; other providers may leave them
unknown.

### Default model from a user configuration file (opt in)

Creating the file below is your opt-in: when it exists and you pass neither
`--model` nor `--provider`, `krypper task` uses its model selection. Manual
mode preserves the configured provider/model exactly; automatic mode uses the
saved provider as its scope anchor and reports a selected route and reason.
Without the file, nothing changes.

```toml
# ~/.config/krypper/config.toml
provider = "ollama"   # optional; built-in selector or enabled profile ID
model = "qwen3"       # required when the file sets anything
# routing_mode = "automatic"
# allow_cross_provider_data_transfer = "true"
# fallback_budget_micro_usd = "250000"
```

- **Location:** `$KRYPPER_CONFIG` (absolute path), else
  `$XDG_CONFIG_HOME/krypper/config.toml`, else `$HOME/.config/krypper/config.toml`.
  Krypper never reads a configuration file from a task workspace, so repository
  contents cannot choose where your task data is sent.
- **Precedence:** passing `--model` or `--provider` (alone or together) ignores
  the file entirely; the file's provider is never combined with a flag.
  `--no-config` ignores model defaults and saved credentials for one run.
- **Visibility:** when the file applies, Krypper prints
  `krypper: using model '...' (provider: ...) from <path>` to standard error
  before the task starts.
- **Format:** a small TOML subset: single-line, quoted `key = "value"`
  assignments, blank lines, and `#` comments. Basic and literal TOML strings are
  supported; decoded control characters are rejected. `provider`, `model`,
  `routing_mode`, `allow_cross_provider_data_transfer`, and
  `fallback_budget_micro_usd` are accepted; tables, multiline strings, duplicate
  or unknown keys, files over 16 KiB, and non-UTF-8 files are rejected. Routing
  settings require a model default. A
  present but invalid file fails the command with status 2 instead of being
  skipped.
- **Credentials:** never stored in this TOML file. Hosted providers read each
  required value from the environment first, then from the separate per-user
  `credentials.json` written by `krypper setup`. A hosted provider without the
  required values fails before any request.

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

`TaskSession::snapshot()` returns an immutable, on-demand view of the session,
task, and turn IDs and states. Each task snapshot includes the high-water value
for its combined lifecycle history and its metadata-only activity history.
These one-based sequence domains are independent and each is zero when empty;
reading a snapshot does not consume either history. Snapshots omit descriptions,
outcomes, prompts, provider data, workspace information, credentials, and secrets.

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
