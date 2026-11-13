use krypper::agent_loop::{
    run_goal_turn_with_budget, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const GOAL: &str = "Read the brief and save a short summary in summary.txt.";
const ACTION_BUDGET: usize = 3;

struct TemporaryWorkspace {
    path: PathBuf,
    owned: bool,
}

impl TemporaryWorkspace {
    fn create() -> io::Result<Self> {
        let temp_root = fs::canonicalize(std::env::temp_dir())?;
        let path = temp_root.join(format!("krypper-task-turn-{}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self { path, owned: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.path)?;
        self.owned = false;
        Ok(())
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "could not remove example workspace {}: {error}",
                self.path.display()
            );
        }
    }
}

fn missing_task() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "task was not found in the session")
}

fn main() -> Result<(), Box<dyn Error>> {
    let workspace = TemporaryWorkspace::create()?;
    fs::write(
        workspace.path().join("brief.txt"),
        "The task API reads files safely and creates new files without overwriting them.\n",
    )?;

    let original_directory = std::env::current_dir()?;
    let mut session = TaskSession::new_in_workspace(workspace.path())?;
    let task_id = session.create_task(GOAL).id();
    if std::env::current_dir()? != original_directory {
        return Err(io::Error::other("the process current directory changed").into());
    }

    let turn_id = {
        let task = session.get_task_mut(task_id).ok_or_else(missing_task)?;
        task.transition(TaskState::Running)?;
        let turn_id = task.create_turn()?;
        task.transition_turn(turn_id, TurnState::Running)?;
        turn_id
    };

    let mut decision_source = |goal: &str, observations: &[WorkspaceObservation]| match observations
    {
        [] => WorkspaceAction::ReadFile {
            path: PathBuf::from("brief.txt"),
        },
        [WorkspaceObservation::FileRead { contents, .. }] => WorkspaceAction::CreateFile {
            path: PathBuf::from("summary.txt"),
            contents: format!("Goal: {goal}\nBrief: {contents}"),
        },
        [WorkspaceObservation::FileRead { contents, .. }, WorkspaceObservation::FileCreated { bytes_written, .. }] => {
            WorkspaceAction::Complete {
                result: format!(
                    "Saved a {bytes_written}-byte summary after reading {} brief bytes for: {goal}",
                    contents.len()
                ),
            }
        }
        _ => WorkspaceAction::Unsupported {
            action: "unexpected observation sequence".to_owned(),
        },
    };

    let result = run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        GOAL,
        ACTION_BUDGET,
        &mut decision_source,
    )?;

    let task_state = match result.outcome() {
        Some(TurnOutcome::Success { .. }) => TaskState::Succeeded,
        Some(TurnOutcome::Failure { .. }) => TaskState::Failed,
        None => TaskState::Cancelled,
    };
    if result.outcome().is_some() {
        session
            .get_task_mut(task_id)
            .ok_or_else(missing_task)?
            .transition(task_state)?;
    }

    if result.stop_reason() != GoalTurnStopReason::Completed
        || !matches!(result.outcome(), Some(TurnOutcome::Success { .. }))
    {
        return Err(io::Error::other("the example task turn did not complete successfully").into());
    }

    println!("Goal: {GOAL}");
    println!(
        "Turn stopped with {:?} after {} of {ACTION_BUDGET} decisions",
        result.stop_reason(),
        result.actions_taken()
    );
    println!("Turn outcome: {:?}", result.outcome());
    println!("Ordered observations:");
    for (index, observation) in result.observations().iter().enumerate() {
        match observation {
            WorkspaceObservation::FileRead { path, contents } => println!(
                "  {}. read {} ({} bytes)",
                index + 1,
                path.display(),
                contents.len()
            ),
            WorkspaceObservation::FileCreated {
                path,
                bytes_written,
            } => println!(
                "  {}. created {} ({} bytes)",
                index + 1,
                path.display(),
                bytes_written
            ),
            other => println!("  {}. {other:?}", index + 1),
        }
    }

    let task = session.get_task(task_id).ok_or_else(missing_task)?;
    println!("Task state: {}", task.state());
    println!("Read-only task lifecycle history:");
    for event in task.history() {
        println!("  #{} {:?}", event.sequence(), event.transition());
    }
    println!("Read-only activity history (metadata only):");
    for event in task.activity_history() {
        println!("  #{} {:?}", event.sequence(), event.summary());
    }
    let created_file = fs::read_to_string(workspace.path().join("summary.txt"))?;
    println!("Created file contents: {created_file}");
    drop(session);
    workspace.cleanup()?;
    Ok(())
}
