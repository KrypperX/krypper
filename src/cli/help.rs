//! Help text, usage lines and the shared argument helpers every subcommand uses.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};

pub(super) fn starts_with_option(argument: &OsStr) -> bool {
    argument.to_string_lossy().starts_with('-')
}

pub(super) fn contains_help_before_terminator(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .take_while(|argument| argument.as_os_str() != OsStr::new("--"))
        .any(|argument| argument == "-h" || argument == "--help")
}

pub(super) fn unrecognized_arguments(arguments: &[OsString], skip: usize) -> String {
    let rest = arguments
        .iter()
        .skip(skip)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!("unrecognized arguments: {rest}")
}

pub(super) fn usage_error(
    error: &mut dyn Write,
    command: Option<&str>,
    message: &str,
) -> io::Result<i32> {
    let usage = match command {
        Some("show") => "usage: krypper show [-h] path",
        Some("list") => "usage: krypper list [-h] [path]",
        Some("write") => "usage: krypper write [-h] [--stdin] path ...",
        Some("models") => "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json] [--provenance] [--refresh]",
        Some("setup") => "usage: krypper setup [--status [--json] | --reset | --routing-mode manual|automatic [--allow-cross-provider] [--fallback-budget-usd USD] | --permission-mode ask|risk-based|full-access | --offline-process --toolchain-root PATH --registry-root PATH]",
        Some("task") => {
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--permission-mode ask|risk-based|full-access] [--sandbox-profile offline] [--verified --verify-file PATH ...] [--no-config] [--checkpoint-out PATH] [--resume PATH] [--activity-jsonl] [--stream-events-jsonl] [--model-usage-jsonl] [--repository-context-json PATH]"
        }
        Some("context") => {
            "usage: krypper context [-h] --goal GOAL [--workspace PATH] [--repository-map]"
        }
        _ => "usage: krypper [-h] {show,list,write,task,models,context,setup} ...",
    };
    let program = command.map_or_else(|| "krypper".to_owned(), |name| format!("krypper {name}"));
    writeln!(error, "{usage}")?;
    writeln!(error, "{program}: error: {message}")?;
    Ok(2)
}

pub(super) fn write_root_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
    b"usage: krypper [-h] {show,list,write,task,models,context,setup} ...\n\nLocal command-line entry point for Krypper.\n\npositional arguments:\n  {show,list,write,task,models,context,setup}\n    show        display a UTF-8 text file\n    list        list immediate directory entries\n    write       create a new UTF-8 text file\n    task        run a caller-directed bounded task\n    models      browse offline model catalog\n    context     show bounded offline repository context for a goal\n    setup       configure a provider, model, and permission mode for this user\n\noptions:\n  -h, --help    show this help message and exit\n",
    )
}

pub(super) fn write_setup_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper setup [--status [--json] | --reset | --routing-mode manual|automatic [--allow-cross-provider] [--fallback-budget-usd USD] | --permission-mode ask|risk-based|full-access | --offline-process --toolchain-root PATH --registry-root PATH]\n\nConfigure the default provider, model, permission mode, or host-owned offline process roots for this user.\n\noptions:\n  --status                    show saved model, permission mode, routing, readiness, and catalog provenance\n  --json                      emit machine-readable status (only with --status)\n  --routing-mode MODE         save manual or automatic model routing\n  --permission-mode MODE      save ask, risk-based, or full-access for this user (default: ask)\n  --offline-process           save preprovisioned Linux process roots for the opt-in offline profile\n  --toolchain-root PATH       existing toolchain directory containing real cargo and rustc binaries\n  --registry-root PATH        existing offline Cargo registry directory\n  --allow-cross-provider      explicitly allow task data to move between providers\n  --fallback-budget-usd USD   opt in to bounded fallback with an aggregate estimate ceiling\n  --reset                     ask before removing saved settings and local provider evidence\n  -h, --help                  show this help message and exit\n\nThe offline process roots must be existing, absolute, disjoint directories. Setup validates and stores them; it does not install or download a toolchain or registry. `krypper task` uses them only when `--sandbox-profile offline` is selected.\nFull Access is a preference for the Linux direct-argv host process action. Every task requires a trusted controlling-terminal confirmation before provider startup; missing terminal or a declined prompt fails closed. The process uses current OS-user authority and has no Krypper sandbox, workspace boundary, or egress restriction.\nRun `krypper setup` in an interactive terminal to choose a provider and search the offline model catalog.\nHosted provider credentials are entered without echo and encrypted in the user's Krypper config directory with a key stored in the system credential vault. A locked or unavailable vault fails closed. Existing plaintext credential files require explicit approval in `krypper setup` before migration.\nEnvironment credential variables override saved values. `KRYPPER_CONFIG` overrides saved model defaults, but never permission mode or offline process roots.\nA connection test sends one short prompt only after you explicitly agree; it may incur provider charges. Its bounded local evidence stores health, latency, typed failure, rate-limit, and available token-usage facts, but not prompts, responses, or credentials.\nAutomatic routing needs a fresh successful probe and known tool-call support. Fallback also needs known model pricing and context/output limits; unknown facts stop selection before a provider request.\n",
    )
}

pub(super) fn write_context_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper context [-h] --goal GOAL [--workspace PATH] [--repository-map]\n\nshow bounded offline repository context for a goal\n\noptions:\n  --goal GOAL        literal goal terms used for repository search\n  --workspace PATH   use an existing directory as the search root\n  --repository-map   show bounded Rust module declarations and path candidates\n  -h, --help         show this help message and exit\n\nThis command makes no model request and changes no workspace file.\nResults are untrusted advisory data; verify map path candidates through workspace reads.\n",
    )
}

pub(super) fn write_task_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        concat!(
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--permission-mode ask|risk-based|full-access] [--sandbox-profile offline] [--verified --verify-file PATH ...] [--no-config] [--checkpoint-out PATH] [--resume PATH] [--activity-jsonl] [--stream-events-jsonl] [--model-usage-jsonl] [--repository-context-json PATH] [--observation-context-budget-bytes N]\n\n",
            "run a bounded task in the selected workspace or current directory\n\n",
            "options:\n  --goal GOAL          task goal (required unless --resume is used)\n",
            "  --permission-mode MODE use ask, risk-based, or full-access for this task; defaults to this user's saved mode\n",
            "  --sandbox-profile offline opt in to the configured Linux offline process sandbox for this task\n",
            "  --max-actions N      required positive action limit for this invocation\n",
            "  --workspace PATH     use an existing directory as the captured task workspace\n",
            "  --model NAME         opt in to model-selected actions or plans (defaults to local Ollama)\n",
        )
        .as_bytes(),
    )?;
    write_provider_selector_help(output)?;
    output.write_all(
        concat!(
            "  --verified           use a planned local Ollama task with independent file checks and one correction\n",
            "  --verify-file PATH   workspace-relative file required for success; repeat with --verified (up to 32)\n",
            "  --no-config          ignore saved model defaults, credentials, and permission mode for this run\n",
            "  --checkpoint-out PATH write only when a completed turn reaches continue; file must be new\n",
            "  --resume PATH        restore one running task and start a fresh turn\n",
            "  --activity-jsonl    write read-only task activity positions as JSONL to stderr after each turn\n",
            "  --stream-events-jsonl  write versioned metadata-only stream events as JSONL to stderr after each turn\n",
            "  --model-usage-jsonl emit provider token counts and decision latency as prefixed JSON records to stderr\n",
            "  --repository-context-json PATH add bounded JSON from \x60krypper context\x60 as untrusted advisory data (OpenAI-compatible providers only)\n",
            "  --observation-context-budget-bytes N cap model-facing observation JSON and report any text compaction (1..2097152; model-backed tasks only)\n",
            "  -h, --help           show this help message and exit\n\n",
            "Without --workspace, task captures the current directory as its workspace.\n",
            "With --workspace, the selected directory is validated before task history starts;\n",
            "Permission defaults are stored per user; --permission-mode overrides one task. Offline process profiles are disabled unless --sandbox-profile offline is selected and a valid private per-user toolchain/registry setting exists. This setting ignores workspace files and KRYPPER_CONFIG. Ask and Risk-based require exact controlling-terminal approval for every direct-argv action; Ask prompts for fixed Cargo while Risk-based permits only that fixed profile when configured. Missing settings, overlap with the workspace, missing terminal approval, or unsupported Linux controls fail closed. Resume requires the explicit profile flag again and receives fresh one-shot grants. The offline profile confines Krypper process actions only; model/provider requests still use the selected provider and may send task data.\n",
            "Full Access is Linux-only. Every task needs a trusted controlling-terminal yes/no confirmation before provider startup; a saved preference or --permission-mode alone does not authorize execution. The registered direct-argv host process runs as the current OS user with inherited environment, filesystem, and network access. Its output may be sent to the selected AI provider in the next request. Krypper applies no process sandbox, workspace boundary, or egress restriction to it; outer OS/container policy still applies and root is not granted. If no controlling terminal is available or the user declines, the task does not start.\n",
            "the process current directory is not changed.\n",
            "Without --model or --provider, task remains caller-directed and makes no model requests,\n",
            "unless the user configuration file (see README) sets a default model; --no-config ignores saved defaults and credentials.\n",
            "Without --model, task remains caller-directed if --provider is also omitted.\n",
            "Observation compaction is opt-in. It preserves structured outcome metadata and marks omitted text with its original byte count and SHA-256 digest; raw TaskSession observations are unchanged. The byte budget covers observation JSON only, not the full provider request.\n",
            "--model NAME without --provider uses local Ollama at 127.0.0.1:11434. Ollama needs no API key.\n",
            "Hosted providers require explicit --provider selection or a saved user default.\n",
            "Built-ins use fixed endpoints; trusted per-user profiles use their configured endpoint.\n",
            "Provider profiles are documented in the CLI reference. Required built-in credentials:\n",
        )
        .as_bytes(),
    )?;
    write_provider_credential_help(output)?;
    output.write_all(
        concat!(
            "Hosted requests send the task goal and observations, which can include workspace file text,\n",
            "to that provider and may incur provider charges.\n",
            "--workspace PATH works with --model NAME or --provider NAME and uses the same captured workspace.\n",
            "Without --checkpoint-out, task can continue across turns up to the action limit.\n",
            "With --checkpoint-out, a new checkpoint is written only after a completed\n",
            "turn reaches the safe continue boundary.\n",
            "A failed or interrupted turn may already have changed workspace files.\n",
            "The most recent checkpoint stays unchanged or the requested new checkpoint\n",
            "is absent.\n",
            "Inspect and reconcile the current workspace before resuming from an older\n",
            "checkpoint after such a turn.\n",
            "Checkpoint files retain lifecycle metadata, not prior observations or file contents.\n",
            "Resume from the newest checkpoint and use a new --checkpoint-out path for each pause.\n",
            "Verified mode requires --model (or a saved model default) with the local Ollama provider,\n",
            "--max-actions of at least 2, and one or more --verify-file paths (up to 192 UTF-8 bytes).\n",
            "Paths must stay inside the task workspace. It plans through the\n",
            "existing Harness and TaskSession; a check passes only after a successful operation on that exact path\n",
            "in the current turn. It does not verify file contents. Missing evidence is unknown,\n",
            "and model completion text is not proof.\n",
            "After a plan, enter complete, continue, or cancel to control the task.\n",
            "A turn with failed or unknown file checks, or a confirmed complete offline test failure, may get one correction per invocation\n",
            "when at least five Harness action slots remain (six for a confirmed test failure). Continue or cancel do not trigger correction.\n",
            "--max-actions counts Harness workspace and task-control actions, not Ollama requests.\n",
            "Denied or failed operations, cancellation, and exhausted turns do not trigger correction.\n",
            "With --checkpoint-out, continue after the initial turn defers correction. A correction already run uses this invocation's allowance.\n",
            "Resume with --verified and repeat\n",
            "all --verify-file paths and --max-actions. The new invocation gets a fresh action budget.\n",
            "Ollama sends the task, current-turn observations, and applicable root/nested AGENTS.md guidance to\n",
            "http://127.0.0.1:11434/api/chat. Read observations can include workspace file text. AGENTS.md guidance may include other workspace text.\n",
            "Trust the local Ollama service receiving this data.\n",
            "Ollama must be running locally\n",
            "with the named model available. Only supported task actions and completion are exposed.\n\n",
            "Enter one action per line on standard input:\n",
            "  read PATH\n  read-range PATH START_LINE MAX_LINES\n",
            "  list [PATH]\n  mkdir PATH\n  create PATH TEXT\n",
            "  replace PATH MATCH_TEXT => REPLACEMENT_TEXT\n",
            "  search DIRECTORY QUERY\n  search-tree DIRECTORY QUERY\n",
            "  continue\n  complete [RESULT]\n  cancel\n\n",
            "PATH may be unquoted as before or a JSON double-quoted string; JSON escapes apply, with \\xNN for non-UTF-8 Unix bytes.\n",
            "Search results print reusable quoted path tokens. Text after PATH keeps its existing syntax.\n",
            "mkdir creates one directory and requires its parent to exist.\n",
            "read-range uses one-based lines, returns at most 200 lines and 64 KiB, and reports the actual range.\n",
            "replace requires one non-empty exact match; it prints only path and byte-count metadata.\n",
            "search returns relative paths and line numbers without matched text.\n",
            "search-tree does the same recursively with 4 KiB queries, 1 MiB files, 16 MiB total,\n",
            "10,000 locations, depth 16, and 4,096 directory entries at most.\n",
            "continue ends this turn and starts another while action budget remains.\n",
            "complete ends the task successfully. Invalid input, end-of-input, action failures,\n",
            "and budget exhaustion fail the task. Explicit cancellation reports a cancelled\n",
            "task and exits with status 130.\n",
        )
        .as_bytes(),
    )
}

fn write_provider_selector_help(output: &mut dyn Write) -> io::Result<()> {
    let mut selectors = crate::brain::provider_descriptors()
        .iter()
        .map(|descriptor| descriptor.selector)
        .collect::<Vec<_>>();
    let last = selectors.pop().expect("built-in providers are not empty");
    let selectors = format!("{}, or {last}", selectors.join(", "));
    writeln!(
        output,
        "  --provider NAME      choose {selectors} or an enabled per-user profile; requires --model"
    )
}

fn write_provider_credential_help(output: &mut dyn Write) -> io::Result<()> {
    use crate::brain::{provider_descriptors, AuthFieldRole, ProviderEndpoint};

    for descriptor in provider_descriptors()
        .iter()
        .filter(|descriptor| !descriptor.auth_fields.is_empty())
    {
        if descriptor
            .auth_field(AuthFieldRole::CloudflareAccountId)
            .is_some()
        {
            let mut fields = descriptor
                .auth_fields
                .iter()
                .filter(|field| field.required)
                .map(|field| field.name.to_owned())
                .collect::<Vec<_>>();
            let gateway = descriptor.auth_field(AuthFieldRole::CloudflareGatewayId);
            if let Some(gateway) = gateway {
                fields.push(format!("optional {}", gateway.name));
            }
            write!(output, "  {}: {}", descriptor.selector, fields.join(" + "))?;
            if let Some(gateway) = gateway {
                writeln!(
                    output,
                    " -> {}",
                    gateway
                        .help_note
                        .expect("gateway help includes routing behavior")
                )?;
            } else if let Some(url) = descriptor.endpoint.help_url() {
                writeln!(output, " ->\n    {url}")?;
            }
            continue;
        }

        let api_key = descriptor
            .auth_field(AuthFieldRole::ApiKey)
            .expect("hosted providers declare an API key");
        let url = match descriptor.endpoint {
            ProviderEndpoint::Fixed(_) | ProviderEndpoint::GeminiModelPath { .. } => descriptor
                .endpoint
                .help_url()
                .expect("hosted provider has a URL"),
            _ => unreachable!("non-Cloudflare providers use fixed or model-path endpoints"),
        };
        writeln!(
            output,
            "  {}: {} -> {url}",
            descriptor.selector, api_key.name
        )?;
        for field in descriptor
            .auth_fields
            .iter()
            .filter(|field| !field.required)
        {
            if let Some(note) = field.help_note {
                writeln!(output, "    optional {} {note}", field.name)?;
            }
        }
        if let Some(note) = descriptor.help_note {
            writeln!(output, "  {note}")?;
        }
    }
    for note in crate::brain::provider_descriptors()
        .iter()
        .flat_map(|descriptor| descriptor.postscript)
    {
        writeln!(output, "{note}")?;
    }
    Ok(())
}

pub(super) fn write_show_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper show [-h] path\n\ndisplay a UTF-8 text file\n\npositional arguments:\n  path        relative path inside the current directory\n\noptions:\n  -h, --help  show this help message and exit\n",
    )
}

pub(super) fn write_list_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper list [-h] [path]\n\nlist immediate directory entries\n\npositional arguments:\n  path        relative directory inside the current directory\n\noptions:\n  -h, --help  show this help message and exit\n",
    )
}

pub(super) fn write_write_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper write [-h] [--stdin] path ...\n\ncreate a new UTF-8 text file\n\npositional arguments:\n  path        relative file path inside the current directory\n  text        text to write\n\noptions:\n  -h, --help  show this help message and exit\n  --stdin     read text from standard input (up to 1 MiB)\n",
    )
}
