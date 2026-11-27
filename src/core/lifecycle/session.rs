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
    search_tree_text_files_at, FileIdentity, PatchEvidence, WorkspaceRoot,
};
use krypper_sandbox::{SpawnAuthorizationGate, SpawnGateError};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Ephemeral host-selected mode snapshots. They are never checkpointed or restored.
    pub(super) permission_modes:
        Arc<Mutex<std::collections::HashMap<Uuid, TaskPermissionSnapshot>>>,
    #[cfg(test)]
    pub(super) test_process_results: std::cell::RefCell<
        std::collections::HashMap<Uuid, std::collections::VecDeque<crate::ProcessResult>>,
    >,
    #[cfg(test)]
    pub(super) test_process_filters:
        std::cell::RefCell<std::collections::HashMap<Uuid, Vec<Option<String>>>>,
}

#[derive(Debug)]
pub(super) struct TaskPermissionSnapshot {
    mode: crate::tools::permission_policy::PermissionMode,
    full_access_confirmed: bool,
    generation: u64,
    task_cancellation: Arc<AtomicBool>,
    active_host_cancellation: Option<Arc<AtomicBool>>,
    host_process_recovery_hold: bool,
}

struct ProcessGrantSpec {
    mode: GrantMode,
    scope: GrantScope,
    exact_filter: Option<String>,
    expiry: Option<GrantExpiry>,
    read_only_exec: Option<crate::tools::process::OfflineReadOnlyExecRequest>,
    source_grant_id: Option<Uuid>,
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
            permission_modes: Arc::new(Mutex::default()),
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
            permission_modes: Arc::new(Mutex::default()),
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

    /// Installs the host-selected, task-local permission snapshot.
    ///
    /// The Full Access confirmation is intentionally ephemeral and must be supplied by a
    /// trusted host interaction for this task. Restored checkpoints have no snapshot and must
    /// be configured again by the host.
    pub(crate) fn set_task_permission_mode(
        &self,
        task_id: Uuid,
        mode: crate::tools::permission_policy::PermissionMode,
        full_access_confirmed: bool,
    ) -> Result<(), &'static str> {
        if self.get_task(task_id).is_none() {
            return Err("task not found");
        }
        if (mode == crate::tools::permission_policy::PermissionMode::FullAccess)
            != full_access_confirmed
        {
            return Err("Full Access requires a trusted per-task confirmation");
        }
        let mut snapshots = self
            .permission_modes
            .lock()
            .map_err(|_| "task permission state is unavailable")?;
        let changed = snapshots.get(&task_id).is_none_or(|snapshot| {
            snapshot.mode != mode || snapshot.full_access_confirmed != full_access_confirmed
        });
        if changed {
            if let Some(snapshot) = snapshots.get_mut(&task_id) {
                snapshot.task_cancellation.store(true, Ordering::SeqCst);
                if let Some(cancellation) = snapshot.active_host_cancellation.as_ref() {
                    cancellation.store(true, Ordering::SeqCst);
                }
                snapshot.generation = snapshot.generation.wrapping_add(1).max(1);
                snapshot.mode = mode;
                snapshot.full_access_confirmed = full_access_confirmed;
                snapshot.task_cancellation = Arc::new(AtomicBool::new(false));
            } else {
                snapshots.insert(
                    task_id,
                    TaskPermissionSnapshot {
                        mode,
                        full_access_confirmed,
                        generation: 1,
                        task_cancellation: Arc::new(AtomicBool::new(false)),
                        active_host_cancellation: None,
                        host_process_recovery_hold: false,
                    },
                );
            }
        }
        drop(snapshots);
        if changed {
            self.revoke_offline_cargo_tests(task_id)?;
        }
        Ok(())
    }

    /// Returns the task-scoped cancellation flag used by the CLI signal handler and host runner.
    pub(crate) fn task_cancellation_flag(&self, task_id: Uuid) -> Option<Arc<AtomicBool>> {
        self.permission_modes
            .lock()
            .ok()?
            .get(&task_id)
            .map(|snapshot| Arc::clone(&snapshot.task_cancellation))
    }

    /// Reports whether the host has requested cancellation for this task.
    pub(crate) fn task_cancellation_requested(&self, task_id: Uuid) -> bool {
        self.task_cancellation_flag(task_id)
            .is_some_and(|cancellation| cancellation.load(Ordering::SeqCst))
    }

    pub(crate) fn task_has_confirmed_full_access(&self, task_id: Uuid) -> bool {
        self.permission_modes.lock().ok().is_some_and(|snapshots| {
            snapshots.get(&task_id).is_some_and(|snapshot| {
                snapshot.mode == crate::tools::permission_policy::PermissionMode::FullAccess
                    && snapshot.full_access_confirmed
            })
        })
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
        if let Ok(snapshots) = self.permission_modes.lock() {
            if let Some(snapshot) = snapshots.get(&task_id) {
                snapshot.task_cancellation.store(true, Ordering::SeqCst);
                if let Some(cancellation) = snapshot.active_host_cancellation.as_ref() {
                    cancellation.store(true, Ordering::SeqCst);
                }
            }
        }
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

    /// Captures the current managed revision for an approved registered extension effect.
    pub(crate) fn extension_workspace_snapshot(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
    ) -> Result<(FileIdentity, u64), WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReadError::File {
                message: error.message().to_owned(),
            })?;
        if !workspace_revision_fence_is_registered_for_root(&task.workspace_revision, root) {
            return Err(WorkspaceReadError::File {
                message: "workspace revision state is unavailable".into(),
            });
        }
        let (_read_lease, revision) =
            task.workspace_revision
                .begin_read()
                .map_err(|_| WorkspaceReadError::File {
                    message: "workspace revision state is unavailable".into(),
                })?;
        let identity = root.identity().map_err(|error| WorkspaceReadError::File {
            message: error.to_string(),
        })?;
        Ok((identity, revision))
    }

    /// Runs one effect only while the captured workspace identity and revision are still current.
    /// The read lease stays held for the full synchronous effect, excluding managed writers.
    pub(crate) fn with_extension_workspace_snapshot<T>(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        expected_identity: FileIdentity,
        expected_revision: u64,
        effect: impl FnOnce() -> T,
    ) -> Result<Option<T>, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReadError::File {
                message: error.message().to_owned(),
            })?;
        if !workspace_revision_fence_is_registered_for_root(&task.workspace_revision, root) {
            return Err(WorkspaceReadError::File {
                message: "workspace revision state is unavailable".into(),
            });
        }
        let (_read_lease, current_revision) =
            task.workspace_revision
                .begin_read()
                .map_err(|_| WorkspaceReadError::File {
                    message: "workspace revision state is unavailable".into(),
                })?;
        let current_identity = root.identity().map_err(|error| WorkspaceReadError::File {
            message: error.to_string(),
        })?;
        if current_identity != expected_identity || current_revision != expected_revision {
            return Ok(None);
        }
        Ok(Some(effect()))
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
            ProcessGrantSpec {
                mode: GrantMode::Standing,
                scope: GrantScope::Session,
                exact_filter: None,
                expiry: None,
                read_only_exec: None,
                source_grant_id: None,
            },
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
            ProcessGrantSpec {
                mode: GrantMode::Standing,
                scope: GrantScope::ExactFilter,
                exact_filter: exact_filter.map(str::to_owned),
                expiry: Some(expiry),
                read_only_exec: None,
                source_grant_id: None,
            },
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
            ProcessGrantSpec {
                mode: GrantMode::OneShot,
                scope: GrantScope::ExactFilter,
                exact_filter: exact_filter.map(str::to_owned),
                expiry: Some(expiry),
                read_only_exec: None,
                source_grant_id: None,
            },
        )
    }

    /// Grants one exact direct-argv request after the trusted host obtains approval.
    /// The grant is one-shot, bound to the current workspace revision, and expires within 24 hours.
    pub fn authorize_offline_read_only_exec_once(
        &self,
        task_id: Uuid,
        policy: crate::OfflineCargoPolicy,
        request: crate::tools::process::OfflineReadOnlyExecRequest,
        lifetime: Duration,
    ) -> Result<(), &'static str> {
        let expiry = GrantExpiry::from_lifetime(lifetime)?;
        self.install_process_grant(
            task_id,
            Arc::new(policy),
            ProcessGrantSpec {
                mode: GrantMode::OneShot,
                scope: GrantScope::ExactIntent,
                exact_filter: None,
                expiry: Some(expiry),
                read_only_exec: Some(request),
                source_grant_id: None,
            },
        )
    }

    /// Converts a current host-installed process policy into one exact direct-argv grant after
    /// approval. The source grant must remain current and nonexpired through the replacement.
    pub fn authorize_offline_read_only_exec_once_from_current_policy(
        &self,
        task_id: Uuid,
        request: crate::tools::process::OfflineReadOnlyExecRequest,
        lifetime: Duration,
    ) -> Result<(), &'static str> {
        let expiry = GrantExpiry::from_lifetime(lifetime)?;
        let grants = self
            .process_grants
            .lock()
            .map_err(|_| "process grant state is unavailable")?;
        let state = grants
            .get(&task_id)
            .cloned()
            .ok_or("trusted process policy is unavailable")?;
        drop(grants);
        let (policy, source_grant_id) = {
            let state = state
                .lock()
                .map_err(|_| "process grant state is unavailable")?;
            let grant = state
                .current
                .as_ref()
                .filter(|grant| {
                    grant.task_id == task_id
                        && !grant.expired()
                        && !grant.consumed
                        && !grant
                            .policy
                            .cancellation()
                            .load(std::sync::atomic::Ordering::SeqCst)
                })
                .ok_or("trusted process policy is unavailable")?;
            (grant.policy.fork_for_new_grant()?, grant.grant_id)
        };
        self.install_process_grant(
            task_id,
            Arc::new(policy),
            ProcessGrantSpec {
                mode: GrantMode::OneShot,
                scope: GrantScope::ExactIntent,
                exact_filter: None,
                expiry: Some(expiry),
                read_only_exec: Some(request),
                source_grant_id: Some(source_grant_id),
            },
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
        spec: ProcessGrantSpec,
    ) -> Result<(), &'static str> {
        let ProcessGrantSpec {
            mode,
            scope,
            exact_filter,
            expiry,
            read_only_exec,
            source_grant_id,
        } = spec;
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
        let scoped = matches!(scope, GrantScope::ExactFilter | GrantScope::ExactIntent);
        if expiry.is_some() != scoped
            || (scope == GrantScope::ExactIntent
                && (mode != GrantMode::OneShot
                    || exact_filter.is_some()
                    || read_only_exec.is_none()))
            || (scope != GrantScope::ExactIntent && read_only_exec.is_some())
        {
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
            if let Some(source_grant_id) = source_grant_id {
                let source_is_current = state.current.as_ref().is_some_and(|grant| {
                    grant.grant_id == source_grant_id
                        && !grant.expired()
                        && !grant.consumed
                        && !grant
                            .policy
                            .cancellation()
                            .load(std::sync::atomic::Ordering::SeqCst)
                });
                if !source_is_current {
                    return Err("trusted process policy changed while authorizing");
                }
            }
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
            read_only_exec,
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

        let mut state = state
            .lock()
            .map_err(|_| "process grant state is unavailable")?;
        if let Some(source_grant_id) = source_grant_id {
            let source_is_current = state.current.as_ref().is_some_and(|source| {
                source.grant_id == source_grant_id
                    && !source.expired()
                    && !source.consumed
                    && !source
                        .policy
                        .cancellation()
                        .load(std::sync::atomic::Ordering::SeqCst)
            });
            if !source_is_current {
                return Err("trusted process policy changed while authorizing");
            }
        }
        state.replace_at_epoch(next_epoch, grant)?;
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
            || grant.read_only_exec.is_some()
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
        if self.test_process_results.borrow().contains_key(&task_id) {
            let authorized = state.lock().ok().is_some_and(|mut state| {
                matches!(
                    start_if_authorized(
                        &mut state,
                        &grant,
                        expected_digest,
                        Some(expected_digest),
                        || Ok::<(), ()>(()),
                    ),
                    Ok(())
                )
            });
            if !authorized {
                return Ok(crate::ProcessResult::denied_read_only_exec());
            }
            let mut process_results = self.test_process_results.borrow_mut();
            let Some(results) = process_results.get_mut(&task_id) else {
                return Ok(crate::ProcessResult::denied_read_only_exec());
            };
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

    /// Executes one exactly approved direct-argv request through the existing process grant.
    pub fn run_offline_read_only_exec(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        request: &crate::tools::process::OfflineReadOnlyExecRequest,
    ) -> Result<crate::ProcessResult, WorkspaceReadError> {
        let task = self.running_workspace_task(task_id, turn_id)?;
        let state = self
            .process_grants
            .lock()
            .ok()
            .and_then(|grants| grants.get(&task_id).cloned());
        let Some(state) = state else {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        };
        let grant = state.lock().ok().and_then(|state| {
            state
                .current
                .as_ref()
                .filter(|grant| grant.epoch == state.epoch)
                .cloned()
        });
        let Some(grant) = grant else {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        };
        if grant.task_id != task_id
            || grant.scope != GrantScope::ExactIntent
            || grant.read_only_exec.as_ref() != Some(request)
            || grant.exact_filter.is_some()
            || grant.expired()
            || grant.consumed
            || grant.epoch == 0
        {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        }
        let root = task
            .workspace_root
            .as_ref()
            .map_err(|error| WorkspaceReadError::File {
                message: error.message().to_owned(),
            })?;
        if !workspace_revision_fence_is_registered_for_root(&task.workspace_revision, root) {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        }
        let (revision_guard, revision) =
            task.workspace_revision
                .begin_read()
                .map_err(|_| WorkspaceReadError::File {
                    message: "workspace revision state is unavailable".into(),
                })?;
        let _revision_guard = revision_guard;
        let intent = process_intent(&grant, self.id, None, root, revision);
        let Ok(intent) = intent else {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        };
        let Ok(expected_digest) = intent.digest() else {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        };
        if grant.approved_digest != Some(expected_digest) {
            return Ok(crate::ProcessResult::denied_read_only_exec());
        }
        #[cfg(test)]
        if self.test_process_results.borrow().contains_key(&task_id) {
            let authorized = state.lock().ok().is_some_and(|mut state| {
                matches!(
                    start_if_authorized(
                        &mut state,
                        &grant,
                        expected_digest,
                        Some(expected_digest),
                        || Ok::<(), ()>(()),
                    ),
                    Ok(())
                )
            });
            if !authorized {
                return Ok(crate::ProcessResult::denied_read_only_exec());
            }
            let mut process_results = self.test_process_results.borrow_mut();
            let Some(results) = process_results.get_mut(&task_id) else {
                return Ok(crate::ProcessResult::denied_read_only_exec());
            };
            self.test_process_filters
                .borrow_mut()
                .entry(task_id)
                .or_default()
                .push(None);
            let mut result = results
                .pop_front()
                .unwrap_or_else(crate::ProcessResult::denied_read_only_exec);
            result.profile = crate::ProcessExecutionProfile::OfflineReadOnlyExecV1;
            return Ok(result);
        }
        let spawn_gate = process_spawn_gate(ProcessSpawnGateContext {
            state,
            expected: grant.clone(),
            expected_intent: intent,
            expected_digest,
            revision: Arc::clone(&task.workspace_revision),
            root: Arc::clone(root),
            session_id: self.id,
            test_filter: None,
        });
        Ok(crate::tools::process::run_read_only_exec(
            root,
            &grant.policy,
            request,
            spawn_gate,
        ))
    }

    /// Runs one direct-argv host process under the task's explicitly confirmed Full Access mode.
    /// The task permission snapshot is checked before the runner and again at the spawn boundary.
    pub fn run_full_access_host_process(
        &self,
        task_id: Uuid,
        turn_id: Uuid,
        request: &crate::tools::process::HostProcessRequest,
    ) -> Result<crate::ProcessResult, WorkspaceReadError> {
        self.running_workspace_task(task_id, turn_id)?;
        let (generation, cancellation) = {
            let Ok(mut snapshots) = self.permission_modes.lock() else {
                return Ok(crate::ProcessResult::denied_host_process());
            };
            let Some(snapshot) = snapshots.get_mut(&task_id) else {
                return Ok(crate::ProcessResult::denied_host_process());
            };
            if snapshot.host_process_recovery_hold {
                return Ok(crate::ProcessResult::host_process_recovery_hold());
            }
            if !full_access_host_start_available(snapshot) {
                return Ok(crate::ProcessResult::denied_host_process());
            }
            let cancellation = Arc::clone(&snapshot.task_cancellation);
            snapshot.active_host_cancellation = Some(Arc::clone(&cancellation));
            (snapshot.generation, cancellation)
        };

        let spawn_gate = full_access_host_spawn_gate(
            Arc::clone(&self.permission_modes),
            task_id,
            generation,
            Arc::clone(&cancellation),
        );

        let result =
            crate::tools::process::run_host_process(request, Arc::clone(&cancellation), spawn_gate);
        if let Ok(mut snapshots) = self.permission_modes.lock() {
            if let Some(snapshot) = snapshots.get_mut(&task_id) {
                if snapshot
                    .active_host_cancellation
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &cancellation))
                {
                    snapshot.active_host_cancellation = None;
                }
                if result.recovery_hold {
                    snapshot.host_process_recovery_hold = true;
                }
            }
        }
        Ok(result)
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
    let facts = match grant.read_only_exec.as_ref() {
        Some(request) => grant
            .policy
            .authorization_facts_for_read_only_exec(request)?,
        None => grant.policy.authorization_facts()?,
    };
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
        let active_scope_matches = if expected.read_only_exec.is_some() {
            active.scope == GrantScope::ExactIntent
                && active.read_only_exec == expected.read_only_exec
                && test_filter.is_none()
        } else {
            active.allows_filter(test_filter.as_deref())
        };
        if active.task_id != expected.task_id
            || active.grant_id != expected.grant_id
            || active.epoch != expected.epoch
            || active.mode != expected.mode
            || active.scope != expected.scope
            || active.exact_filter != expected.exact_filter
            || active.read_only_exec != expected.read_only_exec
            || active.expiry != expected.expiry
            || !active_scope_matches
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

fn full_access_host_spawn_gate(
    permission_modes: Arc<Mutex<std::collections::HashMap<Uuid, TaskPermissionSnapshot>>>,
    task_id: Uuid,
    generation: u64,
    cancellation: Arc<AtomicBool>,
) -> SpawnAuthorizationGate {
    Box::new(move |command, spawn_action| {
        let Ok(snapshots) = permission_modes.lock() else {
            return Err(SpawnGateError::Denied);
        };
        let authorized = snapshots.get(&task_id).is_some_and(|snapshot| {
            snapshot.generation == generation
                && snapshot.mode == crate::tools::permission_policy::PermissionMode::FullAccess
                && snapshot.full_access_confirmed
                && snapshot
                    .active_host_cancellation
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &cancellation))
                && !cancellation.load(Ordering::SeqCst)
        });
        if !authorized {
            return Err(SpawnGateError::Denied);
        }
        // Keep the snapshot lock through spawn so a mode change cannot race between the
        // generation check and process creation. A later change signals the runner token.
        spawn_action(command).map_err(SpawnGateError::Spawn)
    })
}

fn full_access_host_start_available(snapshot: &TaskPermissionSnapshot) -> bool {
    snapshot.mode == crate::tools::permission_policy::PermissionMode::FullAccess
        && snapshot.full_access_confirmed
        && !snapshot.host_process_recovery_hold
        && snapshot.active_host_cancellation.is_none()
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

    #[test]
    fn approved_read_only_exec_is_bound_to_one_request_and_consumed_once() {
        let workspace = TempDir::with_prefix("read-only-exec-grant-workspace-");
        let (_runtime, policy) = policy_fixture();
        let mut session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let task_id = session.create_task("direct process grant").id();
        let turn_id = running_turn(&mut session, task_id);
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
        let source_cancellation = {
            let state = grant_state.lock().unwrap();
            state.current.as_ref().unwrap().policy.cancellation()
        };
        session.queue_offline_cargo_results_for_test(
            task_id,
            [
                crate::ProcessResult::empty(crate::ProcessOutcome::Exited(0)),
                crate::ProcessResult::empty(crate::ProcessOutcome::Exited(0)),
            ],
        );

        let approved = crate::tools::process::OfflineReadOnlyExecRequest::new(
            crate::tools::process::SandboxExecutable::Cargo,
            vec!["--version".into()],
            ".",
        )
        .unwrap();
        let widened = crate::tools::process::OfflineReadOnlyExecRequest::new(
            crate::tools::process::SandboxExecutable::Cargo,
            vec!["--help".into()],
            ".",
        )
        .unwrap();
        let source_facts = {
            let state = grant_state.lock().unwrap();
            state
                .current
                .as_ref()
                .unwrap()
                .policy
                .authorization_facts_for_read_only_exec(&approved)
                .unwrap()
        };
        session
            .authorize_offline_read_only_exec_once_from_current_policy(
                task_id,
                approved.clone(),
                Duration::from_secs(60),
            )
            .unwrap();

        {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            assert_eq!(grant.mode, GrantMode::OneShot);
            assert_eq!(grant.scope, GrantScope::ExactIntent);
            assert_eq!(grant.read_only_exec.as_ref(), Some(&approved));
            assert!(grant.approved_digest.is_some());
        }
        let approved_cancellation = {
            let state = grant_state.lock().unwrap();
            let grant = state.current.as_ref().unwrap();
            let approved_facts = grant
                .policy
                .authorization_facts_for_read_only_exec(&approved)
                .unwrap();
            assert_eq!(
                approved_facts.toolchain_identity, source_facts.toolchain_identity,
                "the approved grant retains the pinned toolchain identity"
            );
            assert_eq!(
                approved_facts.registry_identity, source_facts.registry_identity,
                "the approved grant retains the pinned registry identity"
            );
            grant.policy.cancellation()
        };
        assert!(
            source_cancellation.load(Ordering::SeqCst),
            "replacing the source grant cancels work already using its policy"
        );
        assert!(
            !approved_cancellation.load(Ordering::SeqCst),
            "the new exact-intent grant must have independent cancellation state"
        );
        assert!(
            !Arc::ptr_eq(&source_cancellation, &approved_cancellation),
            "each grant epoch owns a distinct cancellation handle"
        );

        let widened_result = session
            .run_offline_read_only_exec(task_id, turn_id, &widened)
            .unwrap();
        assert_eq!(widened_result.outcome, crate::ProcessOutcome::Denied);
        assert_eq!(
            widened_result.profile,
            crate::ProcessExecutionProfile::OfflineReadOnlyExecV1
        );
        assert!(
            !grant_state
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .consumed
        );

        let accepted = session
            .run_offline_read_only_exec(task_id, turn_id, &approved)
            .unwrap();
        assert_eq!(accepted.outcome, crate::ProcessOutcome::Exited(0));
        assert_eq!(
            accepted.profile,
            crate::ProcessExecutionProfile::OfflineReadOnlyExecV1
        );
        assert!(
            grant_state
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .consumed
        );

        let stale_cargo_grant = session
            .run_offline_cargo_tests(task_id, turn_id, None)
            .unwrap();
        assert_eq!(
            stale_cargo_grant.outcome,
            crate::ProcessOutcome::Denied,
            "the exact approval replaces the former standing Cargo grant"
        );

        let replay = session
            .run_offline_read_only_exec(task_id, turn_id, &approved)
            .unwrap();
        assert_eq!(replay.outcome, crate::ProcessOutcome::Denied);
    }

    #[test]
    fn extension_effect_read_lease_blocks_managed_writer_until_effect_returns() {
        let workspace = TempDir::with_prefix("extension-workspace-lease-");
        let mut effect_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let effect_task_id = effect_session.create_task("extension effect").id();
        let effect_turn_id = running_turn(&mut effect_session, effect_task_id);
        let mut writer_session = TaskSession::new_in_workspace(workspace.path()).unwrap();
        let writer_task_id = writer_session.create_task("managed writer").id();
        let writer_turn_id = running_turn(&mut writer_session, writer_task_id);
        let (identity, revision) = effect_session
            .extension_workspace_snapshot(effect_task_id, effect_turn_id)
            .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();

        let effect_result = effect_session
            .with_extension_workspace_snapshot(
                effect_task_id,
                effect_turn_id,
                identity,
                revision,
                || {
                    let writer = thread::spawn(move || {
                        started_tx.send(()).unwrap();
                        let result = writer_session.create_workspace_file(
                            writer_task_id,
                            writer_turn_id,
                            Path::new("writer.txt"),
                            "committed",
                        );
                        done_tx.send(result).unwrap();
                    });
                    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                    assert!(matches!(
                        done_rx.recv_timeout(Duration::from_millis(50)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ));
                    writer
                },
            )
            .unwrap()
            .expect("captured revision remains current");

        // The effect closure's return drops the read lease, allowing the queued writer to finish.
        effect_result.join().unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let (_, final_revision) = effect_session
            .extension_workspace_snapshot(effect_task_id, effect_turn_id)
            .unwrap();
        assert_eq!(final_revision, revision + 1);
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
            read_only_exec: None,
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

#[cfg(test)]
mod full_access_host_authorization_tests {
    use super::{
        full_access_host_spawn_gate, full_access_host_start_available, TaskPermissionSnapshot,
        TaskState, TurnState,
    };
    use crate::tools::permission_policy::PermissionMode;
    use krypper_sandbox::{SpawnAction, SpawnGateError};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[cfg(target_os = "linux")]
    #[test]
    fn session_dispatch_runs_direct_argv_in_an_unfenced_host_directory() {
        let workspace = tempfile::tempdir().expect("workspace fixture");
        let host_directory = tempfile::tempdir().expect("outside host directory");
        let mut session =
            super::TaskSession::new_in_workspace(workspace.path()).expect("workspace session");
        let task_id = session.create_task("host process outside workspace").id();
        let turn_id = {
            let task = session.get_task_mut(task_id).expect("task");
            task.transition(TaskState::Running).expect("running task");
            let turn_id = task.create_turn().expect("turn");
            task.transition_turn(turn_id, TurnState::Running)
                .expect("running turn");
            turn_id
        };
        session
            .set_task_permission_mode(task_id, PermissionMode::FullAccess, true)
            .expect("confirmed Full Access mode");
        let request = crate::tools::process::HostProcessRequest::new(
            "/bin/pwd".into(),
            Vec::new(),
            host_directory.path().to_string_lossy().into_owned(),
        )
        .expect("direct host request");

        let result = session
            .run_full_access_host_process(task_id, turn_id, &request)
            .expect("host process dispatch");

        assert_eq!(result.outcome, crate::ProcessOutcome::Exited(0));
        assert_eq!(
            result.profile,
            crate::ProcessExecutionProfile::FullAccessHostProcessV1
        );
        assert_eq!(
            result.stdout.trim(),
            host_directory.path().to_string_lossy()
        );
        assert!(result.cleanup_confirmed);
        assert!(result.sandbox_evidence.is_none());
    }

    #[test]
    fn start_gate_rejects_overlap_even_after_cancellation_until_runner_cleanup_returns() {
        let mut snapshot = TaskPermissionSnapshot {
            mode: PermissionMode::FullAccess,
            full_access_confirmed: true,
            generation: 1,
            task_cancellation: Arc::new(AtomicBool::new(false)),
            active_host_cancellation: None,
            host_process_recovery_hold: false,
        };
        assert!(full_access_host_start_available(&snapshot));

        let cancellation = Arc::new(AtomicBool::new(false));
        snapshot.active_host_cancellation = Some(Arc::clone(&cancellation));
        assert!(!full_access_host_start_available(&snapshot));
        cancellation.store(true, Ordering::SeqCst);
        assert!(!full_access_host_start_available(&snapshot));

        snapshot.active_host_cancellation = None;
        snapshot.host_process_recovery_hold = true;
        assert!(!full_access_host_start_available(&snapshot));
    }

    #[test]
    fn changing_task_mode_cancels_and_invalidates_the_active_host_authorization() {
        let mut session = super::TaskSession::new();
        let task_id = session.create_task("mode change").id();
        session
            .set_task_permission_mode(task_id, PermissionMode::FullAccess, true)
            .unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let initial_generation = {
            let mut snapshots = session.permission_modes.lock().unwrap();
            let snapshot = snapshots.get_mut(&task_id).unwrap();
            snapshot.active_host_cancellation = Some(Arc::clone(&cancellation));
            snapshot.generation
        };

        session
            .set_task_permission_mode(task_id, PermissionMode::Ask, false)
            .unwrap();

        let snapshots = session.permission_modes.lock().unwrap();
        let snapshot = snapshots.get(&task_id).unwrap();
        assert!(cancellation.load(Ordering::SeqCst));
        assert_eq!(snapshot.mode, PermissionMode::Ask);
        assert!(!snapshot.full_access_confirmed);
        assert_eq!(snapshot.generation, initial_generation + 1);
        assert!(snapshot
            .active_host_cancellation
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &cancellation)));
    }

    #[test]
    fn spawn_gate_rechecks_the_task_mode_generation_and_confirmation_before_spawn() {
        let task_id = Uuid::new_v4();
        let cancellation = Arc::new(AtomicBool::new(false));
        let permission_modes = Arc::new(Mutex::new(std::collections::HashMap::from([(
            task_id,
            TaskPermissionSnapshot {
                mode: PermissionMode::FullAccess,
                full_access_confirmed: true,
                generation: 4,
                task_cancellation: Arc::clone(&cancellation),
                active_host_cancellation: Some(Arc::clone(&cancellation)),
                host_process_recovery_hold: false,
            },
        )])));
        let gate = full_access_host_spawn_gate(
            Arc::clone(&permission_modes),
            task_id,
            4,
            Arc::clone(&cancellation),
        );

        // The task changes mode after the runner is created but before the spawn boundary.
        {
            let mut snapshots = permission_modes.lock().unwrap();
            let snapshot = snapshots.get_mut(&task_id).unwrap();
            snapshot.mode = PermissionMode::Ask;
            snapshot.full_access_confirmed = false;
            snapshot.generation += 1;
            cancellation.store(true, Ordering::SeqCst);
        }

        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&spawn_calls);
        let spawn: SpawnAction = Box::new(move |_command| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("spawn must remain unreachable"))
        });
        let result = gate(Command::new("must-not-run"), spawn);

        assert!(matches!(result, Err(SpawnGateError::Denied)));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn spawn_gate_rejects_a_different_task_id() {
        let task_id = Uuid::new_v4();
        let other_task_id = Uuid::new_v4();
        let cancellation = Arc::new(AtomicBool::new(false));
        let permission_modes = Arc::new(Mutex::new(std::collections::HashMap::from([(
            task_id,
            TaskPermissionSnapshot {
                mode: PermissionMode::FullAccess,
                full_access_confirmed: true,
                generation: 1,
                task_cancellation: Arc::clone(&cancellation),
                active_host_cancellation: Some(Arc::clone(&cancellation)),
                host_process_recovery_hold: false,
            },
        )])));
        let gate = full_access_host_spawn_gate(permission_modes, other_task_id, 1, cancellation);
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&spawn_calls);
        let spawn: SpawnAction = Box::new(move |_command| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("spawn must remain unreachable"))
        });

        assert!(matches!(
            gate(Command::new("must-not-run"), spawn),
            Err(SpawnGateError::Denied)
        ));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    }
}
