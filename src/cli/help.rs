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
        Some("models") => "usage: krypper models [--provider NAME] [--id MODEL_ID] [--json]",
        Some("task") => {
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--no-config] [--checkpoint-out PATH] [--resume PATH] [--activity-jsonl]"
        }
        Some("context") => "usage: krypper context [-h] --goal GOAL [--workspace PATH]",
        _ => "usage: krypper [-h] {show,list,write,task,models,context} ...",
    };
    let program = command.map_or_else(|| "krypper".to_owned(), |name| format!("krypper {name}"));
    writeln!(error, "{usage}")?;
    writeln!(error, "{program}: error: {message}")?;
    Ok(2)
}

pub(super) fn write_root_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper [-h] {show,list,write,task,models,context} ...\n\nLocal command-line entry point for Krypper.\n\npositional arguments:\n  {show,list,write,task,models,context}\n    show        display a UTF-8 text file\n    list        list immediate directory entries\n    write       create a new UTF-8 text file\n    task        run a caller-directed bounded task\n    models      browse offline model catalog\n    context     show bounded offline repository context for a goal\n\noptions:\n  -h, --help    show this help message and exit\n",
    )
}

pub(super) fn write_context_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        b"usage: krypper context [-h] --goal GOAL [--workspace PATH]\n\nshow bounded offline repository context for a goal\n\noptions:\n  --goal GOAL        literal goal terms used for repository search\n  --workspace PATH   use an existing directory as the search root\n  -h, --help         show this help message and exit\n\nThis command makes no model request and changes no workspace file.\nResults are untrusted advisory excerpts with path, line range, and content revision.\n",
    )
}

pub(super) fn write_task_help(output: &mut dyn Write) -> io::Result<()> {
    output.write_all(
        concat!(
            "usage: krypper task [-h] [--goal GOAL] --max-actions N [--workspace PATH] [--model NAME] [--provider NAME] [--no-config] [--checkpoint-out PATH] [--resume PATH]\n\n",
            "run a bounded task in the selected workspace or current directory\n\n",
            "options:\n  --goal GOAL          task goal (required unless --resume is used)\n",
            "  --max-actions N      required positive action limit for this invocation\n",
            "  --workspace PATH     use an existing directory as the captured task workspace\n",
            "  --model NAME         opt in to model-selected actions (defaults to local Ollama)\n",
            "  --provider NAME      choose ollama, gemini, openai, xai, anthropic, cloudflare, groq, openrouter, deepseek, zai, or mistral; requires --model\n",
            "  --no-config          ignore the user configuration file for this run\n",
            "  --checkpoint-out PATH write only when a completed turn reaches continue; file must be new\n",
            "  --resume PATH        restore one running task and start a fresh turn\n",
            "  --activity-jsonl    write read-only task activity positions as JSONL to stderr after each turn\n",
            "  --stream-events-jsonl  write versioned metadata-only stream events as JSONL to stderr after each turn\n",
            "  -h, --help           show this help message and exit\n\n",
            "Without --workspace, task captures the current directory as its workspace.\n",
            "With --workspace, the selected directory is validated before task history starts;\n",
            "the process current directory is not changed.\n",
            "Without --model or --provider, task remains caller-directed and makes no model requests,\n",
            "unless the user configuration file (see README) sets a default model; --no-config ignores it.\n",
            "Without --model, task remains caller-directed if --provider is also omitted.\n",
            "--model NAME without --provider uses local Ollama at 127.0.0.1:11434. Ollama needs no API key.\n",
            "Each hosted provider requires explicit --provider selection and sends HTTPS requests only\n",
            "to its fixed API endpoint; no custom API URL is accepted. Required credentials:\n",
            "  gemini: GEMINI_API_KEY -> https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent\n",
            "  Gemini model name must be one safe path component; no custom URL is accepted.\n",
            "  openai: OPENAI_API_KEY -> https://api.openai.com/v1/responses\n",
            "  xai: XAI_API_KEY -> https://api.x.ai/v1/responses\n",
            "  anthropic: ANTHROPIC_API_KEY -> https://api.anthropic.com/v1/messages\n",
            "  cloudflare: CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN ->\n",
            "    https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/v1/chat/completions\n",
            "  groq: GROQ_API_KEY -> https://api.groq.com/openai/v1/chat/completions\n",
            "  openrouter: OPENROUTER_API_KEY -> https://openrouter.ai/api/v1/chat/completions\n",
            "  deepseek: DEEPSEEK_API_KEY -> https://api.deepseek.com/chat/completions\n",
            "  zai: ZAI_API_KEY -> https://api.z.ai/api/paas/v4/chat/completions\n",
            "  mistral: MISTRAL_API_KEY -> https://api.mistral.ai/v1/chat/completions\n",
            "OpenRouter may select an upstream provider for the requested model even when fallback\n",
            "providers are disabled in the request.\n",
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
            "Ollama sends the task description and current-turn observations to http://127.0.0.1:11434/api/chat.\n",
            "Read observations can include workspace file text. Ollama must be running locally\n",
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
