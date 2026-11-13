use krypper::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const GOAL: &str = "Create one caller-selected note in each turn.";
const FIRST_FILE: &str = "first-turn.txt";
const FIRST_CONTENT: &str = "Created during the first turn.";
const SECOND_FILE: &str = "selected-after-restore.txt";
const SECOND_CONTENT: &str = "Created by the caller after restoring.";

struct TemporaryWorkspace(PathBuf);

impl TemporaryWorkspace {
    fn create() -> io::Result<Self> {
        let root = fs::canonicalize(std::env::temp_dir())?;
        let path = root.join(format!("krypper-checkpoint-{}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "could not remove example workspace {}: {error}",
                self.0.display()
            );
        }
    }
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

fn missing_task() -> io::Error {
    io::Error::other("task was not found in the session")
}

fn begin_turn(session: &mut TaskSession, task_id: Uuid) -> Result<Uuid, Box<dyn Error>> {
    let task = session.get_task_mut(task_id).ok_or_else(missing_task)?;
    let turn_id = task.create_turn()?;
    task.transition_turn(turn_id, TurnState::Running)?;
    Ok(turn_id)
}

fn main() -> Result<(), Box<dyn Error>> {
    let workspace = TemporaryWorkspace::create()?;
    let _current_directory = CurrentDirectory::enter(workspace.path())?;

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

    let first_file_before_restore = fs::read_to_string(workspace.path().join(FIRST_FILE))?;
    let checkpoint_bytes = session.export_checkpoint()?;
    println!(
        "First turn state: {}",
        session
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(first_turn_id)
            .ok_or_else(|| io::Error::other("first turn was not found"))?
            .state()
    );
    println!(
        "Checkpoint retained in caller memory: {} bytes",
        checkpoint_bytes.len()
    );

    drop(session);
    let mut resumed = TaskSession::restore_checkpoint(&checkpoint_bytes)?;
    let first_file_after_restore = fs::read_to_string(workspace.path().join(FIRST_FILE))?;
    if first_file_after_restore != first_file_before_restore {
        return Err(io::Error::other("restore changed the first turn's file").into());
    }
    println!("After restore, {FIRST_FILE} is unchanged: {first_file_after_restore}");

    let second_turn_id = begin_turn(&mut resumed, task_id)?;
    resumed.create_workspace_file(
        task_id,
        second_turn_id,
        Path::new(SECOND_FILE),
        SECOND_CONTENT,
    )?;
    resumed.complete_turn(
        task_id,
        second_turn_id,
        TurnOutcome::Success {
            result: format!("created {SECOND_FILE}"),
        },
    )?;
    resumed
        .get_task_mut(task_id)
        .ok_or_else(missing_task)?
        .transition(TaskState::Succeeded)?;

    println!(
        "Caller-selected second turn created {SECOND_FILE}: {}",
        fs::read_to_string(workspace.path().join(SECOND_FILE))?
    );
    println!(
        "Final turn states: first={}, second={}",
        resumed
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(first_turn_id)
            .ok_or_else(|| io::Error::other("first turn was not found"))?
            .state(),
        resumed
            .get_task(task_id)
            .ok_or_else(missing_task)?
            .get_turn(second_turn_id)
            .ok_or_else(|| io::Error::other("second turn was not found"))?
            .state()
    );
    println!(
        "Final task state: {}",
        resumed.get_task(task_id).ok_or_else(missing_task)?.state()
    );

    Ok(())
}
