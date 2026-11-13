use krypper::agent_loop::{run_goal_turn_with_budget, GoalTurnStopReason};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use krypper::OllamaDecisionSource;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const GOAL: &str = "Read brief.txt, create summary.txt, and complete the task.";
const MODEL: &str = "qwen3";
const ACTION_BUDGET: usize = 4;

struct TemporaryWorkspace {
    path: PathBuf,
    owned: bool,
}

impl TemporaryWorkspace {
    fn create() -> io::Result<Self> {
        let path = fs::canonicalize(std::env::temp_dir())?
            .join(format!("krypper-local-model-{}", Uuid::new_v4()));
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
        if self.owned {
            if let Err(error) = fs::remove_dir_all(&self.path) {
                eprintln!(
                    "could not remove example workspace {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

struct CurrentDirectory(Option<PathBuf>);

impl CurrentDirectory {
    fn enter(path: &Path) -> io::Result<Self> {
        let previous = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self(Some(previous)))
    }

    fn restore(mut self) -> io::Result<()> {
        if let Some(previous) = &self.0 {
            std::env::set_current_dir(previous)?;
        }
        self.0 = None;
        Ok(())
    }
}

impl Drop for CurrentDirectory {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            if let Err(error) = std::env::set_current_dir(previous) {
                eprintln!("could not restore current directory: {error}");
            }
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let workspace = TemporaryWorkspace::create()?;
    fs::write(
        workspace.path().join("brief.txt"),
        "Krypper tasks use bounded workspace actions and preserve caller-owned lifecycle state.\n",
    )?;

    let (mut session, task_id) = {
        let current_directory = CurrentDirectory::enter(workspace.path())?;
        let mut session = TaskSession::new();
        let task_id = session.create_task(GOAL).id();
        current_directory.restore()?;
        (session, task_id)
    };
    session
        .get_task_mut(task_id)
        .ok_or_else(missing_task)?
        .transition(TaskState::Running)?;

    let mut remaining_actions = ACTION_BUDGET;
    let mut total_actions = 0;
    let mut turn_id = begin_turn(&mut session, task_id)?;
    let final_result = loop {
        let mut decisions = OllamaDecisionSource::new(MODEL, remaining_actions);
        let result = run_goal_turn_with_budget(
            &mut session,
            task_id,
            turn_id,
            GOAL,
            remaining_actions,
            &mut decisions,
        )?;
        remaining_actions -= result.actions_taken();
        total_actions += result.actions_taken();

        match result.stop_reason() {
            GoalTurnStopReason::Continued => {
                turn_id = begin_turn(&mut session, task_id)?;
            }
            GoalTurnStopReason::Completed | GoalTurnStopReason::ActionCompleted => {
                session
                    .get_task_mut(task_id)
                    .ok_or_else(missing_task)?
                    .transition(TaskState::Succeeded)?;
                break result;
            }
            GoalTurnStopReason::ActionBudgetExhausted | GoalTurnStopReason::ActionFailed => {
                session
                    .get_task_mut(task_id)
                    .ok_or_else(missing_task)?
                    .transition(TaskState::Failed)?;
                break result;
            }
            GoalTurnStopReason::Cancelled => break result,
        }
    };

    let task = session.get_task(task_id).ok_or_else(missing_task)?;
    let turn = task
        .get_turn(final_result.turn_id())
        .ok_or_else(|| io::Error::other("turn was not found in the task"))?;
    println!("observations: {}", final_result.observations().len());
    println!("actions taken: {total_actions}");
    println!("turn outcome: {:?}", final_result.outcome());
    println!("turn state: {}", turn.state());
    println!("task state: {}", task.state());

    if !matches!(final_result.outcome(), Some(TurnOutcome::Success { .. })) {
        return Err(io::Error::other("the local model turn did not succeed").into());
    }
    workspace.cleanup()?;
    Ok(())
}

fn begin_turn(session: &mut TaskSession, task_id: Uuid) -> Result<Uuid, Box<dyn Error>> {
    let task = session.get_task_mut(task_id).ok_or_else(missing_task)?;
    let turn_id = task.create_turn()?;
    task.transition_turn(turn_id, TurnState::Running)?;
    Ok(turn_id)
}

fn missing_task() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "task was not found in the session")
}
