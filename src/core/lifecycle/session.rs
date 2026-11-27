//! Task session state, its snapshots and the workspace operations it performs.
//!
//! `TaskSession` owns task creation order, the captured workspace root and per-task process
//! grants. Snapshots are immutable views used by readers and checkpoints.

use super::errors::{
    TaskSessionWorkspaceError, WorkspaceCreateDirectoryError, WorkspaceGuidanceReadError,
    WorkspaceListError, WorkspaceReadError, WorkspaceReplaceError, WorkspaceSearchError,
    WorkspaceSearchLocation, WorkspaceWriteError,
};
use super::process_authorization::{
    launch_command_matches_intent, start_if_authorized, GrantExpiry, GrantMode, GrantScope,
    ProcessGrant, ProcessGrantState, ProcessIntent, ProcessStartError,
};
use crate::core::task::{
    Task, TaskActivityEvent, TaskHistoryEvent, TaskState, TransitionError, TurnActivitySummary,
    TurnCompletionError, TurnOutcome, TurnState, WorkspaceRevisionFence,
};
use crate::tools::{
    apply_patch_file_at_with_revision, capture_workspace_root,
    capture_workspace_root_at_with_error, create_directory_at, create_text_file_at,
    list_task_directory_names_at, read_project_guidance_at, read_root_agents_md_at,
    read_text_file_at, replace_text_file_at_with_revision, search_text_files_at,
    search_tree_text_files_at, PatchEvidence, WorkspaceRoot,
};
use krypper_sandbox::{SpawnAuthorizationGate, SpawnGateError};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLockWriteGuard};
use std::time::Duration;
use uuid::Uuid;

/// A creation-ordered collection of tasks with a stable session identifier.
#[derive(Debug)]
pub struct TaskSession {
    pub(super) id: Uuid,
    pub(super) workspace_root: Option<Arc<WorkspaceRoot>>,
    pub(super) tasks: Vec<Task>,
    pub(super) process_grants:
        Mutex<std::collections::HashMap<Uuid, Arc<Mutex<ProcessGrantState>>>>,
    #[cfg(test)]
    pub(super) test_process_results: std::cell::RefCell<
        std::collections::HashMap<Uuid, std::collections::VecDeque<crate::ProcessResult>>,
    >,
    #[cfg(test)]
    pub(super) test_process_filters:
        std::cell::RefCell<std::collections::HashMap<Uuid, Vec<Option<String>>>>,
}

/// An immutable view of the current identifiers and states in a task session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSessionSnapshot {
    id: Uuid,
    tasks: Vec<TaskSnapshot>,
}

impl TaskSessionSnapshot {
    /// Returns the stable identifier of the task session.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns task snapshots in their session creation order.
    pub fn tasks(&self) -> &[TaskSnapshot] {
        &self.tasks
    }
}

/// An immutable view of one task's state, turns, and history sequence positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshot {
    id: Uuid,
    state: TaskState,
    lifecycle_sequence_high_water: u64,
    activity_sequence_high_water: u64,
    turns: Vec<TurnSnapshot>,
}

impl TaskSnapshot {
    /// Returns the stable identifier of the task.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the current lifecycle state of the task.
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// Returns the last sequence number in this task's combined lifecycle history.
    ///
    /// This one-based sequence is independent of the activity history sequence and is zero when
    /// the combined lifecycle history is empty.
    pub fn lifecycle_sequence_high_water(&self) -> u64 {
        self.lifecycle_sequence_high_water
    }

    /// Returns the last sequence number in this task's metadata-only activity history.
    ///
    /// This one-based sequence is independent of the lifecycle history sequence and is zero when
    /// the activity history is empty.
    pub fn activity_sequence_high_water(&self) -> u64 {
        self.activity_sequence_high_water
    }

    /// Returns turn snapshots in their task creation order.
    pub fn turns(&self) -> &[TurnSnapshot] {
        &self.turns
    }
}

/// An immutable view of one task-owned turn's identifier and current state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnSnapshot {
    id: Uuid,
    state: TurnState,
}

impl TurnSnapshot {
    /// Returns the stable identifier of the turn.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the current lifecycle state of the turn.
    pub fn state(&self) -> TurnState {
        self.state
    }
}

impl TaskSession {
    /// Creates an empty task session.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            workspace_root: None,
            tasks: Vec::new(),
            process_grants: Default::default(),
            #[cfg(test)]
            test_process_results: Default::default(),
            #[cfg(test)]
            test_process_filters: Default::default(),
        }
    }

    /// Creates an empty task session bound to an explicitly selected workspace directory.
    ///
    /// The directory is canonicalized and captured with the same filesystem identity and path
    /// containment protections used by task workspace actions. Tasks created by this session keep
    /// using that captured directory even if the process current directory later changes. This
    /// opt-in constructor does not change the process current directory. [`Self::new`] retains its
    /// existing behavior of capturing the current directory when each task is created.
    ///
    /// # Errors
    ///
    /// Returns [`TaskSessionWorkspaceErrorKind::CaptureFailed`] if the path cannot be resolved,
    /// opened, or verified as a directory, and [`TaskSessionWorkspaceErrorKind::PlatformUnavailable`]
    /// when native workspace containment is not implemented on the current platform.
    ///
    /// # Example
    ///
    /// ```
    /// use krypper::task_lifecycle::TaskSession;
    ///
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let mut session = TaskSession::new_in_workspace(".")?;
    ///     let task_id = session.create_task("Summarize the selected workspace").id();
    ///     assert!(session.get_task(task_id).is_some());
    ///     Ok(())
    /// }
    /// ```
    pub fn new_in_workspace(
        workspace_path: impl AsRef<Path>,
    ) -> Result<Self, TaskSessionWorkspaceError> {
        let workspace_root = capture_workspace_root_at_with_error(workspace_path.as_ref())
            .map_err(TaskSessionWorkspaceError::from)?;
        Ok(Self {
            id: Uuid::new_v4(),
            workspace_root: Some(Arc::new(workspace_root)),
            tasks: Vec::new(),
            process_grants: Default::default(),
            #[cfg(test)]
            test_process_results: Default::default(),
            #[cfg(test)]
            test_process_filters: Default::default(),
        })
    }

    /// Returns the stable session identifier.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns an immutable, on-demand view of the session's current task and turn state.
    ///
    /// The snapshot contains only the session, task, and turn identifiers and states, plus each
    /// task's independent combined-lifecycle and metadata-only activity sequence high-water
    /// values. A high-water value is zero when its history is empty. Creating a snapshot does not
    /// consume history or change session state, and the snapshot exposes no task descriptions,
    /// outcomes, prompts, provider data, workspace information, credentials, or secrets.
    ///
    /// ```
    /// use krypper::task_lifecycle::{TaskSession, TaskState};
    ///
    /// let mut session = TaskSession::new();
    /// let task_id = session.create_task("Summarize the selected workspace").id();
    /// let snapshot = session.snapshot();
    /// let task = &snapshot.tasks()[0];
    /// assert_eq!(task.id(), task_id);
    /// assert_eq!(task.state(), TaskState::Pending);
    /// assert_eq!(task.lifecycle_sequence_high_water(), 0);
    /// ```
    pub fn snapshot(&self) -> TaskSessionSnapshot {
        TaskSessionSnapshot {
            id: self.id,
            tasks: self
                .tasks
                .iter()
                .map(|task| TaskSnapshot {
                    id: task.id,
                    state: task.state,
                    lifecycle_sequence_high_water: task
                        .history
                        .last()
                        .map_or(0, TaskHistoryEvent::sequence),
                    activity_sequence_high_water: task
                        .activity_history
                        .last()
                        .map_or(0, TaskActivityEvent::sequence),
                    turns: task
                        .turns
                        .iter()
                        .map(|turn| TurnSnapshot {
                            id: turn.id,
                            state: turn.state,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Creates and stores a pending task, returning it for immediate use.
    pub fn create_task(&mut self, description: impl Into<String>) -> &mut Task {
        let workspace_root = match &self.workspace_root {
            Some(root) => Ok(Arc::clone(root)),
            None => capture_workspace_root().map(Arc::new),
        };
        let workspace_revision = match &workspace_root {
            Ok(root) => match root.identity() {
                Ok(identity) => WorkspaceRevisionFence::for_identity(identity),
                Err(_) => Arc::new(WorkspaceRevisionFence::default()),
            },
            Err(_) => Arc::new(WorkspaceRevisionFence::default()),
        };
        self.tasks.push(Task::new_with_workspace_root(
            description,
            workspace_root,
            workspace_revision,
        ));
        self.tasks
            .last_mut()
            .expect("invariant: a task was just appended")
    }

    /// Returns a task in this session by identifier.
    pub fn get_task(&self, task_id: Uuid) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == task_id)
    }

    /// Returns a mutable task in this session by identifier.
    pub fn get_task_mut(&mut self, task_id: Uuid) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == task_id)
    }

    pub(crate) fn read_root_project_guidance(
        &self,
        task_id: Uuid,
    ) -> Result<Option<String>, WorkspaceGuidanceReadError> {
        let root = self
            .get_task(task_id)
            .ok_or(WorkspaceGuidanceReadError::WorkspaceUnavailable)?
            .workspace_root
            .as_ref()
            .map_err(|_| WorkspaceGuidanceReadError::WorkspaceUnavailable)?;
        read_root_agents_md_at(root).map_err(Into::into)
    }

    pub(crate) fn read_project_guidance_for_targets(
        &self,
        task_id: Uuid,
        targets: &[PathBuf],
    ) -> Result<Option<String>, WorkspaceGuidanceReadError> {
        let root = self
            .get_task(task_id)
            .ok_or(WorkspaceGuidanceReadError::WorkspaceUnavailable)?
            .workspace_root
            .as_ref()
            .map_err(|_| WorkspaceGuidanceReadError::WorkspaceUnavailable)?;
        read_project_guidance_at(root, targets).map_err(Into::into)
    }

    /// Completes a running turn owned by the requested task in this session.
    ///
    /// Returns a typed error without changing any task, turn, lifecycle history, or existing
    /// outcome when the task or turn cannot be completed.
    pub fn complete_turn(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        outcome: TurnOutcome,
    ) -> Result<(), TurnCompletionError> {
        self.complete_turn_with_activity(task_id, turn_id, outcome, &[])
    }

    pub(crate) fn complete_turn_with_activity(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        outcome: TurnOutcome,
        activity: &[TurnActivitySummary],
    ) -> Result<(), TurnCompletionError> {
        let task = self
            .get_task(task_id)
            .ok_or(TurnCompletionError::TaskNotFound { task_id })?;
        if task.get_turn(turn_id).is_none() {
            let owner_task_id = self
                .tasks
                .iter()
                .find(|candidate| candidate.get_turn(turn_id).is_some())
                .map(Task::id);
            return match owner_task_id {
                Some(owner_task_id) => Err(TurnCompletionError::ForeignTurn {
                    task_id,
                    turn_id,
                    owner_task_id,
                }),
                None => Err(TurnCompletionError::TurnNotFound { task_id, turn_id }),
            };
        }

        self.get_task_mut(task_id)
            .ok_or(TurnCompletionError::TaskNotFound { task_id })?
            .complete_turn_with_activity(turn_id, outcome, activity)
    }

    /// Records metadata-only activity and cancels a running task-owned turn and its task.
    ///
    /// Activity summaries are appended in order before the existing lifecycle cancellation
    /// transitions. Cancellation does not create a success or failure outcome.
    pub fn cancel_turn_with_activity(
        &mut self,
        task_id: Uuid,
        turn_id: Uuid,
        activity: &[TurnActivitySummary],
    ) -> Result<(), TurnCompletionError> {
        let Some(task_index) = self.tasks.iter().position(|task| task.id == task_id) else {
            return Err(TurnCompletionError::TaskNotFound { task_id });
        };

        let Some(turn_index) = self.tasks[task_index]
            .turns
            .iter()
            .position(|turn| turn.id == turn_id)
        else {
            let owner_task_id = self
                .tasks
                .iter()
                .find(|task| task.get_turn(turn_id).is_some())
                .map(Task::id);
            return match owner_task_id {
                Some(owner_task_id) => Err(TurnCompletionError::ForeignTurn {
                    task_id,
                    turn_id,
                    owner_task_id,
                }),
                None => Err(TurnCompletionError::TurnNotFound { task_id, turn_id }),
            };
        };

        let task = &self.tasks[task_index];
        let turn_state = task.turns[turn_index].state;
        if turn_state != TurnState::Running {
            return Err(TurnCompletionError::TurnNotRunning {
                turn_id,
                state: turn_state,
            });
        }
        if !task.state.can_transition_to(TaskState::Cancelled) {
            return Err(TurnCompletionError::TaskTransition(TransitionError {
                from: task.state,
                to: TaskState::Cancelled,
                blocking_turn: None,
            }));
        }

        let task = &mut self.tasks[task_index];
        task.record_activity(turn_index, activity);
        task.cancel().map_err(TurnCompletionError::TaskTransition)
    }

    fn running_workspace_task(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<&Task, WorkspaceReadError> {
        let task = self
            .get_task(task_id)
            .ok_or(WorkspaceReadError::TaskNotFound { task_id })?;
        let turn = match task.get_turn(turn_id) {
            Some(turn) => turn,
            None => {
                let owner_task_id = self
                    .tasks
                    .iter()
                    .find(|candidate| candidate.get_turn(turn_id).is_some())
                    .map(Task::id);
                return match owner_task_id {
                    Some(owner_task_id) => Err(WorkspaceReadError::ForeignTurn {
                        task_id,
                        turn_id,
                        owner_task_id,
                    }),
                    None => Err(WorkspaceReadError::TurnNotFound { task_id, turn_id }),
                };
            }
        };
        if turn.state() != TurnState::Running {
            return Err(WorkspaceReadError::TurnNotRunning {
                turn_id,
                state: turn.state(),
            });
        }
        Ok(task)
    }

    /// Checks task ownership and running-turn state without performing a workspace operation.
    pub(crate) fn validate_running_turn(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<(), WorkspaceReadError> {
        self.running_workspace_task(task_id, turn_id).map(|_| ())
    }

    /// Grants this task the fixed offline Cargo profile for the TaskSession lifetime.
    /// Only trusted embedding code may grant it; model arguments and restored checkpoints
    /// never grant process authority. This preserves the original unbounded session grant.
    pub fn authorize_offline_cargo_tests(
        &self,
        task_id: Uuid,
        policy: crate::OfflineCargoPolicy,
    ) -> Result<(), &'static str> {
        self.install_process_grant(
            task_id,
            Arc::new(policy),
            GrantMode::Standing,
            GrantScope::Session,
            None,
            None,
        )
    }

    /// Grants a fixed-profile standing run for exactly one optional Cargo test filter.
    /// `None` means the no-filter operation. The grant expires after at most 24 hours.
    pub fn authorize_offline_cargo_tests_scoped(
        &self,
        task_id: Uuid,
        policy: crate::OfflineCargoPolicy,
        exact_filter: Option<&str>,
        lifetime: Duration,
    ) -> Result<(), &'static str> {
        let expiry = GrantExpiry::from_lifetime(lifetime)?;
        self.install_process_grant(
            task_id,
            Arc::new(policy),
            GrantMode::Standing,
            GrantScope::ExactFilter,
            exact_filter.map(str::to_owned),
            Some(expiry),
        )
    }

    /// Grants one exact fixed-profile operation. Approval is bound to the current workspace
    /// revision and expires after at most 24 hours; a workspace mutation invalidates it.
    pub fn authorize_offline_cargo_test_once(
        &self,
        task_id: Uuid,
        policy: crate::OfflineCargoPolicy,
        exact_filter: Option<&str>,
        lifetime: Duration,
    ) -> Result<(), &'static str> {
        let expiry = GrantExpiry::from_lifetime(lifetime)?;
        self.install_process_grant(
            task_id,
            Arc::new(policy),
            GrantMode::OneShot,
            GrantScope::ExactFilter,
            exact_filter.map(str::to_owned),
            Some(expiry),
        )
    }

    /// Revokes the active process grant and signals cancellation to its current policy.
    /// A replacement grant or a fresh authorization is required before another run.
    pub fn revoke_offline_cargo_tests(&self, task_id: Uuid) -> Result<(), &'static str> {
        if self.get_task(task_id).is_none() {
            return Err("task not found");
        }
        let grants = self
            .process_grants
            .lock()
            .map_err(|_| "process grant state is unavailable")?;
        let Some(state) = grants.get(&task_id) else {
            return Ok(());
        };
        let mut state = state
            .lock()
            .map_err(|_| "process grant state is unavailable")?;
        if let Some(grant) = state.current.take() {
            grant
                .policy
                .cancellation()
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        state.next_epoch()?;
        Ok(())
    }

    fn install_process_grant(
        &self,
        task_id: Uuid,
        policy: Arc<crate::OfflineCargoPolicy>,
        mode: GrantMode,
        scope: GrantScope,
        exact_filter: Option<String>,
        expiry: Option<GrantExpiry>,
    ) -> Result<(), &'static str> {
        let task = self.get_task(task_id).ok_or("task not found")?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|_| "task workspace is unavailable")?;
        if !workspace_revision_fence_is_registered_for_root(&task.workspace_revision, root) {
            return Err("workspace revision state is unavailable");
        }
        if matches!(scope, GrantScope::ExactFilter)
            && crate::tools::process::validate_test_filter(exact_filter.as_deref()).is_err()
        {
            return Err("test filter is invalid");
        }
        if expiry.is_some() != (scope == GrantScope::ExactFilter) {
            return Err("grant scope and expiry are inconsistent");
        }
        if policy
            .cancellation()
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err("a cancelled process policy cannot be granted");
        }

        let state = {
            let mut grants = self
                .process_grants
                .lock()
                .map_err(|_| "process grant state is unavailable")?;
            grants
                .entry(task_id)
                .or_insert_with(|| Arc::new(Mutex::new(ProcessGrantState::default())))
                .clone()
        };
        let (next_epoch, grant_id) = {
            let mut state = state
                .lock()
                .map_err(|_| "process grant state is unavailable")?;
            (state.candidate_epoch()?, Uuid::new_v4())
        };
        let mut grant = ProcessGrant {
            task_id,
            grant_id,
            epoch: next_epoch,
            mode,
            scope,
            exact_filter,
            expiry,
            policy,
            consumed: false,
            approved_digest: None,
        };

        let revision_guard = if mode == GrantMode::OneShot {
            let (revision_guard, revision) = task
                .workspace_revision
                .begin_read()
                .map_err(|_| "workspace revision state is unavailable")?;
            let root = task
                .workspace_root
                .as_ref()
                .map_err(|_| "task workspace is unavailable")?;
            let intent = process_intent(
                &grant,
                self.id,
                grant.exact_filter.as_deref(),
                root,
                revision,
            )?;
            grant.approved_digest = Some(intent.digest()?);
            Some(revision_guard)
        } else {
            None
        };

        state
            .lock()
            .map_err(|_| "process grant state is unavailable")?
            .replace_at_epoch(next_epoch, grant)?;
        drop(revision_guard);
        Ok(())
    }

    /// Executes the one fixed profile after task ownership and running-turn checks.
    pub fn run_offline_cargo_tests(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        test_filter: Option<&str>,
    ) -> Result<crate::ProcessResult, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        if crate::tools::process::validate_test_filter(test_filter).is_err() {
            return Ok(crate::ProcessResult::invalid_test_filter());
        }
        let state = self
            .process_grants
            .lock()
            .ok()
            .and_then(|grants| grants.get(&task_id).cloned());
        let Some(state) = state else {
            return Ok(crate::ProcessResult::denied(test_filter));
        };
        let grant = state.lock().ok().and_then(|state| {
            state
                .current
                .as_ref()
                .filter(|grant| grant.epoch == state.epoch)
                .cloned()
        });
        let Some(grant) = grant else {
            return Ok(crate::ProcessResult::denied(test_filter));
        };
        if grant.task_id != task_id
            || !grant.allows_filter(test_filter)
            || grant.expired()
            || grant.consumed
            || grant.epoch == 0
        {
            return Ok(crate::ProcessResult::denied(test_filter));
        }
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReadError::File {
                message: error.message().to_owned(),
            })?;
        if !workspace_revision_fence_is_registered_for_root(&task.workspace_revision, root) {
            return Ok(crate::ProcessResult::denied(test_filter));
        }
        let (revision_guard, revision) =
            task.workspace_revision
                .begin_read()
                .map_err(|_| WorkspaceReadError::File {
                    message: "workspace revision state is unavailable".into(),
                })?;
        let _revision_guard = revision_guard;
        let intent = process_intent(&grant, self.id, test_filter, root, revision);
        let Ok(intent) = intent else {
            return Ok(crate::ProcessResult::denied(test_filter));
        };
        let Ok(expected_digest) = intent.digest() else {
            return Ok(crate::ProcessResult::denied(test_filter));
        };
        if grant.mode == GrantMode::OneShot && grant.approved_digest != Some(expected_digest) {
            return Ok(crate::ProcessResult::denied(test_filter));
        }
        #[cfg(test)]
        if let Some(results) = self.test_process_results.borrow_mut().get_mut(&task_id) {
            self.test_process_filters
                .borrow_mut()
                .entry(task_id)
                .or_default()
                .push(test_filter.map(str::to_owned));
            return Ok(results
                .pop_front()
                .unwrap_or_else(|| crate::ProcessResult::denied(test_filter)));
        }
        let policy = Arc::clone(&grant.policy);
        let spawn_gate = process_spawn_gate(ProcessSpawnGateContext {
            state,
            expected: grant,
            expected_intent: intent,
            expected_digest,
            revision: Arc::clone(&task.workspace_revision),
            root: Arc::clone(root),
            session_id: self.id,
            test_filter: test_filter.map(str::to_owned),
        });
        Ok(crate::tools::process::run(
            root,
            &policy,
            test_filter,
            spawn_gate,
        ))
    }

    #[cfg(test)]
    pub(crate) fn queue_offline_cargo_results_for_test(
        &mut self,
        task_id: Uuid,
        results: impl IntoIterator<Item = crate::ProcessResult>,
    ) {
        self.test_process_results
            .borrow_mut()
            .insert(task_id, results.into_iter().collect());
    }

    #[cfg(test)]
    pub(crate) fn offline_cargo_filters_for_test(&self, task_id: Uuid) -> Vec<Option<String>> {
        self.test_process_filters
            .borrow()
            .get(&task_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Reads one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, regular-file, and 1 MiB policies as the `show` command.
    /// This operation borrows the session immutably and does not change lifecycle state or history.
    pub fn read_workspace_file(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<String, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReadError::File {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        read_text_file_at(root, path.as_os_str()).map_err(|error| WorkspaceReadError::File {
            message: error.message().to_owned(),
        })
    }

    /// Lists immediate filesystem-native entry names for a currently running owned turn.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, missing-path, and directory policies as the `list` command.
    /// Names use the same deterministic order as `list` and can be used directly as path
    /// components. At most 1,024 immediate names are returned; larger directories fail without a
    /// partial result. This operation borrows the session immutably and does not change lifecycle
    /// state or history.
    pub fn list_workspace_directory(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<Vec<OsString>, WorkspaceListError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceListError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceListError::Directory {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        list_task_directory_names_at(root, path.as_os_str()).map_err(|error| {
            WorkspaceListError::Directory {
                message: error.message().to_owned(),
            }
        })
    }

    /// Searches immediate regular UTF-8 files in one workspace directory for a literal query.
    ///
    /// Paths are relative to the task's captured workspace. Entry symlinks and non-regular files
    /// are skipped; directory traversal is not recursive. Queries, total bytes scanned, and
    /// returned file-line locations are bounded to 4 KiB, 16 MiB, and 10,000 locations. Each
    /// file remains limited to 1 MiB, and directories use the existing 1,024-entry limit. Query
    /// and file contents are not retained in task activity history.
    pub fn search_workspace_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        directory: &Path,
        query: &str,
    ) -> Result<Vec<WorkspaceSearchLocation>, WorkspaceSearchError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceSearchError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceSearchError::Search {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        search_text_files_at(root, directory.as_os_str(), query)
            .map(|matches| {
                matches
                    .into_iter()
                    .map(|(path, line)| WorkspaceSearchLocation { path, line })
                    .collect()
            })
            .map_err(|error| WorkspaceSearchError::Search {
                message: error.message().to_owned(),
            })
    }

    /// Creates exactly one directory for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's captured workspace. Its parent must already
    /// exist; symlink components, escaping paths, and existing targets are rejected. Creation uses
    /// the process umask and does not change lifecycle state or history.
    pub fn create_workspace_directory(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
    ) -> Result<(), WorkspaceCreateDirectoryError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceCreateDirectoryError::from)?;
        let root = task.workspace_root.as_ref().map_err(|error| {
            WorkspaceCreateDirectoryError::Directory {
                message: format!("task workspace could not be resolved: {}", error.message()),
            }
        })?;
        let _mutation =
            begin_managed_workspace_mutation(&task.workspace_revision, root).map_err(|_| {
                WorkspaceCreateDirectoryError::Directory {
                    message: "workspace revision state is unavailable".into(),
                }
            })?;
        create_directory_at(root, path.as_os_str()).map_err(|error| {
            WorkspaceCreateDirectoryError::Directory {
                message: error.message().to_owned(),
            }
        })
    }

    /// Searches regular UTF-8 files recursively under one workspace directory for a literal query.
    ///
    /// Paths are relative to the task's captured workspace. Entry symlinks and non-regular files
    /// are skipped; hidden entries are included. Queries, files, cumulative bytes, locations,
    /// traversal depth, and directory entries are bounded to 4 KiB, 1 MiB, 16 MiB, 10,000,
    /// 16 levels, and 4,096 entries. Query and file contents are not retained in activity history.
    pub fn search_workspace_tree_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        directory: &Path,
        query: &str,
    ) -> Result<Vec<WorkspaceSearchLocation>, WorkspaceSearchError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceSearchError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceSearchError::Search {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        search_tree_text_files_at(root, directory.as_os_str(), query)
            .map(|matches| {
                matches
                    .into_iter()
                    .map(|(path, line)| WorkspaceSearchLocation { path, line })
                    .collect()
            })
            .map_err(|error| WorkspaceSearchError::Search {
                message: error.message().to_owned(),
            })
    }

    /// Creates one UTF-8 file for a currently running turn owned by this session.
    ///
    /// The relative path is resolved from this task's creation directory and follows the same
    /// workspace boundary, symlink, parent-directory, create-only, and 1 MiB policies as the
    /// `write` command. This operation borrows the session immutably and does not change lifecycle
    /// state or history.
    pub fn create_workspace_file(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        text: &str,
    ) -> Result<(), WorkspaceWriteError> {
        let task = self
            .get_task(task_id)
            .ok_or(WorkspaceWriteError::TaskNotFound { task_id })?;
        let turn = match task.get_turn(turn_id) {
            Some(turn) => turn,
            None => {
                let owner_task_id = self
                    .tasks
                    .iter()
                    .find(|candidate| candidate.get_turn(turn_id).is_some())
                    .map(Task::id);
                return match owner_task_id {
                    Some(owner_task_id) => Err(WorkspaceWriteError::ForeignTurn {
                        task_id,
                        turn_id,
                        owner_task_id,
                    }),
                    None => Err(WorkspaceWriteError::TurnNotFound { task_id, turn_id }),
                };
            }
        };
        if turn.state() != TurnState::Running {
            return Err(WorkspaceWriteError::TurnNotRunning {
                turn_id,
                state: turn.state(),
            });
        }

        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceWriteError::File {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        let _mutation =
            begin_managed_workspace_mutation(&task.workspace_revision, root).map_err(|_| {
                WorkspaceWriteError::File {
                    message: "workspace revision state is unavailable".into(),
                }
            })?;
        create_text_file_at(root, path.as_os_str(), text).map_err(|error| {
            WorkspaceWriteError::File {
                message: error.message().to_owned(),
            }
        })
    }

    /// Replaces one unique exact text match in an existing UTF-8 file for a running task turn.
    ///
    /// The path is resolved beneath the task's captured workspace. Symlink path components,
    /// non-regular files, missing parents, and content over 1 MiB are rejected. The replacement
    /// is staged beside the original and committed with an atomic rename; this operation does not
    /// change lifecycle state or retain either text value in task history. Returns the resulting
    /// file size in bytes.
    pub fn replace_workspace_text(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        match_text: &str,
        replacement_text: &str,
    ) -> Result<usize, WorkspaceReplaceError> {
        self.replace_workspace_text_with_revision(
            task_id,
            turn_id,
            path,
            match_text,
            replacement_text,
            None,
        )
    }

    pub(crate) fn replace_workspace_text_with_revision(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        match_text: &str,
        replacement_text: &str,
        expected_revision: Option<&str>,
    ) -> Result<usize, WorkspaceReplaceError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceReplaceError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReplaceError::File {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        let _mutation =
            begin_managed_workspace_mutation(&task.workspace_revision, root).map_err(|_| {
                WorkspaceReplaceError::File {
                    message: "workspace revision state is unavailable".into(),
                }
            })?;
        replace_text_file_at_with_revision(
            root,
            path.as_os_str(),
            match_text,
            replacement_text,
            expected_revision,
        )
        .map_err(|error| WorkspaceReplaceError::File {
            message: error.message().to_owned(),
        })
    }

    /// Applies one context-checked unified diff to an existing UTF-8 file in a running task.
    ///
    /// The patch is resolved beneath the task's captured workspace. It preserves the file's line
    /// endings and supported permission bits, and succeeds only after atomic replacement and
    /// exact readback verification.
    pub(crate) fn apply_workspace_patch_with_revision(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        path: &Path,
        patch: &str,
        expected_revision: Option<&str>,
    ) -> Result<PatchEvidence, WorkspaceReplaceError> {
        let task = self
            .running_workspace_task(task_id, turn_id)
            .map_err(WorkspaceReplaceError::from)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReplaceError::File {
                message: format!("task workspace could not be resolved: {}", error.message()),
            })?;
        let _mutation =
            begin_managed_workspace_mutation(&task.workspace_revision, root).map_err(|_| {
                WorkspaceReplaceError::File {
                    message: "workspace revision state is unavailable".into(),
                }
            })?;
        apply_patch_file_at_with_revision(root, path.as_os_str(), patch, expected_revision).map_err(
            |error| WorkspaceReplaceError::File {
                message: error.message().to_owned(),
            },
        )
    }

    /// Returns the tasks in creation order as a read-only slice.
    pub fn list_tasks(&self) -> &[Task] {
        &self.tasks
    }
}

fn process_intent(
    grant: &ProcessGrant,
    session_id: Uuid,
    test_filter: Option<&str>,
    root: &WorkspaceRoot,
    revision: u64,
) -> Result<ProcessIntent, &'static str> {
    let identity = root
        .identity()
        .map_err(|_| "workspace identity is unavailable")?;
    let facts = grant.policy.authorization_facts()?;
    Ok(ProcessIntent::for_grant(
        grant,
        session_id,
        test_filter,
        identity,
        revision,
        facts,
    ))
}

fn workspace_revision_fence_is_registered_for_root(
    revision: &Arc<WorkspaceRevisionFence>,
    root: &WorkspaceRoot,
) -> bool {
    let Ok(identity) = root.identity() else {
        return false;
    };
    WorkspaceRevisionFence::is_registered_for_identity(revision, identity)
}

fn begin_managed_workspace_mutation<'a>(
    revision: &'a Arc<WorkspaceRevisionFence>,
    root: &WorkspaceRoot,
) -> Result<RwLockWriteGuard<'a, ()>, ()> {
    if !workspace_revision_fence_is_registered_for_root(revision, root) {
        return Err(());
    }
    revision.begin_mutation()
}

struct ProcessSpawnGateContext {
    state: Arc<Mutex<ProcessGrantState>>,
    expected: ProcessGrant,
    expected_intent: ProcessIntent,
    expected_digest: [u8; 32],
    revision: Arc<WorkspaceRevisionFence>,
    root: Arc<WorkspaceRoot>,
    session_id: Uuid,
    test_filter: Option<String>,
}

fn process_spawn_gate(context: ProcessSpawnGateContext) -> SpawnAuthorizationGate {
    let ProcessSpawnGateContext {
        state,
        expected,
        expected_intent,
        expected_digest,
        revision,
        root,
        session_id,
        test_filter,
    } = context;
    Box::new(move |command, spawn_action| {
        if !workspace_revision_fence_is_registered_for_root(&revision, &root) {
            return Err(SpawnGateError::Denied);
        }
        let Ok(mut state) = state.lock() else {
            return Err(SpawnGateError::Denied);
        };
        if !launch_command_matches_intent(&command, &expected_intent) {
            return Err(SpawnGateError::Denied);
        }
        if state.epoch != expected.epoch
            || expected.grant_id != expected_intent.grant_id
            || expected.epoch != expected_intent.grant_epoch
            || expected.mode != expected_intent.mode
            || expected.scope != expected_intent.scope
            || expected.task_id != expected_intent.task_id
            || session_id != expected_intent.session_id
        {
            return Err(SpawnGateError::Denied);
        }
        let Some(active) = state.current.as_ref() else {
            return Err(SpawnGateError::Denied);
        };
        if active.task_id != expected.task_id
            || active.grant_id != expected.grant_id
            || active.epoch != expected.epoch
            || active.mode != expected.mode
            || active.scope != expected.scope
            || active.exact_filter != expected.exact_filter
            || active.expiry != expected.expiry
            || !active.allows_filter(test_filter.as_deref())
            || active.expired()
            || !Arc::ptr_eq(&active.policy, &expected.policy)
        {
            return Err(SpawnGateError::Denied);
        }
        let current_intent = process_intent(
            active,
            session_id,
            test_filter.as_deref(),
            &root,
            revision.current_revision(),
        )
        .ok();
        let current_digest = current_intent.and_then(|intent| intent.digest().ok());
        if expected_intent.digest().ok() != Some(expected_digest) {
            return Err(SpawnGateError::Denied);
        }
        match start_if_authorized(
            &mut state,
            &expected,
            expected_digest,
            current_digest,
            || spawn_action(command),
        ) {
            Ok(child) => Ok(child),
            Err(ProcessStartError::Denied) => Err(SpawnGateError::Denied),
            Err(ProcessStartError::Spawn(error)) => Err(SpawnGateError::Spawn(error)),
        }
    })
}

impl Default for TaskSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod process_grant_tests {
    use super::*;
    use crate::core::lifecycle::process_authorization::{GrantMode, GrantScope};
    use crate::test_support::TempDir;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::Ordering;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    fn policy_fixture() -> (TempDir, crate::OfflineCargoPolicy) {
        let runtime = TempDir::with_prefix("process-grant-api-");
        let toolchain = runtime.path().join("toolchain");
        let registry = runtime.path().join("registry");
        fs::create_dir_all(toolchain.join("bin")).unwrap();
        fs::create_dir_all(&registry).unwrap();
        for name in ["cargo", "rustc"] {
            fs::write(toolchain.join("bin").join(name), b"\x7fELF").unwrap();
        }
        let policy = crate::OfflineCargoPolicy::new(&toolchain, &registry).unwrap();
        (runtime, policy)
    }

    fn running_turn(session: &mut TaskSession, task_id: Uuid) -> Uuid {
        let task = session.get_task_mut(task_id).unwrap();
        task.transition(TaskState::Running).unwrap();
        let turn = task.create_turn().unwrap();
        task.transition_turn(turn, TurnState::Running).unwrap();
        turn
    }

    fn command_for_intent(intent: &ProcessIntent) -> Command {
        let mut command = Command::new("/usr/bin/bwrap");
        for (fd, path) in krypper_sandbox::runner_contract::FIXED_FD_MOUNTS {
            command.arg("--ro-bind-fd").arg(fd.to_string()).arg(path);
        }
        command.arg("--clearenv");
        for (name, value) in krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT {
            command.args(["--setenv", name, value]);
        }
        command.args([
            "--chdir",
            krypper_sandbox::runner_contract::WORKSPACE_MOUNT.1,
            "--",
            krypper_sandbox::runner_contract::RUNNER_MOUNT_PATH,
            "--krypper-landlock-runner-v1",
            "--status-fd",
        ]);
        command
            .arg(krypper_sandbox::runner_protocol::STATUS_FD.to_string())
            .arg("--ack-fd")
            .arg(krypper_sandbox::runner_protocol::ACK_FD.to_string())
            .args(["--optional", "--"]);
        command.arg(&intent.facts.executable);
        command.args(&intent.facts.arguments);
        if let Some(filter) = &intent.filter {
            command.arg("--").arg(filter);
        }
        command
    }

    fn command_with_extra_separator(intent: &ProcessIntent) -> Command {
        let mut command = Command::new("/usr/bin/bwrap");
        command.arg("--").arg("--clearenv");
        for (fd, path) in krypper_sandbox::runner_contract::FIXED_FD_MOUNTS {
            command.arg("--ro-bind-fd").arg(fd.to_string()).arg(path);
        }
        for (name, value) in krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT {
            command.args(["--setenv", name, value]);
        }
        command.args([
            "--chdir",
            krypper_sandbox::runner_contract::WORKSPACE_MOUNT.1,
            "--",
            krypper_sandbox::runner_contract::RUNNER_MOUNT_PATH,
            "--krypper-landlock-runner-v1",
            "--status-fd",
        ]);
        command
            .arg(krypper_sandbox::runner_protocol::STATUS_FD.to_string())
            .arg("--ack-fd")
            .arg(krypper_sandbox::runner_protocol::ACK_FD.to_string())
            .args(["--optional", "--"]);
        command.arg(&intent.facts.executable);
        command.args(&intent.facts.arguments);
        if let Some(filter) = &intent.filter {
            command.arg("--").arg(filter);
        }
        command
    }

    #[test]
    fn public_grant_apis_preserve_session_scope_validate_filter_expiry_and_revoke() {
        let workspace = TempDir::with_prefix("process-grant-workspace-");
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let task_id = session.create_task("grant API fixture").id();

        let (_runtime, policy) = policy_fixture();
        session
            .authorize_offline_cargo_tests(task_id, policy)
            .unwrap();
        let grant_state = session
            .process_grants
            .lock()
            .unwrap()
            .get(&task_id)
            .unwrap()
            .clone();
        let cancellation = {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            assert_eq!(grant.mode, GrantMode::Standing);
            assert_eq!(grant.scope, GrantScope::Session);
            assert!(grant.expiry.is_none());
            assert!(grant.allows_filter(None));
            assert!(grant.allows_filter(Some("any_valid_filter")));
            grant.policy.cancellation()
        };

        let (_runtime, policy) = policy_fixture();
        session
            .authorize_offline_cargo_tests_scoped(
                task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(cancellation.load(Ordering::SeqCst));
        {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            assert_eq!(grant.mode, GrantMode::Standing);
            assert_eq!(grant.scope, GrantScope::ExactFilter);
            assert_eq!(grant.exact_filter.as_deref(), Some("selected_case"));
            assert!(grant.expiry.is_some());
            assert!(grant.allows_filter(Some("selected_case")));
            assert!(!grant.allows_filter(Some("other_case")));
            assert!(!grant.allows_filter(None));
        }

        let (_runtime, policy) = policy_fixture();
        assert!(session
            .authorize_offline_cargo_tests_scoped(
                task_id,
                policy,
                Some("--help"),
                Duration::from_secs(60),
            )
            .is_err());
        let (_runtime, policy) = policy_fixture();
        assert!(session
            .authorize_offline_cargo_test_once(
                task_id,
                policy,
                Some("selected_case"),
                Duration::ZERO
            )
            .is_err());

        let (_runtime, policy) = policy_fixture();
        assert!(session
            .authorize_offline_cargo_tests_scoped(
                task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(24 * 60 * 60 + 1),
            )
            .is_err());

        let (_runtime, policy) = policy_fixture();
        session
            .authorize_offline_cargo_test_once(
                task_id,
                policy,
                Some("selected_case"),
                Duration::from_millis(1),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let expired_grant_cancellation = {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            assert_eq!(grant.mode, GrantMode::OneShot);
            assert!(grant.expired());
            grant.policy.cancellation()
        };

        let active_cancellation = grant_state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .policy
            .cancellation();
        session.revoke_offline_cargo_tests(task_id).unwrap();
        assert!(active_cancellation.load(Ordering::SeqCst));
        assert!(expired_grant_cancellation.load(Ordering::SeqCst));
        let state = grant_state.lock().unwrap();
        assert!(state.current.is_none());
        assert_eq!(state.epoch, 4);
    }

    #[test]
    fn one_shot_install_does_not_hold_grant_lock_while_waiting_on_run_revision_lease() {
        let workspace = TempDir::with_prefix("process-grant-lock-order-");
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let task_id = session.create_task("grant lock ordering fixture").id();
        let revision = Arc::clone(&session.get_task(task_id).unwrap().workspace_revision);
        let (_old_runtime, old_policy) = policy_fixture();
        session
            .authorize_offline_cargo_tests(task_id, old_policy)
            .unwrap();
        let state = session
            .process_grants
            .lock()
            .unwrap()
            .get(&task_id)
            .unwrap()
            .clone();
        let old_cancellation = state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .policy
            .cancellation();

        let (run_lease, _) = revision.begin_read().unwrap();
        let writer_revision = Arc::clone(&revision);
        let (writer_done_tx, writer_done_rx) = mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            {
                let _mutation = writer_revision.begin_mutation().unwrap();
            }
            writer_done_tx.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while revision.pending_mutations() == 0 && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        let mutation_queued = revision.pending_mutations() == 1;

        let (_runtime, policy) = policy_fixture();
        let read_attempt = revision.pause_next_read_for_test();
        let (installer_done_tx, installer_done_rx) = mpsc::sync_channel(1);
        let installer = thread::spawn(move || {
            let result = session.authorize_offline_cargo_test_once(
                task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            );
            installer_done_tx.send(result).unwrap();
        });

        let read_attempt_result = read_attempt.wait_until_read_attempted();
        if read_attempt_result.is_err() {
            read_attempt.allow_read();
            drop(run_lease);
            let _ = installer_done_rx.recv_timeout(Duration::from_secs(10));
            let _ = writer_done_rx.recv_timeout(Duration::from_secs(10));
            panic!("one-shot installer did not reach the workspace read lease");
        }
        read_attempt_result
            .expect("one-shot installer must reach the workspace read lease before probing state");
        let gate_state_available = state.try_lock().is_ok();
        read_attempt.allow_read();
        drop(run_lease);
        let install_result = installer_done_rx.recv_timeout(Duration::from_secs(10));
        let writer_finished = writer_done_rx.recv_timeout(Duration::from_secs(10)).is_ok();
        if install_result.is_ok() {
            installer.join().unwrap();
        }
        if writer_finished {
            writer.join().unwrap();
        }
        assert!(
            mutation_queued,
            "workspace mutation writer should be queued"
        );
        assert!(
            gate_state_available,
            "grant installer must release its state lock before waiting for workspace access"
        );
        install_result
            .expect("one-shot grant installation should finish within the timeout")
            .unwrap();
        assert!(writer_finished, "queued workspace mutation should finish");
        assert!(old_cancellation.load(Ordering::SeqCst));
        let state = state.lock().unwrap();
        assert_eq!(state.epoch, 2);
        let grant = state.current.as_ref().unwrap();
        assert_eq!(grant.mode, GrantMode::OneShot);
        assert_eq!(grant.scope, GrantScope::ExactFilter);
        assert_eq!(grant.exact_filter.as_deref(), Some("selected_case"));
    }

    #[test]
    fn workspace_mutation_invalidates_one_shot_approval_before_run() {
        let workspace = TempDir::with_prefix("process-grant-revision-");
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let task_id = session.create_task("one-shot revision fixture").id();
        let (_runtime, policy) = policy_fixture();
        session
            .authorize_offline_cargo_test_once(
                task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .unwrap();
        let task = session.get_task(task_id).unwrap();
        let initial_revision = task.workspace_revision.current_revision();
        let grant_state = session
            .process_grants
            .lock()
            .unwrap()
            .get(&task_id)
            .unwrap()
            .clone();
        let approved_digest = grant_state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .approved_digest
            .unwrap();

        let turn_id = running_turn(&mut session, task_id);
        session
            .create_workspace_file(task_id, turn_id, Path::new("created.txt"), "new content")
            .unwrap();
        let task = session.get_task(task_id).unwrap();
        let current_revision = task.workspace_revision.current_revision();
        assert_eq!(current_revision, initial_revision + 1);
        {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            let root = task.workspace_root.as_ref().unwrap();
            let identity = root.identity().unwrap();
            let current_intent = process_intent(
                grant,
                session.id,
                Some("selected_case"),
                root,
                current_revision,
            )
            .unwrap();
            assert_ne!(current_intent.digest().unwrap(), approved_digest);
            assert_eq!(
                grant.approved_digest,
                Some(
                    process_intent(
                        grant,
                        session.id,
                        Some("selected_case"),
                        root,
                        initial_revision,
                    )
                    .unwrap()
                    .digest()
                    .unwrap()
                )
            );
            assert_eq!(identity, current_intent.workspace_identity);
        }

        let result = session
            .run_offline_cargo_tests(task_id, turn_id, Some("selected_case"))
            .unwrap();
        assert_eq!(result.outcome, crate::ProcessOutcome::Denied);
        assert!(
            !grant_state
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .consumed
        );
    }

    #[test]
    fn sibling_task_workspace_mutation_invalidates_one_shot_approval() {
        let workspace = TempDir::with_prefix("process-grant-sibling-revision-");
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let authorized_task_id = session.create_task("authorized task").id();
        let sibling_task_id = session.create_task("sibling writer task").id();
        let authorized_revision = Arc::clone(
            &session
                .get_task(authorized_task_id)
                .unwrap()
                .workspace_revision,
        );
        let sibling_revision = Arc::clone(
            &session
                .get_task(sibling_task_id)
                .unwrap()
                .workspace_revision,
        );
        assert!(Arc::ptr_eq(&authorized_revision, &sibling_revision));

        let (_runtime, policy) = policy_fixture();
        session
            .authorize_offline_cargo_test_once(
                authorized_task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .unwrap();
        let authorized_turn = running_turn(&mut session, authorized_task_id);
        let sibling_turn = running_turn(&mut session, sibling_task_id);
        session
            .create_workspace_file(
                sibling_task_id,
                sibling_turn,
                Path::new("sibling-created.txt"),
                "changed by another task",
            )
            .unwrap();

        assert_eq!(authorized_revision.current_revision(), 1);
        let result = session
            .run_offline_cargo_tests(authorized_task_id, authorized_turn, Some("selected_case"))
            .unwrap();
        assert_eq!(result.outcome, crate::ProcessOutcome::Denied);
        assert!(
            !session
                .process_grants
                .lock()
                .unwrap()
                .get(&authorized_task_id)
                .unwrap()
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .consumed
        );
    }

    #[test]
    fn another_session_mutation_invalidates_one_shot_for_same_workspace() {
        let workspace = TempDir::with_prefix("process-grant-other-session-revision-");
        let mut authorized_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let mut writer_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let authorized_task_id = authorized_session.create_task("authorized task").id();
        let writer_task_id = writer_session.create_task("writer task").id();
        let authorized_revision = Arc::clone(
            &authorized_session
                .get_task(authorized_task_id)
                .unwrap()
                .workspace_revision,
        );
        let writer_revision = Arc::clone(
            &writer_session
                .get_task(writer_task_id)
                .unwrap()
                .workspace_revision,
        );
        assert!(Arc::ptr_eq(&authorized_revision, &writer_revision));

        let (_runtime, policy) = policy_fixture();
        authorized_session
            .authorize_offline_cargo_test_once(
                authorized_task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .unwrap();
        let authorized_turn = running_turn(&mut authorized_session, authorized_task_id);
        let writer_turn = running_turn(&mut writer_session, writer_task_id);
        writer_session
            .create_workspace_file(
                writer_task_id,
                writer_turn,
                Path::new("other-session-created.txt"),
                "changed by another session",
            )
            .unwrap();

        let result = authorized_session
            .run_offline_cargo_tests(authorized_task_id, authorized_turn, Some("selected_case"))
            .unwrap();
        assert_eq!(result.outcome, crate::ProcessOutcome::Denied);
        assert!(
            !authorized_session
                .process_grants
                .lock()
                .unwrap()
                .get(&authorized_task_id)
                .unwrap()
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .consumed
        );
    }

    #[test]
    fn recovered_workspace_identity_cannot_authorize_with_a_private_fallback_fence() {
        let workspace = TempDir::with_prefix("process-grant-fallback-fence-");
        let mut authorized_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let mut writer_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let authorized_task_id = authorized_session.create_task("authorized task").id();
        let writer_task_id = writer_session.create_task("writer task").id();

        let shared_revision = Arc::clone(
            &authorized_session
                .get_task(authorized_task_id)
                .unwrap()
                .workspace_revision,
        );
        let writer_revision = Arc::clone(
            &writer_session
                .get_task(writer_task_id)
                .unwrap()
                .workspace_revision,
        );
        assert!(Arc::ptr_eq(&shared_revision, &writer_revision));

        let (_runtime, policy) = policy_fixture();
        authorized_session
            .authorize_offline_cargo_tests(authorized_task_id, policy)
            .unwrap();

        // A private default fence models identity lookup failing at task creation and
        // succeeding later. Such a task can still use workspace helpers, but process
        // authorization must not treat its task-local revision as workspace-wide.
        let fallback_revision = Arc::new(WorkspaceRevisionFence::default());
        authorized_session
            .get_task_mut(authorized_task_id)
            .unwrap()
            .workspace_revision = Arc::clone(&fallback_revision);
        let root = authorized_session
            .get_task(authorized_task_id)
            .unwrap()
            .workspace_root
            .as_ref()
            .unwrap();
        assert!(!workspace_revision_fence_is_registered_for_root(
            &fallback_revision,
            root
        ));

        let (_runtime, policy) = policy_fixture();
        assert!(authorized_session
            .authorize_offline_cargo_test_once(
                authorized_task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .is_err());

        let writer_turn = running_turn(&mut writer_session, writer_task_id);
        writer_session
            .create_workspace_file(
                writer_task_id,
                writer_turn,
                Path::new("fallback-fence-write.txt"),
                "managed mutation",
            )
            .unwrap();
        assert_eq!(writer_revision.current_revision(), 1);
        assert_eq!(fallback_revision.current_revision(), 0);

        let authorized_turn = running_turn(&mut authorized_session, authorized_task_id);
        let result = authorized_session
            .run_offline_cargo_tests(authorized_task_id, authorized_turn, None)
            .unwrap();
        assert_eq!(result.outcome, crate::ProcessOutcome::Denied);
    }

    #[test]
    fn managed_workspace_mutations_fail_closed_with_a_private_fallback_fence() {
        let workspace = TempDir::with_prefix("process-grant-fallback-writer-fence-");
        fs::write(workspace.path().join("existing.txt"), "before\n").unwrap();
        let mut authorized_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let mut writer_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let authorized_task_id = authorized_session.create_task("authorized task").id();
        let writer_task_id = writer_session.create_task("writer task").id();

        let shared_revision = Arc::clone(
            &authorized_session
                .get_task(authorized_task_id)
                .unwrap()
                .workspace_revision,
        );
        let registered_writer_revision = Arc::clone(
            &writer_session
                .get_task(writer_task_id)
                .unwrap()
                .workspace_revision,
        );
        assert!(Arc::ptr_eq(&shared_revision, &registered_writer_revision));

        let (_runtime, policy) = policy_fixture();
        authorized_session
            .authorize_offline_cargo_test_once(
                authorized_task_id,
                policy,
                Some("selected_case"),
                Duration::from_secs(60),
            )
            .unwrap();
        let grant_state = authorized_session
            .process_grants
            .lock()
            .unwrap()
            .get(&authorized_task_id)
            .unwrap()
            .clone();
        let approved_digest = grant_state
            .lock()
            .unwrap()
            .current
            .as_ref()
            .unwrap()
            .approved_digest
            .unwrap();

        // Model a task created while identity lookup failed, then used after it recovers.
        let fallback_revision = Arc::new(WorkspaceRevisionFence::default());
        writer_session
            .get_task_mut(writer_task_id)
            .unwrap()
            .workspace_revision = Arc::clone(&fallback_revision);
        let writer_turn = running_turn(&mut writer_session, writer_task_id);

        // An unregistered fence must not bypass a process read lease held on the shared fence.
        let (read_lease, revision_at_approval) = shared_revision.begin_read().unwrap();
        assert_eq!(revision_at_approval, 0);
        assert!(writer_session
            .create_workspace_directory(writer_task_id, writer_turn, Path::new("new-directory"),)
            .is_err());
        assert!(writer_session
            .create_workspace_file(
                writer_task_id,
                writer_turn,
                Path::new("new-file.txt"),
                "new content",
            )
            .is_err());
        assert!(writer_session
            .replace_workspace_text_with_revision(
                writer_task_id,
                writer_turn,
                Path::new("existing.txt"),
                "before",
                "replaced",
                None,
            )
            .is_err());
        assert!(writer_session
            .apply_workspace_patch_with_revision(
                writer_task_id,
                writer_turn,
                Path::new("existing.txt"),
                "--- a/existing.txt\n+++ b/existing.txt\n@@ -1 +1 @@\n-before\n+patched\n",
                None,
            )
            .is_err());
        drop(read_lease);

        assert!(!workspace.path().join("new-directory").exists());
        assert!(!workspace.path().join("new-file.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.path().join("existing.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(shared_revision.current_revision(), 0);
        assert_eq!(fallback_revision.current_revision(), 0);
        let state = grant_state.lock().unwrap();
        let grant = state.current.as_ref().unwrap();
        assert_eq!(grant.approved_digest, Some(approved_digest));
        assert!(!grant.consumed);
    }

    #[test]
    fn sibling_task_workspace_mutation_waits_for_process_read_lease() {
        let workspace = TempDir::with_prefix("process-grant-sibling-lease-");
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let first_task_id = session.create_task("process task").id();
        let sibling_task_id = session.create_task("sibling writer task").id();
        let first_revision =
            Arc::clone(&session.get_task(first_task_id).unwrap().workspace_revision);
        let sibling_revision = Arc::clone(
            &session
                .get_task(sibling_task_id)
                .unwrap()
                .workspace_revision,
        );
        assert!(Arc::ptr_eq(&first_revision, &sibling_revision));

        let (read_lease, _) = first_revision.begin_read().unwrap();
        let mutation_revision = Arc::clone(&sibling_revision);
        let (mutation_done, mutation_result) = mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            let _mutation = mutation_revision.begin_mutation().unwrap();
            mutation_done.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sibling_revision.pending_mutations() == 0 && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(sibling_revision.pending_mutations(), 1);
        assert!(mutation_result.try_recv().is_err());

        drop(read_lease);
        mutation_result
            .recv_timeout(Duration::from_secs(2))
            .expect("sibling mutation should proceed after the process read lease ends");
        writer.join().unwrap();
        assert_eq!(first_revision.current_revision(), 1);
    }

    #[test]
    fn product_spawn_gate_rechecks_workspace_fence_and_consumes_one_shot_at_spawn() {
        let workspace = TempDir::with_prefix("process-grant-gate-workspace-");
        let root = Arc::new(capture_workspace_root_at_with_error(workspace.path()).unwrap());
        let identity = root.identity().unwrap();
        let revision = WorkspaceRevisionFence::for_identity(identity);
        let (_runtime, policy) = policy_fixture();
        let expiry = GrantExpiry::from_lifetime(Duration::from_secs(60)).unwrap();
        let mut grant = ProcessGrant {
            task_id: Uuid::from_u128(501),
            grant_id: Uuid::from_u128(502),
            epoch: 1,
            mode: GrantMode::OneShot,
            scope: GrantScope::ExactFilter,
            exact_filter: Some("selected_case".into()),
            expiry: Some(expiry),
            policy: Arc::new(policy),
            consumed: false,
            approved_digest: None,
        };
        let session_id = Uuid::from_u128(503);
        let intent = process_intent(
            &grant,
            session_id,
            Some("selected_case"),
            &root,
            revision.current_revision(),
        )
        .unwrap();
        let digest = intent.digest().unwrap();
        grant.approved_digest = Some(digest);
        let state = Arc::new(Mutex::new(ProcessGrantState {
            epoch: grant.epoch,
            current: Some(grant.clone()),
        }));
        let gate = process_spawn_gate(ProcessSpawnGateContext {
            state: Arc::clone(&state),
            expected: grant.clone(),
            expected_intent: intent.clone(),
            expected_digest: digest,
            revision: Arc::clone(&revision),
            root: Arc::clone(&root),
            session_id,
            test_filter: Some("selected_case".into()),
        });
        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_calls_for_denied = Arc::clone(&spawn_calls);
        let denied = gate(
            command_with_extra_separator(&intent),
            Box::new(move |_command| {
                spawn_calls_for_denied.fetch_add(1, Ordering::SeqCst);
                Err::<std::process::Child, _>(std::io::Error::other("injected spawn failure"))
            }),
        );
        assert!(matches!(denied, Err(SpawnGateError::Denied)));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        assert!(!state.lock().unwrap().current.as_ref().unwrap().consumed);

        let fallback_revision = Arc::new(WorkspaceRevisionFence::default());
        assert!(!workspace_revision_fence_is_registered_for_root(
            &fallback_revision,
            &root
        ));
        let fence_gate = process_spawn_gate(ProcessSpawnGateContext {
            state: Arc::clone(&state),
            expected: grant.clone(),
            expected_intent: intent.clone(),
            expected_digest: digest,
            revision: fallback_revision,
            root: Arc::clone(&root),
            session_id,
            test_filter: Some("selected_case".into()),
        });
        let spawn_calls_for_fence = Arc::clone(&spawn_calls);
        let fence_denied = fence_gate(
            command_for_intent(&intent),
            Box::new(move |_command| {
                spawn_calls_for_fence.fetch_add(1, Ordering::SeqCst);
                Err::<std::process::Child, _>(std::io::Error::other("injected spawn failure"))
            }),
        );
        assert!(matches!(fence_denied, Err(SpawnGateError::Denied)));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        assert!(!state.lock().unwrap().current.as_ref().unwrap().consumed);

        let gate = process_spawn_gate(ProcessSpawnGateContext {
            state: Arc::clone(&state),
            expected: grant,
            expected_intent: intent.clone(),
            expected_digest: digest,
            revision,
            root,
            session_id,
            test_filter: Some("selected_case".into()),
        });
        let spawn_calls_for_failed = Arc::clone(&spawn_calls);
        let result = gate(
            command_for_intent(&intent),
            Box::new(move |_command| {
                spawn_calls_for_failed.fetch_add(1, Ordering::SeqCst);
                Err::<std::process::Child, _>(std::io::Error::other("injected spawn failure"))
            }),
        );

        assert!(matches!(result, Err(SpawnGateError::Spawn(_))));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
        assert!(state.lock().unwrap().current.as_ref().unwrap().consumed);
    }
}
