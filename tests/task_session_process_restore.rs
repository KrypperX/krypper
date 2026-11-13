use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use uuid::Uuid;

#[path = "../examples/task_session_process_restore.rs"]
mod process_example;

const CHILD_MODE: &str = "KRYPPER_PROCESS_EXAMPLE_CHILD_MODE";
const CHILD_STATE_DIR: &str = "KRYPPER_PROCESS_EXAMPLE_STATE_DIR";
const CHILD_FILE: &str = "KRYPPER_PROCESS_EXAMPLE_FILE";
const CHILD_CONTENT: &str = "KRYPPER_PROCESS_EXAMPLE_CONTENT";
const CHILD_EXPECT_ERROR: &str = "KRYPPER_PROCESS_EXAMPLE_EXPECT_ERROR";
const FIRST_FILE: &str = "first-turn.txt";
const FIRST_CONTENT: &str = "checkpoint-fixture-payload-must-not-be-serialized";
const CHECKPOINT_FILE: &str = "task-session.checkpoint.json";

struct TemporaryState(PathBuf);

impl TemporaryState {
    fn new() -> io::Result<Self> {
        let path = env::temp_dir().join(format!("krypper process restore {}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn state_dir(&self) -> PathBuf {
        self.0.join("caller state")
    }
}

impl Drop for TemporaryState {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("failed to remove test-owned temporary state: {error}");
        }
    }
}

#[test]
fn process_example_child() {
    let Ok(mode) = env::var(CHILD_MODE) else {
        return;
    };
    let state_dir = env::var_os(CHILD_STATE_DIR).expect("child state directory is configured");
    let mut args = vec![OsString::from(mode.as_str()), state_dir];
    if mode == "resume" {
        args.push(env::var_os(CHILD_FILE).expect("resume file is configured"));
        args.push(env::var_os(CHILD_CONTENT).expect("resume content is configured"));
    }

    let original_directory = env::current_dir().expect("test process has a current directory");
    let result = process_example::run(args);
    assert_eq!(
        env::current_dir().expect("test process has a current directory"),
        original_directory
    );
    if env::var(CHILD_EXPECT_ERROR).is_ok_and(|value| value == "1") {
        assert!(
            result.is_err(),
            "the example unexpectedly overwrote existing state"
        );
    } else if let Err(error) = result {
        panic!("example child failed: {error}");
    }
}

#[test]
fn save_and_resume_in_separate_processes_preserves_lifecycle_and_caller_state() -> io::Result<()> {
    let temporary = TemporaryState::new()?;
    let state_dir = temporary.state_dir();
    let workspace = state_dir.join("workspace");
    let checkpoint_path = state_dir.join(CHECKPOINT_FILE);

    let save = run_child("save", &state_dir, None, false);
    assert_child_succeeded(&save);
    let saved_output = String::from_utf8_lossy(&save.stdout);
    let session_id = output_value(&saved_output, "session_id");
    let task_id = output_value(&saved_output, "task_id");
    let first_turn_id = output_value(&saved_output, "first_turn_id");
    assert_eq!(output_value(&saved_output, "task_state"), "running");
    assert_eq!(output_value(&saved_output, "first_turn_state"), "succeeded");
    assert_eq!(
        fs::read_to_string(workspace.join(FIRST_FILE))?,
        FIRST_CONTENT
    );

    let checkpoint_before_resume = fs::read(&checkpoint_path)?;
    assert!(!contains_bytes(
        &checkpoint_before_resume,
        FIRST_CONTENT.as_bytes()
    ));

    let second_file = "selected after restore.txt";
    let second_content = "created only by the fresh selected turn";
    let resume = run_child(
        "resume",
        &state_dir,
        Some((second_file, second_content)),
        false,
    );
    assert_child_succeeded(&resume);
    let resumed_output = String::from_utf8_lossy(&resume.stdout);
    assert_eq!(output_value(&resumed_output, "session_id"), session_id);
    assert_eq!(output_value(&resumed_output, "task_id"), task_id);
    assert_eq!(
        output_value(&resumed_output, "first_turn_id"),
        first_turn_id
    );
    assert_ne!(
        output_value(&resumed_output, "second_turn_id"),
        first_turn_id
    );
    assert_eq!(
        output_value(&resumed_output, "first_turn_state"),
        "succeeded"
    );
    assert_eq!(
        output_value(&resumed_output, "second_turn_state"),
        "succeeded"
    );
    assert_eq!(output_value(&resumed_output, "task_state"), "succeeded");

    assert_eq!(
        fs::read_to_string(workspace.join(FIRST_FILE))?,
        FIRST_CONTENT
    );
    assert_eq!(
        fs::read_to_string(workspace.join(second_file))?,
        second_content
    );
    assert_eq!(fs::read(&checkpoint_path)?, checkpoint_before_resume);
    assert!(!contains_bytes(
        &fs::read(&checkpoint_path)?,
        FIRST_CONTENT.as_bytes()
    ));

    let save_again = run_child("save", &state_dir, None, true);
    assert_child_succeeded(&save_again);
    assert_eq!(
        fs::read_to_string(workspace.join(FIRST_FILE))?,
        FIRST_CONTENT
    );
    assert_eq!(
        fs::read_to_string(workspace.join(second_file))?,
        second_content
    );
    assert_eq!(fs::read(&checkpoint_path)?, checkpoint_before_resume);

    let mut files: Vec<_> = fs::read_dir(&workspace)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()?;
    files.sort();
    assert_eq!(
        files,
        vec![OsString::from(FIRST_FILE), OsString::from(second_file)]
    );

    Ok(())
}

fn run_child(
    mode: &str,
    state_dir: &Path,
    selected_action: Option<(&str, &str)>,
    expect_error: bool,
) -> Output {
    let mut command = Command::new(env::current_exe().expect("integration test executable"));
    command
        .args(["--exact", "process_example_child", "--nocapture"])
        .current_dir(env::temp_dir())
        .env(CHILD_MODE, mode)
        .env(CHILD_STATE_DIR, state_dir)
        .env(CHILD_EXPECT_ERROR, if expect_error { "1" } else { "0" });
    if let Some((path, content)) = selected_action {
        command.env(CHILD_FILE, path).env(CHILD_CONTENT, content);
    }
    command.output().expect("launch child test process")
}

fn assert_child_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "child process failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_value<'a>(output: &'a str, key: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("child output omitted {key}: {output}"))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
