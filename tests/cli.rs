use krypper::task_lifecycle::{TaskSession, TaskState};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

const MAX_FILE_SIZE: usize = 1024 * 1024;
const MAX_TASK_ACTION_LINE_BYTES: usize = MAX_FILE_SIZE + 4096;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("krypper-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command(cwd: Option<&Path>, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_krypper"));
    if let Some(directory) = cwd {
        command.current_dir(directory);
    }
    command.args(args);
    command
}

fn run(cwd: Option<&Path>, args: &[&str]) -> Output {
    command(cwd, args).output().expect("run Krypper")
}

fn run_with_input(cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child: Child = command(Some(cwd), args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Krypper");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write test input");
    child.wait_with_output().expect("wait for Krypper")
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy)]
enum TaskCliStream {
    Stdout,
    Stderr,
}

enum TaskCliOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

fn read_task_cli_until_prompt(
    receiver: &Receiver<TaskCliOutput>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    action: usize,
    expected_output_lines: usize,
) {
    let prompt = format!("action {action}/6> ");
    while !stderr.ends_with(prompt.as_bytes())
        || stdout.iter().filter(|byte| **byte == b'\n').count() < expected_output_lines
    {
        let message = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("receive CLI output before the next action prompt");
        match message {
            TaskCliOutput::Stdout(bytes) => stdout.extend(bytes),
            TaskCliOutput::Stderr(bytes) => stderr.extend(bytes),
        }
    }

    let expected_prompts = (1..=action)
        .map(|number| format!("action {number}/6> "))
        .collect::<String>();
    assert_eq!(String::from_utf8_lossy(stderr), expected_prompts);
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_lifecycle_transitions(output: &str, terminal_state: &str) {
    let task_history_start = output
        .find("Task lifecycle history (metadata only):")
        .unwrap();
    let turn_history_start = output
        .find("Turn lifecycle history (metadata only):")
        .unwrap();
    let activity_history_start = output.find("Activity history (metadata only):").unwrap();

    assert!(task_history_start < turn_history_start);
    assert!(turn_history_start < activity_history_start);
    assert_eq!(
        &output[task_history_start..turn_history_start],
        format!(
            "Task lifecycle history (metadata only):\n\
- sequence 1: task_transition pending -> running\n\
- sequence 2: task_transition running -> {terminal_state}\n"
        )
    );
    assert_eq!(
        &output[turn_history_start..activity_history_start],
        format!(
            "Turn lifecycle history (metadata only):\n\
- sequence 1: turn_transition pending -> running\n\
- sequence 2: turn_transition running -> {terminal_state}\n"
        )
    );
}

#[test]
fn help_prints_usage_and_succeeds() {
    let output = run(None, &["--help"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("usage: krypper"));
    assert!(stdout(&output).contains("{show,list,write,task}"));
}

#[test]
fn missing_command_prints_usage_and_fails() {
    let output = run(None, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("usage: krypper"));
    assert!(stderr(&output).contains("{show,list,write,task}"));
}

#[test]
fn unsupported_argument_prints_error_and_usage() {
    let output = run(None, &["--unsupported"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("usage: krypper"));
    assert!(stderr(&output).contains("error: unrecognized arguments: --unsupported"));
}

#[test]
fn help_and_option_terminators_follow_the_command_parser_rules() {
    let directory = TempDir::new();
    fs::write(directory.path().join("-note.txt"), "leading option\n").unwrap();

    let show = run(Some(directory.path()), &["show", "--", "-note.txt"]);
    let help = run(Some(directory.path()), &["write", "--stdin", "--help"]);
    let write = run(
        Some(directory.path()),
        &["write", "--", "-created.txt", "-draft"],
    );

    assert!(show.status.success());
    assert_eq!(stdout(&show), "leading option\n");
    assert!(help.status.success());
    assert!(stdout(&help).contains("usage: krypper write"));
    assert!(help.stderr.is_empty());
    assert!(write.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("-created.txt")).unwrap(),
        "-draft"
    );
}

#[test]
fn write_walks_every_repeated_directory_component() {
    let directory = TempDir::new();
    fs::create_dir_all(directory.path().join("nested/nested/nested")).unwrap();
    let output = run(
        Some(directory.path()),
        &["write", "nested/nested/nested/note.txt", "text"],
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("nested/nested/nested/note.txt")).unwrap(),
        "text"
    );
}

#[test]
fn list_json_quotes_non_ascii_names_as_ascii() {
    let directory = TempDir::new();
    fs::write(directory.path().join("東京"), "").unwrap();
    fs::write(directory.path().join("café"), "").unwrap();
    let output = run(Some(directory.path()), &["list"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "\"caf\\u00e9\"\n\"\\u6771\\u4eac\"\n");
}

#[cfg(unix)]
#[test]
fn list_preserves_python_surrogate_escaping_for_non_utf8_names() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let directory = TempDir::new();
    fs::write(
        directory
            .path()
            .join(OsString::from_vec(b"bad-\xff".to_vec())),
        "",
    )
    .unwrap();
    let output = run(Some(directory.path()), &["list"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "\"bad-\\udcff\"\n");
}

#[test]
fn write_missing_path_and_text_errors_print_usage_and_exit_two() {
    let directory = TempDir::new();
    let missing_path = run(Some(directory.path()), &["write"]);
    let missing_text = run(Some(directory.path()), &["write", "note.txt"]);

    assert_eq!(missing_path.status.code(), Some(2));
    assert!(stderr(&missing_path).contains("usage: krypper write"));
    assert!(stderr(&missing_path).contains("the following arguments are required: path"));
    assert_eq!(missing_text.status.code(), Some(2));
    assert!(stderr(&missing_text).contains("usage: krypper write"));
    assert!(stderr(&missing_text).contains("text is required"));
    assert!(!directory.path().join("note.txt").exists());
}

#[test]
fn show_prints_utf8_file_contents_without_adding_output() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "Hello, café.\n").unwrap();
    let output = run(Some(directory.path()), &["show", "note.txt"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "Hello, café.\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn show_rejects_absolute_and_escaping_paths() {
    let directory = TempDir::new();
    fs::write(directory.path().join("outside.txt"), "text").unwrap();
    let inside = directory.path().join("inside");
    fs::create_dir(&inside).unwrap();

    let absolute = run(
        Some(&inside),
        &[
            "show",
            directory.path().join("outside.txt").to_str().unwrap(),
        ],
    );
    let traversal = run(Some(&inside), &["show", "../outside.txt"]);
    assert_eq!(absolute.status.code(), Some(2));
    assert!(stderr(&absolute).contains("absolute paths are not allowed"));
    assert_eq!(traversal.status.code(), Some(2));
    assert!(stderr(&traversal).contains("path must stay inside the current directory"));
}

#[cfg(unix)]
#[test]
fn show_follows_only_symlinks_that_stay_inside_the_current_directory() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "inside\n").unwrap();
    symlink("note.txt", directory.path().join("alias.txt")).unwrap();
    symlink(
        directory.path().join("note.txt"),
        directory.path().join("absolute-alias.txt"),
    )
    .unwrap();
    let outside = TempDir::new();
    fs::write(outside.path().join("secret.txt"), "outside").unwrap();
    symlink(outside.path(), directory.path().join("escape")).unwrap();

    let inside = run(Some(directory.path()), &["show", "alias.txt"]);
    let absolute_inside = run(Some(directory.path()), &["show", "absolute-alias.txt"]);
    let escape = run(Some(directory.path()), &["show", "escape/secret.txt"]);
    assert!(inside.status.success());
    assert_eq!(stdout(&inside), "inside\n");
    assert!(absolute_inside.status.success());
    assert_eq!(stdout(&absolute_inside), "inside\n");
    assert_eq!(escape.status.code(), Some(2));
    assert!(stderr(&escape).contains("path must stay inside the current directory"));
}

#[test]
fn show_rejects_missing_directories_invalid_utf8_and_oversized_files() {
    let directory = TempDir::new();
    fs::create_dir(directory.path().join("folder")).unwrap();
    fs::write(directory.path().join("invalid.txt"), [0xff]).unwrap();
    fs::write(
        directory.path().join("large.txt"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .unwrap();

    let missing = run(Some(directory.path()), &["show", "missing.txt"]);
    let not_file = run(Some(directory.path()), &["show", "folder"]);
    let invalid = run(Some(directory.path()), &["show", "invalid.txt"]);
    let large = run(Some(directory.path()), &["show", "large.txt"]);
    assert!(stderr(&missing).contains("file not found"));
    assert!(stderr(&not_file).contains("not a regular file"));
    assert!(stderr(&invalid).contains("not valid UTF-8"));
    assert!(stderr(&large).contains("file exceeds 1 MiB"));
    for result in [&missing, &not_file, &invalid, &large] {
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(result).contains("usage: krypper show"));
    }
}

#[test]
fn show_accepts_a_file_at_the_one_mib_limit() {
    let directory = TempDir::new();
    let contents = vec![b'x'; 1024 * 1024];
    fs::write(directory.path().join("limit.txt"), &contents).unwrap();
    let output = run(Some(directory.path()), &["show", "limit.txt"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, contents);
}

#[test]
fn list_defaults_to_current_directory_and_includes_hidden_names_in_sorted_order() {
    let directory = TempDir::new();
    for name in ["zeta", ".hidden", "alpha"] {
        fs::write(directory.path().join(name), "not listed").unwrap();
    }
    let output = run(Some(directory.path()), &["list"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "\".hidden\"\n\"alpha\"\n\"zeta\"\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn standalone_list_keeps_output_for_more_than_1024_entries() {
    let directory = TempDir::new();
    for index in 0..1025 {
        fs::write(directory.path().join(format!("entry-{index:04}")), "").unwrap();
    }

    let output = run(Some(directory.path()), &["list"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let names = stdout(&output).lines().collect::<Vec<_>>();
    assert_eq!(names.len(), 1025);
    assert_eq!(names.first(), Some(&"\"entry-0000\""));
    assert_eq!(names.last(), Some(&"\"entry-1024\""));
}

#[test]
fn list_shows_only_immediate_names_and_escapes_control_characters() {
    let directory = TempDir::new();
    let nested = directory.path().join("nested");
    fs::create_dir_all(nested.join("child")).unwrap();
    fs::write(nested.join("file.txt"), "secret contents").unwrap();
    fs::write(nested.join("line\nbreak"), "").unwrap();
    fs::write(nested.join("tab\tname"), "").unwrap();

    let output = run(Some(directory.path()), &["list", "nested/"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        "\"child\"\n\"file.txt\"\n\"line\\nbreak\"\n\"tab\\tname\"\n"
    );
    assert!(!stdout(&output).contains("secret contents"));
}

#[cfg(unix)]
#[test]
fn list_contains_selected_symlinks_but_does_not_follow_symlink_entries() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    let selected = directory.path().join("selected");
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("shown.txt"), "").unwrap();
    symlink("selected", directory.path().join("selected-link")).unwrap();
    let outside = TempDir::new();
    fs::write(outside.path().join("secret.txt"), "").unwrap();
    symlink(outside.path(), directory.path().join("linked-entry")).unwrap();

    let selected_output = run(Some(directory.path()), &["list", "selected-link"]);
    let root_output = run(Some(directory.path()), &["list"]);
    assert_eq!(stdout(&selected_output), "\"shown.txt\"\n");
    assert!(stdout(&root_output).contains("\"linked-entry\""));
    assert!(!stdout(&root_output).contains("secret.txt"));
}

#[test]
fn list_rejects_missing_nondirectory_absolute_and_escaping_paths() {
    let directory = TempDir::new();
    fs::write(directory.path().join("file.txt"), "").unwrap();
    let missing = run(Some(directory.path()), &["list", "missing"]);
    let not_directory = run(Some(directory.path()), &["list", "file.txt"]);
    let absolute = run(
        Some(directory.path()),
        &["list", directory.path().to_str().unwrap()],
    );
    let outside = run(Some(directory.path()), &["list", ".."]);
    assert!(stderr(&missing).contains("file not found"));
    assert!(stderr(&not_directory).contains("not a directory"));
    assert!(stderr(&absolute).contains("absolute paths are not allowed"));
    assert!(stderr(&outside).contains("path must stay inside the current directory"));
}

#[cfg(unix)]
#[test]
fn list_rejects_a_symlink_escape() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    let outside = TempDir::new();
    symlink(outside.path(), directory.path().join("escape")).unwrap();
    let output = run(Some(directory.path()), &["list", "escape"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("path must stay inside the current directory"));
}

#[test]
fn write_creates_utf8_text_and_joins_unquoted_words() {
    let directory = TempDir::new();
    let output = run(
        Some(directory.path()),
        &["write", "note.txt", "-draft", "with", "spaces", "café"],
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "-draft with spaces café"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn write_stdin_creates_multiline_utf8_and_accepts_exactly_one_mib() {
    let directory = TempDir::new();
    let contents = "First line\nCafé 東京\n";
    let output = run_with_input(
        directory.path(),
        &["write", "--stdin", "note.txt"],
        contents.as_bytes(),
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        contents
    );

    let exact = "é".repeat(1024 * 1024 / 2);
    let exact_output = run_with_input(
        directory.path(),
        &["write", "--stdin", "limit.txt"],
        exact.as_bytes(),
    );
    assert!(exact_output.status.success());
    assert_eq!(
        fs::metadata(directory.path().join("limit.txt"))
            .unwrap()
            .len(),
        1024 * 1024
    );
}

#[test]
fn write_stdin_rejects_oversize_invalid_utf8_and_extra_text_before_creation() {
    let directory = TempDir::new();
    let too_large = run_with_input(
        directory.path(),
        &["write", "--stdin", "large.txt"],
        &vec![b'x'; 1024 * 1024 + 1],
    );
    let invalid = run_with_input(
        directory.path(),
        &["write", "--stdin", "invalid.txt"],
        b"bad \xff",
    );
    let extra = run_with_input(
        directory.path(),
        &["write", "--stdin", "extra.txt", "unexpected"],
        b"",
    );
    assert!(stderr(&too_large).contains("stdin exceeds 1 MiB"));
    assert!(stderr(&invalid).contains("stdin is not valid UTF-8"));
    assert!(stderr(&extra).contains("extra text is not allowed with --stdin"));
    for result in [&too_large, &invalid, &extra] {
        assert_eq!(result.status.code(), Some(2));
        assert!(stderr(result).contains("usage: krypper write"));
    }
    for name in ["large.txt", "invalid.txt", "extra.txt"] {
        assert!(!directory.path().join(name).exists());
    }
}

#[test]
fn write_creates_in_existing_nested_parent_and_refuses_existing_targets() {
    let directory = TempDir::new();
    fs::create_dir(directory.path().join("nested")).unwrap();
    let nested = run(
        Some(directory.path()),
        &["write", "nested/note.txt", "nested text"],
    );
    assert!(nested.status.success());
    assert_eq!(
        fs::read_to_string(directory.path().join("nested/note.txt")).unwrap(),
        "nested text"
    );

    let existing = run(
        Some(directory.path()),
        &["write", "nested/note.txt", "replacement"],
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("nested/note.txt")).unwrap(),
        "nested text"
    );
    assert_eq!(existing.status.code(), Some(2));
    assert!(stderr(&existing).contains("file already exists"));
}

#[test]
fn write_rejects_absolute_and_outside_paths_without_changing_outside_files() {
    let directory = TempDir::new();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside = directory.path().join("outside.txt");
    fs::write(&outside, "keep").unwrap();

    let absolute = run(
        Some(&workspace),
        &["write", outside.to_str().unwrap(), "replacement"],
    );
    let traversal = run(
        Some(&workspace),
        &["write", "../outside.txt", "replacement"],
    );
    assert!(stderr(&absolute).contains("absolute paths are not allowed"));
    assert!(stderr(&traversal).contains("path must stay inside the current directory"));
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn write_refuses_symlink_parents_targets_and_missing_or_nondirectory_parents() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("existing.txt"), "keep").unwrap();
    symlink(&nested, directory.path().join("linked")).unwrap();
    symlink(
        nested.join("existing.txt"),
        directory.path().join("alias.txt"),
    )
    .unwrap();
    fs::write(directory.path().join("file.txt"), "").unwrap();

    let symlink_parent = run(Some(directory.path()), &["write", "linked/new.txt", "text"]);
    let symlink_target = run(
        Some(directory.path()),
        &["write", "alias.txt", "replacement"],
    );
    let missing_parent = run(
        Some(directory.path()),
        &["write", "missing/new.txt", "text"],
    );
    let nondirectory_parent = run(
        Some(directory.path()),
        &["write", "file.txt/new.txt", "text"],
    );
    assert!(stderr(&symlink_parent).contains("symlink paths are not allowed"));
    assert!(stderr(&symlink_target).contains("file already exists"));
    assert!(stderr(&missing_parent).contains("parent directory not found"));
    assert!(stderr(&nondirectory_parent).contains("parent is not a directory"));
    assert_eq!(
        fs::read_to_string(nested.join("existing.txt")).unwrap(),
        "keep"
    );
    assert!(!nested.join("new.txt").exists());
}

#[test]
fn task_shows_ordered_observations_completion_and_metadata_history() {
    let directory = TempDir::new();
    fs::write(directory.path().join("brief.txt"), "brief contents").unwrap();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Summarize the brief",
            "--max-actions",
            "4",
        ],
        b"read brief.txt\nlist .\ncreate summary.txt saved summary\ncomplete saved\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    let read = output.find("Observation 1: read brief.txt").unwrap();
    let listed = output.find("Observation 2: listed .").unwrap();
    let created = output.find("Observation 3: created summary.txt").unwrap();
    let state = output.find("Turn state: succeeded").unwrap();
    assert!(read < listed && listed < created && created < state);
    assert!(output.contains("brief contents"));
    assert!(output.contains("Turn outcome: success: saved"));
    assert!(output.contains("Actions: 4/4"));
    assert!(output.contains("Task state: succeeded"));
    assert_lifecycle_transitions(output, "succeeded");
    let history = output
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(history.contains("FileRead { bytes_read: 14 }"));
    assert!(history.contains("DirectoryListed { entry_count: 1 }"));
    assert!(history.contains("FileCreated { bytes_written: 13 }"));
    assert!(history.contains("Completed"));
    assert!(!history.contains("brief contents"));
    assert_eq!(
        fs::read_to_string(directory.path().join("summary.txt")).unwrap(),
        "saved summary"
    );
}

#[test]
fn task_cli_uses_the_selected_workspace_path() {
    let directory = TempDir::new();
    let caller_directory = directory.path().join("caller directory");
    let workspace = directory.path().join("selected workspace");
    fs::create_dir(&caller_directory).unwrap();
    fs::create_dir(&workspace).unwrap();
    let workspace_argument = workspace.to_str().expect("UTF-8 workspace path");

    let output = run_with_input(
        &caller_directory,
        &[
            "task",
            "--goal",
            "Create a file in the selected workspace",
            "--max-actions",
            "2",
            "--workspace",
            workspace_argument,
        ],
        b"create selected.txt created\ncomplete saved\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Task outcome: succeeded"));
    assert_eq!(
        fs::read_to_string(workspace.join("selected.txt")).unwrap(),
        "created"
    );
    assert!(!caller_directory.join("selected.txt").exists());
}

#[test]
fn task_mkdir_creates_one_directory_for_existing_file_actions() {
    let directory = TempDir::new();
    let payload = "private directory payload";
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Prepare a workspace",
            "--max-actions",
            "3",
        ],
        b"mkdir generated\ncreate generated/note.txt private directory payload\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output_text = stdout(&output);
    let created_directory = output_text
        .find("Observation 1: created directory generated")
        .unwrap();
    let created_file = output_text
        .find("Observation 2: created generated/note.txt")
        .unwrap();
    let completed = output_text.find("Task state: succeeded").unwrap();
    assert!(created_directory < created_file && created_file < completed);
    assert!(output_text.contains("Actions: 3/3"));
    assert_eq!(
        fs::read_to_string(directory.path().join("generated/note.txt")).unwrap(),
        payload
    );
    let history = output_text
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(history.contains("DirectoryCreated"));
    assert!(history.contains("FileCreated { bytes_written: 25 }"));
    assert!(!history.contains("generated"));
    assert!(!history.contains(payload));
}

#[test]
fn task_composes_actions_from_the_observed_search_location_interactively() {
    let directory = TempDir::new();
    let query = "PRIVATE_QUERY_82";
    let old_match = "OLD_MATCH_91";
    let replacement = "NEW_VALUE_47";
    let initial_contents =
        format!("unrelated first line\n{query} / {old_match}\nunrelated last line\n");
    let final_contents =
        format!("unrelated first line\n{query} / {replacement}\nunrelated last line\n");
    let nested_path = "nested space/quoted \" slash \\ café";
    let relative_path = format!("{nested_path}/notes.txt");
    fs::create_dir_all(directory.path().join(nested_path)).unwrap();
    fs::write(directory.path().join(&relative_path), &initial_contents).unwrap();
    fs::write(
        directory.path().join("unrelated.txt"),
        "leave this file alone\n",
    )
    .unwrap();

    let mut child = KillOnDrop(
        command(
            Some(directory.path()),
            &[
                "task",
                "--goal",
                "Derive a result from a matching line",
                "--max-actions",
                "6",
            ],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start interactive Krypper task"),
    );
    let mut stdin = child.0.stdin.take().expect("piped stdin");
    let streams: [(TaskCliStream, Box<dyn Read + Send>); 2] = [
        (
            TaskCliStream::Stdout,
            Box::new(child.0.stdout.take().expect("piped stdout")),
        ),
        (
            TaskCliStream::Stderr,
            Box::new(child.0.stderr.take().expect("piped stderr")),
        ),
    ];
    let (sender, receiver) = mpsc::channel();
    let mut reader_threads = Vec::new();
    for (stream, mut pipe) in streams {
        let sender = sender.clone();
        reader_threads.push(thread::spawn(move || {
            let mut buffer = [0_u8; 512];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        let message = match stream {
                            TaskCliStream::Stdout => {
                                TaskCliOutput::Stdout(buffer[..length].to_vec())
                            }
                            TaskCliStream::Stderr => {
                                TaskCliOutput::Stderr(buffer[..length].to_vec())
                            }
                        };
                        if sender.send(message).is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }
    drop(sender);

    let mut output = Vec::new();
    let mut error = Vec::new();
    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 1, 0);
    writeln!(stdin, "search-tree . {query}").unwrap();
    stdin.flush().unwrap();

    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 2, 2);
    let search_observation = String::from_utf8(output.clone()).unwrap();
    assert_eq!(
        search_observation,
        "Observation 1: search-tree in . found 1 locations\n  \"nested space/quoted \\\" slash \\\\ café/notes.txt\":2\n"
    );
    let location = search_observation.lines().nth(1).unwrap().trim();
    let (path_prefix, observed_line) = location.rsplit_once("\":").unwrap();
    let observed_path_token = format!("{path_prefix}\"");
    let observed_line = observed_line.parse::<usize>().unwrap();
    let observed_path = relative_path.as_str();
    assert_eq!(
        observed_path_token,
        "\"nested space/quoted \\\" slash \\\\ café/notes.txt\""
    );
    assert_eq!(observed_line, 2);
    writeln!(stdin, "read-range {observed_path_token} {observed_line} 1").unwrap();
    stdin.flush().unwrap();

    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 3, 4);
    let observed_output = String::from_utf8(output.clone()).unwrap();
    let range_header = format!(
        "Observation 2: read-range {observed_path} lines {observed_line}-{observed_line}\n"
    );
    let observed_excerpt = observed_output
        .split_once(&range_header)
        .unwrap()
        .1
        .lines()
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(observed_excerpt, format!("{query} / {old_match}"));
    assert!(observed_output.ends_with(&format!("{range_header}{observed_excerpt}\n")));
    let observed_old_match = observed_excerpt.split_once(" / ").unwrap().1;
    writeln!(
        stdin,
        "replace {observed_path_token} {observed_old_match} => {replacement}"
    )
    .unwrap();
    stdin.flush().unwrap();

    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 4, 5);
    assert!(String::from_utf8_lossy(&output).ends_with(&format!(
        "Observation 3: replaced text in {observed_path} ({} bytes)\n",
        final_contents.len()
    )));
    writeln!(stdin, "mkdir derived").unwrap();
    stdin.flush().unwrap();

    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 5, 6);
    assert!(
        String::from_utf8_lossy(&output).ends_with("Observation 4: created directory derived\n")
    );
    let derived_result = format!("{observed_path}:{observed_line} => {replacement}");
    writeln!(stdin, "create derived/summary.txt {derived_result}").unwrap();
    stdin.flush().unwrap();

    read_task_cli_until_prompt(&receiver, &mut output, &mut error, 6, 7);
    assert!(String::from_utf8_lossy(&output).ends_with(&format!(
        "Observation 5: created derived/summary.txt ({} bytes)\n",
        derived_result.len()
    )));
    writeln!(stdin, "complete derived result saved").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let status = child.0.wait().expect("wait for interactive task");
    for message in receiver {
        match message {
            TaskCliOutput::Stdout(bytes) => output.extend(bytes),
            TaskCliOutput::Stderr(bytes) => error.extend(bytes),
        }
    }
    for reader_thread in reader_threads {
        reader_thread.join().expect("join CLI stream reader");
    }

    assert!(status.success(), "{}", String::from_utf8_lossy(&error));
    assert_eq!(
        String::from_utf8(error).unwrap(),
        "action 1/6> action 2/6> action 3/6> action 4/6> action 5/6> action 6/6> "
    );
    let output = String::from_utf8(output).unwrap();
    let search = output
        .find("Observation 1: search-tree in . found 1 locations")
        .unwrap();
    let range = output
        .find(&format!(
            "Observation 2: read-range {observed_path} lines 2-2"
        ))
        .unwrap();
    let replace = output
        .find(&format!("Observation 3: replaced text in {observed_path}"))
        .unwrap();
    let mkdir = output
        .find("Observation 4: created directory derived")
        .unwrap();
    let create = output
        .find("Observation 5: created derived/summary.txt")
        .unwrap();
    let task_state = output.find("Task state: succeeded").unwrap();
    assert!(
        search < range
            && range < replace
            && replace < mkdir
            && mkdir < create
            && create < task_state
    );
    assert!(output.contains("Task outcome: succeeded"));
    assert!(output.contains("Turn state: succeeded"));
    assert!(output.contains("Turn outcome: success: derived result saved"));
    assert!(output.contains("Actions: 6/6"));
    assert_lifecycle_transitions(&output, "succeeded");

    let activity = output
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("TreeTextSearched { location_count: 1 }"));
    assert!(activity.contains("FileRangeRead { start_line: 2, end_line: 2,"));
    assert!(activity.contains("TextReplaced { bytes_written:"));
    assert!(activity.contains("DirectoryCreated"));
    assert!(activity.contains(&format!(
        "FileCreated {{ bytes_written: {} }}",
        derived_result.len()
    )));
    assert!(activity.contains("Completed"));
    let retained_summaries = &output[output
        .find("Task lifecycle history (metadata only):")
        .unwrap()..];
    assert_eq!(output.matches(query).count(), 1);
    assert_eq!(output.matches(observed_excerpt.as_str()).count(), 1);
    assert_eq!(output.matches(observed_old_match).count(), 1);
    assert!(!output.contains(replacement));
    for private_text in [
        query,
        observed_excerpt.as_str(),
        observed_old_match,
        replacement,
    ] {
        assert!(!retained_summaries.contains(private_text));
    }

    assert_eq!(
        fs::read_to_string(directory.path().join(&relative_path)).unwrap(),
        final_contents
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("derived/summary.txt")).unwrap(),
        derived_result
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("unrelated.txt")).unwrap(),
        "leave this file alone\n"
    );
}

#[test]
fn task_mkdir_failure_records_one_terminal_result_without_another_decision() {
    let directory = TempDir::new();
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject a workspace escape",
            "--max-actions",
            "2",
        ],
        b"mkdir ../escaped\ncomplete must-not-run\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output_text = stdout(&output);
    assert!(output_text.contains("Observation 1: mkdir ../escaped failed:"));
    assert!(output_text.contains(
        "Turn outcome: failure: workspace directory creation failed: path must stay inside the current directory"
    ));
    assert!(output_text.contains("Task state: failed"));
    assert!(output_text.contains("Actions: 1/2"));
    assert_eq!(stderr(&output).matches("action ").count(), 1);
    assert!(!directory.path().parent().unwrap().join("escaped").exists());

    let history = output_text
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(history.contains("ActionFailed { action: CreateDirectory }"));
    assert!(!history.contains("../escaped"));
    assert_lifecycle_transitions(output_text, "failed");
}

#[test]
fn task_replaces_one_exact_match_and_prints_metadata_only() {
    let directory = TempDir::new();
    let match_text = "PRIVATE_MATCH_33";
    let replacement_text = "PRIVATE_REPLACEMENT_44";
    fs::write(
        directory.path().join("note.txt"),
        format!("before {match_text} after"),
    )
    .unwrap();
    let input = format!("replace note.txt {match_text} => {replacement_text}\ncomplete done\n");

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Replace one exact match",
            "--max-actions",
            "2",
        ],
        input.as_bytes(),
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("Observation 1: replaced text in note.txt"));
    assert!(stdout.contains("Turn outcome: success: done"));
    assert!(stdout.contains("Actions: 2/2"));
    let activity = stdout
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("TextReplaced { bytes_written:"));
    assert!(activity.contains("Completed"));
    assert!(!stdout.contains(match_text));
    assert!(!stdout.contains(replacement_text));
    assert!(!activity.contains(match_text));
    assert!(!activity.contains(replacement_text));
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        format!("before {replacement_text} after")
    );
}

#[test]
fn task_can_replace_a_match_with_empty_text() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "remove this text").unwrap();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Remove one exact match",
            "--max-actions",
            "2",
        ],
        b"replace note.txt remove this text => \ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Observation 1: replaced text in note.txt (0 bytes)"));
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        ""
    );
}

#[test]
fn task_replaces_with_multibyte_whitespace_after_the_path() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "before needle after").unwrap();

    let input = "replace note.txt\u{2003}needle => fresh\ncomplete done\n";
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Replace a match after Unicode whitespace",
            "--max-actions",
            "2",
        ],
        input.as_bytes(),
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "before fresh after"
    );
}

#[test]
fn task_replaces_with_multibyte_whitespace_after_the_action() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "before needle after").unwrap();

    let input = "replace\u{2003}note.txt needle => fresh\ncomplete done\n";
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Replace a match after Unicode whitespace",
            "--max-actions",
            "2",
        ],
        input.as_bytes(),
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "before fresh after"
    );
}

#[test]
fn task_records_and_stops_after_a_failed_replacement_with_ideographic_space() {
    let directory = TempDir::new();
    let original_contents = "before original after";
    fs::write(directory.path().join("note.txt"), original_contents).unwrap();

    let input = "replace note.txt\u{3000}needle => fresh\ncomplete must_not_run\n";
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Record a rejected replacement",
            "--max-actions",
            "3",
        ],
        input.as_bytes(),
    );

    assert_eq!(output.status.code(), Some(1));
    let output = stdout(&output);
    assert!(output.contains("replacement in note.txt failed"));
    assert!(output.contains("match text was not found"));
    assert!(output.contains("Actions: 1/3"));
    assert!(output.contains("ActionFailed { action: ReplaceText }"));
    assert!(output.contains("Turn state: failed"));
    assert!(output.contains("Task state: failed"));
    assert_lifecycle_transitions(output, "failed");
    assert!(!output.contains("needle"));
    assert!(!output.contains("fresh"));
    assert!(!output.contains("must_not_run"));
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        original_contents
    );
}

#[test]
fn task_rejects_ambiguous_replacement_once_without_printing_text_or_reading_again() {
    let directory = TempDir::new();
    let match_text = "PRIVATE_MATCH_57";
    let replacement_text = "PRIVATE_REPLACEMENT_68";
    fs::write(
        directory.path().join("note.txt"),
        format!("{match_text} {match_text}"),
    )
    .unwrap();
    let input =
        format!("replace note.txt {match_text} => {replacement_text}\ncomplete must_not_run\n");

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject an ambiguous match",
            "--max-actions",
            "3",
        ],
        input.as_bytes(),
    );

    assert_eq!(output.status.code(), Some(1));
    let stdout = stdout(&output);
    assert!(stdout.contains("replacement in note.txt failed"));
    assert!(stdout.contains("match text is ambiguous"));
    assert!(stdout.contains("Actions: 1/3"));
    assert!(stdout.contains("ActionFailed { action: ReplaceText }"));
    assert!(!stdout.contains(match_text));
    assert!(!stdout.contains(replacement_text));
    assert!(!stdout.contains("must_not_run"));
    assert_eq!(stderr(&output).matches("action ").count(), 1);
    assert_eq!(
        fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        format!("{match_text} {match_text}")
    );
}

#[test]
fn task_continue_starts_a_second_turn_with_the_same_workspace_and_shared_history() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Carry work across turns",
            "--max-actions",
            "4",
        ],
        b"create carried.txt retained effect\ncontinue\nread carried.txt\ncomplete finished\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output_text = stdout(&output);
    assert!(output_text.contains("Turn 1 result: success: continued"));
    assert!(output_text.contains("Turn 2 result: success: finished"));
    assert!(output_text.contains("Task outcome: succeeded"));
    assert!(output_text.contains("Actions: 4/4"));
    assert!(output_text.contains("Task/turn lifecycle history (metadata only):"));
    let turn_one = output_text.find("Turn 1 result:").unwrap();
    let turn_two = output_text.find("Turn 2 result:").unwrap();
    let task_outcome = output_text.find("Task outcome:").unwrap();
    assert!(turn_one < turn_two && turn_two < task_outcome);
    let lifecycle = output_text
        .split("Task/turn lifecycle history (metadata only):")
        .nth(1)
        .unwrap();
    let first_turn_ended = lifecycle.find("sequence 4: Turn {").unwrap();
    let second_turn_created = lifecycle.find("sequence 5: TurnCreated {").unwrap();
    let task_completed = lifecycle.find("sequence 8: Task {").unwrap();
    assert!(first_turn_ended < second_turn_created && second_turn_created < task_completed);
    assert!(output_text.contains("Observation 1: created carried.txt (15 bytes)"));
    assert!(output_text.contains("Observation 2: read carried.txt"));
    assert!(output_text.contains("retained effect"));
    let activity = output_text
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("Continued"));
    assert!(activity.contains("FileCreated { bytes_written: 15 }"));
    assert!(activity.contains("FileRead { bytes_read: 15 }"));
    assert!(!activity.contains("retained effect"));
    assert_eq!(
        fs::read_to_string(directory.path().join("carried.txt")).unwrap(),
        "retained effect"
    );
    assert_eq!(stderr(&output).matches("action ").count(), 4);
}

#[test]
fn task_continue_at_the_last_action_fails_without_starting_another_turn() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Stay within the budget",
            "--max-actions",
            "1",
        ],
        b"continue\ncomplete too late\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output_text = stdout(&output);
    assert!(output_text.contains("Turn outcome: failure: action budget exhausted after 1 actions"));
    assert!(output_text.contains("Task outcome: failed"));
    assert!(output_text.contains("Actions: 1/1"));
    assert!(!output_text.contains("Turn 2 result:"));
    assert!(!output_text.contains("Task/turn lifecycle history"));
    assert!(!output_text.contains("too late"));
    let activity = output_text
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("Continue"));
    assert!(activity.contains("BudgetExhausted { actions: 1 }"));
    assert_eq!(stderr(&output).matches("action ").count(), 1);
}

#[test]
fn task_reads_only_the_requested_range_and_keeps_history_metadata_only() {
    let directory = TempDir::new();
    fs::write(
        directory.path().join("notes with spaces.txt"),
        "first line\nsecond line\nthird line",
    )
    .unwrap();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Read selected lines",
            "--max-actions",
            "2",
        ],
        b"read-range notes with spaces.txt 2 2\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    assert!(output.contains("Observation 1: read-range notes with spaces.txt lines 2-3"));
    assert!(output.contains("second line\nthird line"));
    assert!(!output.contains("first line"));
    assert!(output.contains("Actions: 2/2"));
    let activity = output
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("FileRangeRead { start_line: 2, end_line: 3, bytes_returned: 22 }"));
    assert!(!activity.contains("second line"));
    assert!(!activity.contains("third line"));
}

#[test]
fn task_range_reads_report_eof_and_newline_boundaries() {
    let directory = TempDir::new();
    fs::write(directory.path().join("trailing.txt"), "first\nsecond\n").unwrap();
    fs::write(directory.path().join("empty-line.txt"), "\n").unwrap();
    fs::write(directory.path().join("empty.txt"), "").unwrap();

    let past_eof = run_with_input(
        directory.path(),
        &["task", "--goal", "Read to EOF", "--max-actions", "2"],
        b"read-range trailing.txt 2 200\ncomplete done\n",
    );
    assert!(past_eof.status.success(), "{}", stderr(&past_eof));
    let past_eof = stdout(&past_eof);
    assert!(past_eof.contains("Observation 1: read-range trailing.txt lines 2-2"));
    assert!(past_eof.contains("second\n"));
    assert!(!past_eof.contains("first"));

    let empty_line = run_with_input(
        directory.path(),
        &["task", "--goal", "Read an empty line", "--max-actions", "2"],
        b"read-range empty-line.txt 1 1\ncomplete done\n",
    );
    assert!(empty_line.status.success(), "{}", stderr(&empty_line));
    let empty_line = stdout(&empty_line);
    assert!(empty_line.contains("Observation 1: read-range empty-line.txt lines 1-1\n\n"));
    assert!(empty_line.contains("FileRangeRead { start_line: 1, end_line: 1, bytes_returned: 0 }"));

    let empty_file_output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject an empty file start",
            "--max-actions",
            "2",
        ],
        b"read-range empty.txt 1 1\ncomplete must not run\n",
    );
    assert_eq!(empty_file_output.status.code(), Some(1));
    let empty_file = stdout(&empty_file_output);
    assert!(empty_file.contains("starting line 1 does not exist"));
    assert!(empty_file.contains("Actions: 1/2"));
    assert!(!empty_file.contains("must not run"));
    assert_eq!(stderr(&empty_file_output).matches("action ").count(), 1);
}

#[test]
fn task_range_read_rejects_invalid_arguments_and_oversized_excerpt_once() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "available\n").unwrap();
    fs::write(
        directory.path().join("large-line.txt"),
        "x".repeat(64 * 1024 + 1),
    )
    .unwrap();

    for (input, expected_error) in [
        (
            b"read-range note.txt 0 1\ncomplete later\n".as_slice(),
            "starting line must be positive",
        ),
        (
            b"read-range note.txt 1 0\ncomplete later\n".as_slice(),
            "line count must be positive",
        ),
        (
            b"read-range note.txt 1 201\ncomplete later\n".as_slice(),
            "requested line count exceeds 200 lines",
        ),
        (
            b"read-range large-line.txt 1 1\ncomplete later\n".as_slice(),
            "selected excerpt exceeds 64 KiB",
        ),
    ] {
        let output = run_with_input(
            directory.path(),
            &[
                "task",
                "--goal",
                "Reject invalid range",
                "--max-actions",
                "3",
            ],
            input,
        );
        assert_eq!(output.status.code(), Some(1));
        let stdout = stdout(&output);
        assert!(stdout.contains(expected_error), "{stdout}");
        assert!(stdout.contains("Actions: 1/3"));
        assert!(stdout.contains("ActionFailed { action: ReadRange }"));
        assert!(!stdout.contains("must not run"));
        assert!(!stdout.contains(&"x".repeat(32)));
        assert_eq!(stderr(&output).matches("action ").count(), 1);
    }
}

#[test]
fn task_shares_the_action_budget_across_continued_turns() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Share one action budget",
            "--max-actions",
            "3",
        ],
        b"create carried.txt retained\ncontinue\nread carried.txt\ncomplete too late\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output_text = stdout(&output);
    assert!(output_text.contains("Turn 1 result: success: continued"));
    assert!(output_text.contains("Turn 2 result: failure: action budget exhausted after 1 actions"));
    assert!(output_text.contains("Task outcome: failed"));
    assert!(output_text.contains("Actions: 3/3"));
    assert!(!output_text.contains("too late"));
    assert_eq!(stderr(&output).matches("action ").count(), 3);
    assert_eq!(
        fs::read_to_string(directory.path().join("carried.txt")).unwrap(),
        "retained"
    );
}

#[test]
fn task_cancellation_during_a_later_turn_keeps_earlier_results_and_effects() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Cancel after continuing",
            "--max-actions",
            "6",
        ],
        b"create earlier.txt earlier effect\nread earlier.txt\ncontinue\ncreate kept.txt retained\ncancel\ncreate ignored.txt ignored\n",
    );

    assert_eq!(output.status.code(), Some(130));
    let output_text = stdout(&output);
    assert!(output_text.contains("Turn 1 result: success: continued"));
    assert!(output_text.contains("Turn 2 result: cancelled by caller"));
    assert!(output_text.contains("Task outcome: cancelled by caller"));
    assert!(output_text.contains("Actions: 5/6"));
    assert!(output_text.contains("earlier effect"));
    assert!(!output_text.contains("ignored.txt"));
    assert_eq!(
        fs::read_to_string(directory.path().join("kept.txt")).unwrap(),
        "retained"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("earlier.txt")).unwrap(),
        "earlier effect"
    );
    assert!(!directory.path().join("ignored.txt").exists());
    assert_eq!(stderr(&output).matches("action ").count(), 5);
}

#[test]
fn task_failure_after_continue_stops_before_another_decision() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Stop after an invalid action",
            "--max-actions",
            "4",
        ],
        b"continue\ndelete note.txt\ncomplete too late\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output_text = stdout(&output);
    assert!(output_text.contains("Turn 1 result: success: continued"));
    assert!(output_text.contains("Turn 2 result: failure:"));
    assert!(output_text.contains("Task outcome: failed"));
    assert!(!output_text.contains("too late"));
    assert_eq!(stderr(&output).matches("action ").count(), 2);
}

#[test]
fn task_cancel_stops_before_reading_or_running_a_later_action() {
    let directory = TempDir::new();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Inspect files", "--max-actions", "3"],
        b"cancel\ncreate must-not-exist.txt ignored\n",
    );

    assert_eq!(output.status.code(), Some(130));
    let output_text = stdout(&output);
    assert!(output_text.contains("Task state: cancelled"));
    assert!(output_text.contains("Turn state: cancelled"));
    assert!(output_text.contains("Turn outcome: cancelled by caller"));
    assert!(output_text.contains("Actions: 1/3"));
    assert_lifecycle_transitions(output_text, "cancelled");
    assert!(output_text.contains("- Cancelled"));
    assert!(!output_text.contains("Turn outcome: success"));
    assert!(!output_text.contains("Turn outcome: failure"));
    assert_eq!(stderr(&output).matches("action ").count(), 1);
    assert!(!directory.path().join("must-not-exist.txt").exists());
}

#[test]
fn task_cancel_preserves_prior_observations_and_effects_in_metadata_order() {
    let directory = TempDir::new();
    fs::write(directory.path().join("brief.txt"), "private contents").unwrap();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Inspect files", "--max-actions", "4"],
        b"read brief.txt\ncreate kept.txt retained effect\ncancel\ncreate must-not-exist.txt ignored\n",
    );

    assert_eq!(output.status.code(), Some(130));
    let output_text = stdout(&output);
    let first = output_text.find("Observation 1: read brief.txt").unwrap();
    let second = output_text.find("Observation 2: created kept.txt").unwrap();
    let state = output_text.find("Task state: cancelled").unwrap();
    assert!(first < second && second < state);
    assert!(output_text.contains("private contents"));
    assert!(output_text.contains("Turn state: cancelled"));
    assert!(output_text.contains("Turn outcome: cancelled by caller"));
    assert!(output_text.contains("Actions: 3/4"));

    let history = output_text
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    let read = history.find("FileRead { bytes_read: 16 }").unwrap();
    let created = history.find("FileCreated { bytes_written: 15 }").unwrap();
    let cancelled = history.find("Cancelled").unwrap();
    assert!(read < created && created < cancelled);
    assert!(!history.contains("private contents"));
    assert!(!history.contains("retained effect"));
    assert_eq!(
        fs::read_to_string(directory.path().join("kept.txt")).unwrap(),
        "retained effect"
    );
    assert!(!directory.path().join("must-not-exist.txt").exists());
    assert_eq!(stderr(&output).matches("action ").count(), 3);
}

#[test]
fn task_exhausts_the_action_budget_with_one_failed_terminal_outcome() {
    let directory = TempDir::new();
    fs::write(directory.path().join("brief.txt"), "brief").unwrap();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Inspect files", "--max-actions", "2"],
        b"read brief.txt\nlist .\ncomplete too late\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output = stdout(&output);
    assert!(output.contains("Observation 1: read brief.txt"));
    assert!(output.contains("Observation 2: listed ."));
    assert!(output.contains("Turn state: failed"));
    assert!(output.contains("Turn outcome: failure: action budget exhausted after 2 actions"));
    assert!(output.contains("Actions: 2/2"));
    assert!(output.contains("Task state: failed"));
    assert_lifecycle_transitions(output, "failed");
    assert!(!output.contains("too late"));
}

#[test]
fn task_eof_and_invalid_input_share_a_failed_terminal_state() {
    let directory = TempDir::new();
    let cases: [(&[u8], &str); 2] = [
        (b"", "end of input"),
        (b"delete note.txt\n", "unsupported workspace action"),
    ];

    for (input, expected_error) in cases {
        let output = run_with_input(
            directory.path(),
            &["task", "--goal", "Inspect files", "--max-actions", "3"],
            input,
        );

        assert_eq!(output.status.code(), Some(1));
        let output = stdout(&output);
        assert!(output.contains("Turn state: failed"));
        assert!(output.contains("Turn outcome: failure:"));
        assert!(output.contains("Actions: 1/3"));
        assert!(output.contains(expected_error));
    }
}

#[test]
fn task_accepts_an_action_line_at_the_documented_byte_limit() {
    let directory = TempDir::new();
    let mut input = b"list . ".to_vec();
    input.resize(MAX_TASK_ACTION_LINE_BYTES - 1, b' ');
    input.push(b'\n');
    input.extend_from_slice(b"complete done\n");
    assert_eq!(
        input.iter().position(|byte| *byte == b'\n').unwrap() + 1,
        MAX_TASK_ACTION_LINE_BYTES
    );

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Inspect files", "--max-actions", "2"],
        &input,
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Observation 1: listed . (0 entries)"));
    assert!(stdout(&output).contains("Turn outcome: success: done"));
    assert!(stdout(&output).contains("Actions: 2/2"));
}

#[test]
fn task_rejects_an_oversized_action_line_once_without_writing_or_parsing_a_prefix() {
    let directory = TempDir::new();
    let mut input = b"create should-not-exist.txt ".to_vec();
    input.extend(std::iter::repeat_n(b'x', MAX_TASK_ACTION_LINE_BYTES));
    input.extend_from_slice(b"\ncomplete should-not-run\n");

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Create a file", "--max-actions", "3"],
        &input,
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(!directory.path().join("should-not-exist.txt").exists());
    assert!(!directory.path().join("should-not-run").exists());
    let output = stdout(&output);
    let limit_error = format!("action line exceeds limit of {MAX_TASK_ACTION_LINE_BYTES} bytes");
    assert_eq!(output.matches(&limit_error).count(), 2);
    assert!(output.contains("Observation 1: action failed:"));
    assert!(output.contains("Turn state: failed"));
    assert!(output.contains(&format!(
        "Turn outcome: failure: unsupported workspace action '{limit_error}'"
    )));
    assert!(output.contains("Actions: 1/3"));
    assert!(!output.contains("Observation 2:"));
}

#[test]
fn task_can_create_a_file_with_the_maximum_utf8_content() {
    let directory = TempDir::new();
    let contents = "é".repeat(MAX_FILE_SIZE / "é".len());
    let mut input = b"create a ".to_vec();
    input.extend_from_slice(contents.as_bytes());
    input.extend_from_slice(b"\ncomplete done\n");

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Create a file", "--max-actions", "2"],
        &input,
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Observation 1: created a (1048576 bytes)"));
    assert!(stdout(&output).contains("Turn outcome: success: done"));
    assert_eq!(
        fs::read_to_string(directory.path().join("a")).unwrap(),
        contents
    );
}

#[test]
fn task_rejects_outside_and_existing_file_creations_without_writing() {
    let directory = TempDir::new();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside = directory.path().join("outside.txt");
    fs::write(&outside, "keep outside").unwrap();
    fs::write(workspace.join("existing.txt"), "keep existing").unwrap();

    let outside_output = run_with_input(
        &workspace,
        &["task", "--goal", "Create a file", "--max-actions", "1"],
        b"create ../outside.txt replace\n",
    );
    let existing_output = run_with_input(
        &workspace,
        &["task", "--goal", "Create a file", "--max-actions", "1"],
        b"create existing.txt replace\n",
    );

    assert_eq!(outside_output.status.code(), Some(1));
    assert_eq!(existing_output.status.code(), Some(1));
    assert!(stdout(&outside_output).contains("Turn outcome: failure:"));
    assert!(stdout(&outside_output).contains("path must stay inside the current directory"));
    assert!(stdout(&existing_output).contains("file already exists"));
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep outside");
    assert_eq!(
        fs::read_to_string(workspace.join("existing.txt")).unwrap(),
        "keep existing"
    );
}

#[test]
fn malformed_quoted_path_fails_once_and_stops_the_task() {
    let directory = TempDir::new();
    let private_payload = "PRIVATE_CREATE_PAYLOAD";
    let input = format!("create \"unterminated {private_payload}\ncomplete must_not_run\n");
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject malformed input",
            "--max-actions",
            "3",
        ],
        input.as_bytes(),
    );

    assert_eq!(output.status.code(), Some(1));
    let output_text = stdout(&output);
    assert!(output_text.contains("Task state: failed"));
    assert_eq!(
        output_text.matches("Observation 1: action failed:").count(),
        1
    );
    assert!(output_text.contains("unsupported workspace action 'invalid quoted path arguments'"));
    assert!(output_text.contains(
        "Turn outcome: failure: unsupported workspace action 'invalid quoted path arguments'"
    ));
    assert_eq!(
        output_text
            .matches("ActionFailed { action: Unsupported }")
            .count(),
        1
    );
    assert!(output_text.contains("Actions: 1/3"));
    assert!(!output_text.contains(private_payload));
    assert!(!output_text.contains("must_not_run"));
    assert_eq!(stderr(&output), "action 1/3> ");
    assert_lifecycle_transitions(output_text, "failed");
}

#[cfg(unix)]
#[test]
fn quoted_task_paths_still_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside = directory.path().join("outside.txt");
    fs::write(&outside, "outside contents").unwrap();
    symlink(&outside, workspace.join("alias.txt")).unwrap();

    let output = run_with_input(
        &workspace,
        &[
            "task",
            "--goal",
            "Reject symlink paths",
            "--max-actions",
            "1",
        ],
        b"read \"alias.txt\"\n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("path must stay inside the current directory"));
    assert!(!stdout(&output).contains("outside contents"));
}

#[test]
fn task_requires_a_nonempty_goal_and_positive_action_limit() {
    let directory = TempDir::new();
    let missing_goal = run(Some(directory.path()), &["task", "--max-actions", "1"]);
    let empty_goal = run(
        Some(directory.path()),
        &["task", "--goal", " ", "--max-actions", "1"],
    );
    let zero_limit = run(
        Some(directory.path()),
        &["task", "--goal", "Inspect files", "--max-actions", "0"],
    );

    assert_eq!(missing_goal.status.code(), Some(2));
    assert!(stderr(&missing_goal).contains("the following argument is required: --goal"));
    assert_eq!(empty_goal.status.code(), Some(2));
    assert!(stderr(&empty_goal).contains("goal must not be empty"));
    assert_eq!(zero_limit.status.code(), Some(2));
    assert!(stderr(&zero_limit).contains("action limit must be a positive integer"));
}

#[test]
fn task_help_lists_checkpoint_options_and_describes_the_invocation_budget() {
    let directory = TempDir::new();
    let output = run(Some(directory.path()), &["task", "--help"]);

    assert!(output.status.success());
    assert!(stdout(&output).contains("  continue\n"));
    assert!(stdout(&output).contains("read-range PATH START_LINE MAX_LINES"));
    assert!(stdout(&output).contains("at most 200 lines and 64 KiB"));
    assert!(stdout(&output).contains("replace PATH MATCH_TEXT => REPLACEMENT_TEXT"));
    assert!(stdout(&output).contains("search DIRECTORY QUERY"));
    assert!(stdout(&output).contains("search-tree DIRECTORY QUERY"));
    assert!(stdout(&output).contains("JSON double-quoted string"));
    assert!(stdout(&output).contains("reusable quoted path tokens"));
    assert!(stdout(&output).contains("action limit for this invocation"));
    assert!(stdout(&output).contains("--model NAME"));
    assert!(stdout(&output).contains("--workspace PATH"));
    assert!(stdout(&output).contains("--checkpoint-out PATH"));
    assert!(stdout(&output).contains("--resume PATH"));
    assert!(stdout(&output).contains(
        "choose ollama, gemini, openai, anthropic, cloudflare, groq, openrouter, deepseek, or zai"
    ));
    assert!(stdout(&output).contains("CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN"));
    assert!(stdout(&output).contains("GROQ_API_KEY"));
    assert!(stdout(&output).contains("OPENROUTER_API_KEY"));
    assert!(stdout(&output).contains("api.groq.com/openai/v1/chat/completions"));
    assert!(stdout(&output).contains("openrouter.ai/api/v1/chat/completions"));
    assert!(stdout(&output).contains("may select an upstream provider for the requested model"));
    assert!(stdout(&output).contains("Without --workspace"));
    assert!(stdout(&output).contains("--workspace PATH works with --model NAME"));
    assert!(stdout(&output).contains("127.0.0.1:11434"));
    assert!(stdout(&output).contains("Without --model, task remains caller-directed"));
    assert!(stdout(&output).contains("Read observations can include workspace file text."));
    assert!(stdout(&output).contains("not prior observations or file contents"));
    assert!(stdout(&output).contains("new --checkpoint-out path for each pause"));
    assert!(stdout(&output).contains("new checkpoint is written only after a completed"));
    assert!(stdout(&output).contains("turn reaches the safe continue boundary"));
    assert!(stdout(&output)
        .contains("A failed or interrupted turn may already have changed workspace files."));
    assert!(stdout(&output)
        .contains("The most recent checkpoint stays unchanged or the requested new checkpoint"));
    assert!(stdout(&output)
        .contains("Inspect and reconcile the current workspace before resuming from an older"));
    assert!(stdout(&output).contains("checkpoint after such a turn."));
}

#[test]
fn cloudflare_configuration_error_is_visible_before_lifecycle_in_process() {
    let directory = TempDir::new();
    let checkpoint = "new checkpoint.json";
    let mut command = command(
        Some(directory.path()),
        &[
            "task",
            "--goal",
            "Reject missing provider configuration",
            "--max-actions",
            "1",
            "--model",
            "test-model",
            "--provider",
            "cloudflare",
            "--workspace",
            directory.path().to_str().expect("UTF-8 test directory"),
            "--checkpoint-out",
            checkpoint,
        ],
    );
    for name in [
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_API_TOKEN",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GROQ_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        command.env_remove(name);
    }
    let output = command.output().expect("run Cloudflare preflight case");

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("CLOUDFLARE_ACCOUNT_ID"));
    assert!(!stderr(&output).contains("model decision"));
    assert!(!directory.path().join(checkpoint).exists());
}

#[test]
fn task_search_actions_emit_quoted_paths_with_escapes_and_unicode() {
    let directory = TempDir::new();
    let path = "quoted \"name\" \\ café.txt";
    fs::write(directory.path().join(path), "needle\n").unwrap();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Find both paths", "--max-actions", "3"],
        b"search . needle\nsearch-tree . needle\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    let token = r#""quoted \"name\" \\ café.txt""#;
    assert_eq!(output.matches(&format!("  {token}:1\n")).count(), 2);
    assert!(output.contains("Observation 1: search in . found 1 locations"));
    assert!(output.contains("Observation 2: search-tree in . found 1 locations"));
    assert!(output.contains("Turn outcome: success: done"));
}

#[test]
fn task_searches_hidden_files_in_order_and_reports_only_matching_line_locations() {
    let directory = TempDir::new();
    fs::write(
        directory.path().join("z.txt"),
        "first line\nneedle secret needle\nNeedle different case\n",
    )
    .unwrap();
    fs::write(directory.path().join(".hidden"), "needle hidden\n").unwrap();
    fs::create_dir(directory.path().join("nested")).unwrap();
    fs::write(
        directory.path().join("nested/ignored.txt"),
        "needle nested\n",
    )
    .unwrap();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Find a literal", "--max-actions", "2"],
        b"search . needle\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    let hidden = output.find("  \".hidden\":1").unwrap();
    let regular = output.find("  \"z.txt\":2").unwrap();
    assert!(hidden < regular, "search locations must be ordered by path");
    assert!(output.contains("Observation 1: search in . found 2 locations"));
    assert!(output.contains("Turn outcome: success: done"));
    assert!(output.contains("Actions: 2/2"));
    assert!(!output.contains("needle"));
    assert!(!output.contains("secret"));
    assert!(!output.contains("nested/ignored.txt"));
    assert!(!output.contains("\"z.txt\":3"));

    let activity = output
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("TextSearched { location_count: 2 }"));
    assert!(!activity.contains("needle"));
}

#[test]
fn task_search_with_no_matches_reports_an_empty_location_list() {
    let directory = TempDir::new();
    fs::write(directory.path().join("note.txt"), "different literal\n").unwrap();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Find a missing literal",
            "--max-actions",
            "2",
        ],
        b"search . absent\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    assert!(output.contains("Observation 1: search in . found 0 locations"));
    assert!(output.contains("Turn outcome: success: done"));
    assert!(output.contains("TextSearched { location_count: 0 }"));
    assert!(!output.contains("\"note.txt\":"));
}

#[test]
fn task_search_failure_is_terminal_and_does_not_print_partial_locations_or_contents() {
    let directory = TempDir::new();
    let query = "PRIVATE_SEARCH_FAILURE_QUERY_31";
    fs::write(directory.path().join("a.txt"), format!("{query} first\n")).unwrap();
    fs::write(
        directory.path().join("z-invalid.txt"),
        [b"private bytes ".as_slice(), &[0xff]].concat(),
    )
    .unwrap();
    let input = format!("search . {query}\ncomplete must_not_run\n");

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject invalid UTF-8",
            "--max-actions",
            "3",
        ],
        input.as_bytes(),
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr_output = stderr(&output);
    let output = stdout(&output);
    assert!(output
        .contains("search in . failed: workspace text search failed: file is not valid UTF-8"));
    assert!(output.contains("ActionFailed { action: SearchText }"));
    assert!(output
        .contains("Turn outcome: failure: workspace text search failed: file is not valid UTF-8"));
    assert!(output.contains("Task state: failed"));
    assert!(output.contains("Actions: 1/3"));
    assert!(!output.contains(query));
    assert!(!output.contains("private bytes"));
    assert!(!output.contains("\"a.txt\":1"));
    assert!(!output.contains("must_not_run"));
    assert_eq!(stderr_output.matches("action ").count(), 1);
}

#[test]
fn task_search_rejects_workspace_escapes() {
    let directory = TempDir::new();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(directory.path().join("outside.txt"), "outside secret").unwrap();

    let output = run_with_input(
        &workspace,
        &["task", "--goal", "Stay in workspace", "--max-actions", "1"],
        b"search \"../\" outside\n",
    );

    assert_eq!(output.status.code(), Some(1));
    let output = stdout(&output);
    assert!(output
        .contains("workspace text search failed: path must stay inside the current directory"));
    assert!(output.contains("Actions: 1/1"));
    assert!(!output.contains("outside secret"));
}

#[test]
fn task_search_escapes_control_characters_in_returned_paths() {
    let directory = TempDir::new();
    fs::write(directory.path().join("line\nbreak.txt"), "needle\n").unwrap();

    let output = run_with_input(
        directory.path(),
        &["task", "--goal", "Find a literal", "--max-actions", "2"],
        b"search . needle\ncomplete done\n",
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let output = stdout(&output);
    assert!(output.contains("\"line\\nbreak.txt\":1"));
    assert!(!output.contains("line\nbreak.txt"));
}

#[test]
fn task_search_tree_finds_nested_hidden_matches_in_order_without_contents() {
    let directory = TempDir::new();
    fs::create_dir_all(directory.path().join("nested/.hidden-dir")).unwrap();
    fs::write(directory.path().join("z.txt"), "needle phrase z\n").unwrap();
    fs::write(directory.path().join(".hidden"), "needle phrase hidden\n").unwrap();
    fs::write(
        directory.path().join("nested/.hidden-dir/a.txt"),
        "skip\nneedle phrase nested secret\n",
    )
    .unwrap();

    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Find nested literals",
            "--max-actions",
            "2",
        ],
        b"search-tree . needle phrase\ncomplete done\n",
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    let hidden = output.find("  \".hidden\":1").unwrap();
    let nested = output.find("  \"nested/.hidden-dir/a.txt\":2").unwrap();
    let regular = output.find("  \"z.txt\":1").unwrap();
    assert!(hidden < nested && nested < regular);
    assert!(output.contains("Observation 1: search-tree in . found 3 locations"));
    assert!(output.contains("Turn outcome: success: done"));
    assert!(output.contains("Actions: 2/2"));
    assert!(!output.contains("needle phrase"));
    assert!(!output.contains("hidden secret"));
    assert!(!output.contains("nested secret"));
    let activity = output
        .split("Activity history (metadata only):")
        .nth(1)
        .unwrap();
    assert!(activity.contains("TreeTextSearched { location_count: 3 }"));
    assert!(!activity.contains("needle"));
}

#[test]
fn task_search_tree_no_match_and_failure_are_metadata_only_and_terminal() {
    let no_match_directory = TempDir::new();
    fs::create_dir_all(no_match_directory.path().join("nested")).unwrap();
    fs::write(
        no_match_directory.path().join("nested/note.txt"),
        "different text\n",
    )
    .unwrap();
    let no_match = run_with_input(
        no_match_directory.path(),
        &[
            "task",
            "--goal",
            "Find a missing literal",
            "--max-actions",
            "2",
        ],
        b"search-tree nested absent\ncomplete done\n",
    );
    assert!(no_match.status.success(), "{}", stderr(&no_match));
    assert!(stdout(&no_match).contains("search-tree in nested found 0 locations"));
    assert!(stdout(&no_match).contains("TreeTextSearched { location_count: 0 }"));

    let directory = TempDir::new();
    let query = "PRIVATE_TREE_FAILURE_QUERY_31";
    fs::create_dir_all(directory.path().join("nested")).unwrap();
    fs::write(
        directory.path().join("nested/a.txt"),
        format!("{query} private match\n"),
    )
    .unwrap();
    fs::write(
        directory.path().join("nested/z-invalid.txt"),
        [b"private bytes ".as_slice(), &[0xff]].concat(),
    )
    .unwrap();
    let input = format!("search-tree . {query}\ncomplete must_not_run\n");
    let output = run_with_input(
        directory.path(),
        &[
            "task",
            "--goal",
            "Reject invalid UTF-8",
            "--max-actions",
            "3",
        ],
        input.as_bytes(),
    );

    assert_eq!(output.status.code(), Some(1));
    let stdout_output = stdout(&output);
    assert!(stdout_output.contains(
        "search-tree in . failed: workspace text search failed: file is not valid UTF-8"
    ));
    assert!(stdout_output.contains("ActionFailed { action: SearchTreeText }"));
    assert!(stdout_output
        .contains("Turn outcome: failure: workspace text search failed: file is not valid UTF-8"));
    assert!(stdout_output.contains("Actions: 1/3"));
    assert!(!stdout_output.contains(query));
    assert!(!stdout_output.contains("private bytes"));
    assert!(!stdout_output.contains("nested/a.txt:1"));
    assert!(!stdout_output.contains("must_not_run"));
    assert_eq!(stderr(&output).matches("action ").count(), 1);
}

#[test]
fn task_checkpoint_out_pauses_after_a_completed_turn_and_resume_does_not_replay_actions() {
    let temporary = TempDir::new();
    let workspace = temporary.path().join("task workspace");
    fs::create_dir(&workspace).unwrap();
    let checkpoint = temporary.path().join("task checkpoint.json");
    let checkpoint_arg = checkpoint.to_str().unwrap();
    let original_contents = "prior file contents stay outside the checkpoint";

    let save = run_with_input(
        &workspace,
        &[
            "task",
            "--goal",
            "Save this task",
            "--max-actions",
            "3",
            "--checkpoint-out",
            checkpoint_arg,
        ],
        format!("create \"first file.txt\" {original_contents}\ncontinue\n").as_bytes(),
    );

    assert_eq!(save.status.code(), Some(0), "{}", stderr(&save));
    assert!(stdout(&save).contains("Task state: running"));
    assert!(stdout(&save).contains("Task outcome: paused (still running)"));
    assert!(!stdout(&save).contains("Task outcome: succeeded"));
    let checkpoint_bytes = fs::read(&checkpoint).unwrap();
    assert!(!checkpoint_bytes
        .windows(original_contents.len())
        .any(|window| window == original_contents.as_bytes()));
    assert_eq!(
        fs::read_to_string(workspace.join("first file.txt")).unwrap(),
        original_contents
    );

    let next_checkpoint = temporary.path().join("next task checkpoint.json");
    let next_checkpoint_arg = next_checkpoint.to_str().unwrap();
    let pause_again = run_with_input(
        &workspace,
        &[
            "task",
            "--resume",
            checkpoint_arg,
            "--max-actions",
            "2",
            "--checkpoint-out",
            next_checkpoint_arg,
        ],
        b"continue\n",
    );

    assert!(pause_again.status.success(), "{}", stderr(&pause_again));
    assert!(stdout(&pause_again).contains("Task outcome: paused (still running)"));
    let next_checkpoint_bytes = fs::read(&next_checkpoint).unwrap();
    assert_ne!(next_checkpoint_bytes, checkpoint_bytes);
    assert_eq!(fs::read(&checkpoint).unwrap(), checkpoint_bytes);

    let resume = run_with_input(
        &workspace,
        &[
            "task",
            "--resume",
            next_checkpoint_arg,
            "--max-actions",
            "2",
        ],
        b"create \"second file.txt\" fresh turn\ncomplete resumed task\n",
    );

    assert!(resume.status.success(), "{}", stderr(&resume));
    assert!(stdout(&resume).contains("Task outcome: succeeded"));
    assert_eq!(stdout(&resume).matches("TurnCreated {").count(), 3);
    assert_eq!(
        fs::read_to_string(workspace.join("first file.txt")).unwrap(),
        original_contents
    );
    assert_eq!(
        fs::read_to_string(workspace.join("second file.txt")).unwrap(),
        "fresh turn"
    );
    assert_eq!(fs::read(&checkpoint).unwrap(), checkpoint_bytes);
    assert_eq!(fs::read(&next_checkpoint).unwrap(), next_checkpoint_bytes);
}

#[test]
fn task_checkpoint_out_failure_keeps_completed_workspace_actions_without_saving_checkpoint() {
    let temporary = TempDir::new();
    let workspace = temporary.path().join("task workspace");
    fs::create_dir(&workspace).unwrap();
    let checkpoint = temporary.path().join("running task checkpoint.json");
    let checkpoint_arg = checkpoint.to_str().unwrap();
    let save = run_with_input(
        &workspace,
        &[
            "task",
            "--goal",
            "Save a running task before the failed turn",
            "--max-actions",
            "3",
            "--checkpoint-out",
            checkpoint_arg,
        ],
        b"create \"first file.txt\" saved before resume\ncontinue\n",
    );

    assert_eq!(save.status.code(), Some(0), "{}", stderr(&save));
    assert!(stdout(&save).contains("Task state: running"));
    assert!(stdout(&save).contains("Task outcome: paused (still running)"));
    let checkpoint_bytes = fs::read(&checkpoint).unwrap();

    let next_checkpoint = temporary.path().join("next task checkpoint.json");
    let failed = run_with_input(
        &workspace,
        &[
            "task",
            "--resume",
            checkpoint_arg,
            "--max-actions",
            "3",
            "--checkpoint-out",
            next_checkpoint.to_str().unwrap(),
        ],
        b"create \"completed before failure.txt\" remains in workspace\n",
    );

    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    assert!(stdout(&failed).contains("Task outcome: failed"));
    assert!(!stdout(&failed).contains("Task outcome: paused"));
    assert!(!stdout(&failed).contains("Task outcome: succeeded"));
    assert!(!stdout(&failed).contains("Checkpoint saved:"));
    assert_eq!(
        fs::read_to_string(workspace.join("completed before failure.txt")).unwrap(),
        "remains in workspace"
    );
    assert_eq!(fs::read(&checkpoint).unwrap(), checkpoint_bytes);
    assert!(!next_checkpoint.exists());
}

#[test]
fn task_checkpoint_output_collision_is_rejected_before_workspace_actions() {
    let temporary = TempDir::new();
    let workspace = temporary.path().join("task workspace");
    fs::create_dir(&workspace).unwrap();
    let checkpoint = temporary.path().join("existing checkpoint.json");
    fs::write(&checkpoint, b"keep existing checkpoint").unwrap();

    let output = run(
        Some(&workspace),
        &[
            "task",
            "--goal",
            "Do not run before checkpoint collision is found",
            "--max-actions",
            "2",
            "--checkpoint-out",
            checkpoint.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("already exists"));
    assert!(!workspace.join("must-not-exist.txt").exists());
    assert_eq!(fs::read(&checkpoint).unwrap(), b"keep existing checkpoint");
}

#[test]
fn task_resume_rejects_malformed_and_oversized_checkpoints_before_actions() {
    for (name, bytes, expected_error) in [
        (
            "malformed.json",
            b"not a checkpoint".to_vec(),
            "invalid TaskSession checkpoint",
        ),
        (
            "oversized.json",
            vec![b' '; 1_048_577],
            "checkpoint exceeds 1048576 bytes",
        ),
    ] {
        let temporary = TempDir::new();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let checkpoint = temporary.path().join(name);
        fs::write(&checkpoint, bytes).unwrap();

        let output = run(
            Some(&workspace),
            &[
                "task",
                "--resume",
                checkpoint.to_str().unwrap(),
                "--max-actions",
                "2",
            ],
        );

        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains(expected_error));
        assert!(stdout(&output).is_empty());
        assert!(!workspace.join("must-not-exist.txt").exists());
    }
}

#[test]
fn task_resume_rejects_unsupported_terminal_and_ambiguous_checkpoints() {
    let temporary = TempDir::new();
    let workspace = temporary.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let running = checkpoint_with_tasks(&workspace, 1, false);

    let mut unsupported: serde_json::Value = serde_json::from_slice(&running).unwrap();
    unsupported["version"] = serde_json::json!(2);
    let terminal = checkpoint_with_tasks(&workspace, 1, true);
    let ambiguous = checkpoint_with_tasks(&workspace, 2, false);
    let cases = [
        (
            "unsupported.json",
            serde_json::to_vec(&unsupported).unwrap(),
            "unsupported TaskSession checkpoint version 2",
        ),
        (
            "terminal.json",
            terminal,
            "checkpoint task must be running, found succeeded",
        ),
        (
            "ambiguous.json",
            ambiguous,
            "checkpoint must contain exactly one task, found 2",
        ),
    ];

    for (name, bytes, expected_error) in cases {
        let checkpoint = temporary.path().join(name);
        fs::write(&checkpoint, bytes).unwrap();
        let output = run(
            Some(&workspace),
            &[
                "task",
                "--resume",
                checkpoint.to_str().unwrap(),
                "--max-actions",
                "2",
            ],
        );

        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains(expected_error));
        assert!(stdout(&output).is_empty());
        assert!(!workspace.join("must-not-exist.txt").exists());
    }
}

#[test]
fn task_resume_uses_explicit_workspace_without_changing_the_caller_directory() {
    let temporary = TempDir::new();
    let workspace = temporary.path().join("saved workspace");
    let caller_directory = temporary.path().join("different caller directory");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&caller_directory).unwrap();
    let checkpoint = temporary.path().join("saved task.json");
    fs::write(&checkpoint, checkpoint_with_tasks(&workspace, 1, false)).unwrap();

    let mismatch = run(
        Some(&caller_directory),
        &[
            "task",
            "--resume",
            checkpoint.to_str().unwrap(),
            "--max-actions",
            "1",
        ],
    );
    assert_eq!(mismatch.status.code(), Some(2));
    assert!(stderr(&mismatch).contains("workspace binding"));
    assert!(stdout(&mismatch).is_empty());

    let resumed = run_with_input(
        &caller_directory,
        &[
            "task",
            "--resume",
            checkpoint.to_str().unwrap(),
            "--max-actions",
            "1",
            "--workspace",
            workspace.to_str().unwrap(),
        ],
        b"complete restored task\n",
    );

    assert!(resumed.status.success(), "{}", stderr(&resumed));
    assert!(stdout(&resumed).contains("Task outcome: succeeded"));
    assert!(!caller_directory.join("result.txt").exists());
}

fn checkpoint_with_tasks(workspace: &Path, task_count: usize, terminal_first: bool) -> Vec<u8> {
    let mut session = TaskSession::new_in_workspace(workspace).unwrap();
    for index in 0..task_count {
        let task = session.create_task(format!("stored task {index}"));
        task.transition(TaskState::Running).unwrap();
        if terminal_first && index == 0 {
            task.transition(TaskState::Succeeded).unwrap();
        }
    }
    session.export_checkpoint().unwrap()
}
