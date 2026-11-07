use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

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

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_prints_usage_and_succeeds() {
    let output = run(None, &["--help"]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("usage: krypper"));
    assert!(stdout(&output).contains("{show,list,write}"));
}

#[test]
fn missing_command_prints_usage_and_fails() {
    let output = run(None, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("usage: krypper"));
    assert!(stderr(&output).contains("{show,list,write}"));
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
