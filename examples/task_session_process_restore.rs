use krypper::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CHECKPOINT_FILE: &str = "task-session.checkpoint.json";
const FIRST_CONTENT: &str = "checkpoint-fixture-payload-must-not-be-serialized";
const FIRST_FILE: &str = "first-turn.txt";
const GOAL: &str = "Create one caller-selected note after restoring a task session.";
const USAGE: &str = "usage: task_session_process_restore save <fresh-state-directory> | resume <state-directory> <new-file-path> <content>";
const WORKSPACE_DIRECTORY: &str = "workspace";

enum Mode {
    Save {
        state_directory: PathBuf,
    },
    Resume {
        state_directory: PathBuf,
        selected_path: PathBuf,
        selected_content: String,
    },
}

struct CurrentDirectory(PathBuf);

impl CurrentDirectory {
    fn enter(path: &Path) -> io::Result<Self> {
        let previous = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self(previous))
    }
}

impl Drop for CurrentDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::env::set_current_dir(&self.0) {
            eprintln!("could not restore current directory: {error}");
        }
    }
}

fn usage_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, USAGE)
}

fn required_path(args: &mut impl Iterator<Item = OsString>) -> io::Result<PathBuf> {
    args.next().map(PathBuf::from).ok_or_else(usage_error)
}

fn required_text(args: &mut impl Iterator<Item = OsString>) -> io::Result<String> {
    args.next()
        .ok_or_else(usage_error)?
        .into_string()
        .map_err(|_| io::Error::other("selected file content must be valid UTF-8"))
}

fn parse_mode(args: impl IntoIterator<Item = OsString>) -> io::Result<Mode> {
    let mut args = args.into_iter();
    let mode = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .ok_or_else(usage_error)?;

    match mode.as_str() {
        "save" => {
            let state_directory = required_path(&mut args)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Save { state_directory })
        }
        "resume" => {
            let state_directory = required_path(&mut args)?;
            let selected_path = required_path(&mut args)?;
            let selected_content = required_text(&mut args)?;
            if args.next().is_some() {
                return Err(usage_error());
            }
            Ok(Mode::Resume {
                state_directory,
                selected_path,
                selected_content,
            })
        }
        _ => Err(usage_error()),
    }
}

/// Runs one explicit save or resume invocation without starting another process.
pub fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    match parse_mode(args)? {
        Mode::Save { state_directory } => save(&state_directory),
        Mode::Resume {
            state_directory,
            selected_path,
            selected_content,
        } => resume(&state_directory, &selected_path, &selected_content),
    }
}

fn save(state_directory: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir(state_directory)?;
    let state_root = fs::canonicalize(state_directory)?;
    let workspace_path = state_root.join(WORKSPACE_DIRECTORY);
    fs::create_dir(&workspace_path)?;
    let workspace = fs::canonicalize(workspace_path)?;
    let checkpoint_path = state_root.join(CHECKPOINT_FILE);
    let _current_directory = CurrentDirectory::enter(&workspace)?;

    let mut session = TaskSession::new();
    let task_id = {
        let task = session.create_task(GOAL);
        task.transition(TaskState::Running)?;
        task.id()
    };
    let first_turn_id = begin_turn(&mut session, task_id)?;
    session.create_workspace_file(task_id, first_turn_id, Path::new(FIRST_FILE), FIRST_CONTENT)?;
    session.complete_turn(
        task_id,
        first_turn_id,
        TurnOutcome::Success {
            result: format!("created {FIRST_FILE}"),
        },
    )?;

    let checkpoint_bytes = session.export_checkpoint()?;
    write_checkpoint_exclusively(&checkpoint_path, &checkpoint_bytes)?;

    println!("session_id={}", session.id());
    println!("task_id={task_id}");
    println!("first_turn_id={first_turn_id}");
    println!(
        "task_state={}",
        session.get_task(task_id).ok_or_else(missing_task)?.state()
    );
    println!(
        "first_turn_state={}",
        session
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(first_turn_id)
            .ok_or_else(|| io::Error::other("first turn was not found"))?
            .state()
    );
    println!("checkpoint_bytes={}", checkpoint_bytes.len());

    Ok(())
}

fn resume(
    state_directory: &Path,
    selected_path: &Path,
    selected_content: &str,
) -> Result<(), Box<dyn Error>> {
    let state_root = fs::canonicalize(state_directory)?;
    let workspace = fs::canonicalize(state_root.join(WORKSPACE_DIRECTORY))?;
    let checkpoint_bytes = fs::read(state_root.join(CHECKPOINT_FILE))?;
    let _current_directory = CurrentDirectory::enter(&workspace)?;
    let mut session = TaskSession::restore_checkpoint(&checkpoint_bytes)?;
    let (task_id, first_turn_id) = restored_first_turn(&session)?;

    let second_turn_id = begin_turn(&mut session, task_id)?;
    session.create_workspace_file(task_id, second_turn_id, selected_path, selected_content)?;
    session.complete_turn(
        task_id,
        second_turn_id,
        TurnOutcome::Success {
            result: format!("created {}", selected_path.display()),
        },
    )?;
    session
        .get_task_mut(task_id)
        .ok_or_else(missing_task)?
        .transition(TaskState::Succeeded)?;

    println!("session_id={}", session.id());
    println!("task_id={task_id}");
    println!("first_turn_id={first_turn_id}");
    println!("second_turn_id={second_turn_id}");
    println!(
        "first_turn_state={}",
        session
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(first_turn_id)
            .ok_or_else(|| io::Error::other("first turn was not found"))?
            .state()
    );
    println!(
        "second_turn_state={}",
        session
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(second_turn_id)
            .ok_or_else(|| io::Error::other("second turn was not found"))?
            .state()
    );
    println!(
        "task_state={}",
        session.get_task(task_id).ok_or_else(missing_task)?.state()
    );

    Ok(())
}

fn begin_turn(session: &mut TaskSession, task_id: Uuid) -> Result<Uuid, Box<dyn Error>> {
    let task = session.get_task_mut(task_id).ok_or_else(missing_task)?;
    let turn_id = task.create_turn()?;
    task.transition_turn(turn_id, TurnState::Running)?;
    Ok(turn_id)
}

fn restored_first_turn(session: &TaskSession) -> Result<(Uuid, Uuid), Box<dyn Error>> {
    let [task] = session.list_tasks() else {
        return Err(io::Error::other("checkpoint must contain exactly one task").into());
    };
    let [first_turn] = task.turns() else {
        return Err(io::Error::other("checkpoint must contain exactly one first turn").into());
    };
    if task.state() != TaskState::Running || first_turn.state() != TurnState::Succeeded {
        return Err(io::Error::other("checkpoint does not contain a completed first turn").into());
    }
    Ok((task.id(), first_turn.id()))
}

fn write_checkpoint_exclusively(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn missing_task() -> io::Error {
    io::Error::other("task was not found in the restored session")
}

#[cfg(not(test))]
fn main() {
    if let Err(error) = run(std::env::args_os().skip(1)) {
        eprintln!("{error}");
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
}
