use krypper::task_lifecycle::{Task, TaskSession, TaskState};

#[test]
fn new_task_keeps_description_and_starts_pending_with_empty_read_only_history() {
    let task = Task::new("Review the release notes");
    assert_eq!(task.description(), "Review the release notes");
    assert_eq!(task.state(), TaskState::Pending);
    assert!(task.events().is_empty());
}

#[test]
fn task_ids_are_unique_and_stable() {
    let mut first = Task::new("First task");
    let second = Task::new("Second task");
    let id = first.id();
    first.transition(TaskState::Running).unwrap();
    first.transition(TaskState::Succeeded).unwrap();
    assert_ne!(id, second.id());
    assert_eq!(first.id(), id);
}

#[test]
fn valid_transitions_record_ordered_task_bound_events() {
    let cases = [
        (TaskState::Pending, TaskState::Running),
        (TaskState::Pending, TaskState::Cancelled),
        (TaskState::Running, TaskState::Succeeded),
        (TaskState::Running, TaskState::Failed),
        (TaskState::Running, TaskState::Cancelled),
    ];
    for (initial, next) in cases {
        let mut task = Task::new("A task");
        if initial == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        let previous = task.state();
        task.transition(next).unwrap();
        let event = task.events().last().unwrap();
        assert_eq!(task.state(), next);
        assert_eq!(
            task.events().len(),
            if initial == TaskState::Running { 2 } else { 1 }
        );
        assert_eq!(event.task_id(), task.id());
        assert_eq!(event.previous_state(), previous);
        assert_eq!(event.new_state(), next);
        assert_eq!(event.sequence(), task.events().len() as u64);
    }
}

#[test]
fn invalid_transitions_leave_state_and_history_unchanged() {
    let cases = [
        (&[][..], TaskState::Succeeded),
        (&[][..], TaskState::Failed),
        (&[TaskState::Running][..], TaskState::Pending),
        (
            &[TaskState::Running, TaskState::Succeeded][..],
            TaskState::Failed,
        ),
        (
            &[TaskState::Running, TaskState::Failed][..],
            TaskState::Cancelled,
        ),
        (&[TaskState::Cancelled][..], TaskState::Running),
    ];

    for (prefix, invalid) in cases {
        let mut task = Task::new("A task");
        for state in prefix {
            task.transition(*state).unwrap();
        }
        let previous_state = task.state();
        let previous_events = task.events().to_vec();

        assert!(task.transition(invalid).is_err());
        assert_eq!(task.state(), previous_state);
        assert_eq!(task.events(), previous_events);
    }
}

#[test]
fn separate_tasks_keep_events_isolated() {
    let mut first = Task::new("First task");
    let second = Task::new("Second task");
    first.transition(TaskState::Running).unwrap();
    assert_eq!(first.events().len(), 1);
    assert!(second.events().is_empty());
    assert_eq!(first.events()[0].task_id(), first.id());
    assert_ne!(first.events()[0].task_id(), second.id());
}

#[test]
fn sessions_have_unique_stable_ids_and_start_empty() {
    let first = TaskSession::new();
    let second = TaskSession::new();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.list_tasks().len(), 0);
}

#[test]
fn sessions_create_find_and_list_tasks_in_creation_order() {
    let mut session = TaskSession::new();
    let first_id = session.create_task("First task").id();
    let second_id = session.create_task("Second task").id();
    let first = session.get_task(first_id).unwrap();
    assert_eq!(first.description(), "First task");
    assert_eq!(first.state(), TaskState::Pending);
    assert_eq!(session.list_tasks()[0].id(), first_id);
    assert_eq!(session.list_tasks()[1].id(), second_id);
    assert!(session.get_task(uuid::Uuid::nil()).is_none());
}

#[test]
fn task_lookup_and_events_are_isolated_between_sessions() {
    let mut first_session = TaskSession::new();
    let mut second_session = TaskSession::new();
    let first_id = first_session.create_task("A task").id();
    let second_id = second_session.create_task("A task").id();
    first_session
        .get_task_mut(first_id)
        .unwrap()
        .transition(TaskState::Running)
        .unwrap();

    assert_ne!(first_id, second_id);
    assert!(first_session.get_task(second_id).is_none());
    assert!(second_session.get_task(first_id).is_none());
    assert_eq!(first_session.get_task(first_id).unwrap().events().len(), 1);
    assert!(second_session
        .get_task(second_id)
        .unwrap()
        .events()
        .is_empty());
}
