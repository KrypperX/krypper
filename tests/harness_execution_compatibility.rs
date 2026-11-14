use krypper::agent_loop as compatibility;
use krypper::harness::execution;
use krypper::task_lifecycle::{TaskSession, TaskState, TurnActivitySummary, TurnState};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("krypper-harness-{}", Uuid::new_v4()));
        fs::create_dir(&path).expect("create task workspace");
        Self(path)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove task workspace");
    }
}

#[test]
fn harness_execution_keeps_agent_loop_types_and_ordered_task_behavior() {
    let workspace = TempWorkspace::new();
    let mut session = TaskSession::new_in_workspace(&workspace.0).expect("bind task workspace");
    let goal = "Create and read a workspace note";
    let task_id = session.create_task(goal).id();
    session
        .get_task_mut(task_id)
        .expect("created task")
        .transition(TaskState::Running)
        .expect("start task");
    let turn_id = session
        .get_task_mut(task_id)
        .expect("running task")
        .create_turn()
        .expect("create turn");
    session
        .get_task_mut(task_id)
        .expect("running task")
        .transition_turn(turn_id, TurnState::Running)
        .expect("start turn");

    let mut observation_counts = Vec::new();
    let mut decisions = |_: &str, observations: &[compatibility::WorkspaceObservation]| {
        observation_counts.push(observations.len());
        match observations.len() {
            0 => compatibility::WorkspaceAction::CreateFile {
                path: PathBuf::from("note.txt"),
                contents: "same task-turn behavior".to_owned(),
            },
            1 => compatibility::WorkspaceAction::ReadFile {
                path: PathBuf::from("note.txt"),
            },
            _ => compatibility::WorkspaceAction::Complete {
                result: "done".to_owned(),
            },
        }
    };

    let result: compatibility::GoalTurnResult = execution::run_goal_turn_with_budget(
        &mut session,
        task_id,
        turn_id,
        goal,
        3,
        &mut decisions,
    )
    .expect("run bounded task turn");

    assert_eq!(observation_counts, [0, 1, 2]);
    assert_eq!(result.actions_taken(), 3);
    assert_eq!(
        result.stop_reason(),
        compatibility::GoalTurnStopReason::Completed
    );
    assert_eq!(result.read_result(), "same task-turn behavior");
    assert_eq!(
        result.observations(),
        &[
            execution::WorkspaceObservation::FileCreated {
                path: PathBuf::from("note.txt"),
                bytes_written: "same task-turn behavior".len(),
            },
            execution::WorkspaceObservation::FileRead {
                path: PathBuf::from("note.txt"),
                contents: "same task-turn behavior".to_owned(),
            },
        ]
    );

    let task = session.get_task(task_id).expect("task remains in session");
    let turn = task.get_turn(turn_id).expect("turn remains in task");
    assert_eq!(task.state(), TaskState::Running);
    assert_eq!(turn.state(), TurnState::Succeeded);
    assert_eq!(
        turn.activity_history(),
        [
            TurnActivitySummary::FileCreated {
                bytes_written: "same task-turn behavior".len(),
            },
            TurnActivitySummary::FileRead {
                bytes_read: "same task-turn behavior".len(),
            },
            TurnActivitySummary::Completed,
        ]
    );
}
