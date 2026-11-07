use krypper::task_lifecycle::{
    Task, TaskHistoryTransition, TaskSession, TaskState, TurnState, TurnTransitionError,
};

#[test]
fn new_task_keeps_description_and_starts_pending_with_empty_read_only_history() {
    let task = Task::new("Review the release notes");
    assert_eq!(task.description(), "Review the release notes");
    assert_eq!(task.state(), TaskState::Pending);
    assert!(task.events().is_empty());
}

#[test]
fn turn_creation_is_allowed_for_pending_and_running_tasks_in_creation_order() {
    for task_state in [TaskState::Pending, TaskState::Running] {
        let mut task = Task::new("A task");
        if task_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        let task_id = task.id();

        let first_turn_id = task.create_turn().unwrap();
        let second_turn_id = task.create_turn().unwrap();

        assert_eq!(
            task.turns()
                .iter()
                .map(|turn| turn.id())
                .collect::<Vec<_>>(),
            vec![first_turn_id, second_turn_id]
        );
        assert_eq!(
            task.history().len(),
            if task_state == TaskState::Running {
                3
            } else {
                2
            }
        );
        let first_creation_sequence = if task_state == TaskState::Running {
            2
        } else {
            1
        };
        assert_eq!(
            task.history()
                .iter()
                .filter_map(|event| match event.transition() {
                    TaskHistoryTransition::TurnCreated {
                        turn_id,
                        initial_state,
                    } => Some((event.task_id(), event.sequence(), turn_id, initial_state,)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    task_id,
                    first_creation_sequence,
                    first_turn_id,
                    TurnState::Pending,
                ),
                (
                    task_id,
                    first_creation_sequence + 1,
                    second_turn_id,
                    TurnState::Pending,
                ),
            ]
        );
        for turn in task.turns() {
            assert_eq!(turn.task_id(), task_id);
            assert_eq!(turn.state(), TurnState::Pending);
            assert!(turn.events().is_empty());
        }
    }
}

#[test]
fn turn_creation_rejects_terminal_tasks_without_mutating_state_or_history() {
    for terminal_state in [
        TaskState::Succeeded,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();
        let completed_turn_id = task.create_turn().unwrap();
        let cancelled_turn_id = task.create_turn().unwrap();
        task.transition_turn(completed_turn_id, TurnState::Running)
            .unwrap();
        task.transition_turn(completed_turn_id, TurnState::Succeeded)
            .unwrap();
        task.transition_turn(cancelled_turn_id, TurnState::Cancelled)
            .unwrap();
        task.transition(terminal_state).unwrap();

        let task_state_before = task.state();
        let task_events_before = task.events().to_vec();
        let history_before = task.history().to_vec();
        let turns_before = task
            .turns()
            .iter()
            .map(|turn| {
                (
                    turn.id(),
                    turn.task_id(),
                    turn.state(),
                    turn.events().to_vec(),
                )
            })
            .collect::<Vec<_>>();

        let error = task.create_turn().unwrap_err();

        assert_eq!(error.task_state(), terminal_state);
        assert_eq!(
            error.to_string(),
            format!("cannot create a turn for task in terminal state '{terminal_state}'")
        );
        assert_eq!(task.state(), task_state_before);
        assert_eq!(task.events(), task_events_before);
        assert_eq!(task.history(), history_before);
        assert_eq!(task.turns().len(), turns_before.len());
        assert_eq!(
            task.turns()
                .iter()
                .map(|turn| {
                    (
                        turn.id(),
                        turn.task_id(),
                        turn.state(),
                        turn.events().to_vec(),
                    )
                })
                .collect::<Vec<_>>(),
            turns_before
        );
    }
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
fn combined_history_records_interleaved_task_and_turn_transitions() {
    let mut task = Task::new("A task");
    let task_id = task.id();
    task.transition(TaskState::Running).unwrap();
    let first_turn_id = task.create_turn().unwrap();
    task.transition_turn(first_turn_id, TurnState::Running)
        .unwrap();

    let second_turn_id = task.create_turn().unwrap();
    task.transition_turn(second_turn_id, TurnState::Cancelled)
        .unwrap();
    task.transition_turn(first_turn_id, TurnState::Succeeded)
        .unwrap();
    task.transition(TaskState::Succeeded).unwrap();

    assert_eq!(task.history().len(), 7);

    assert_eq!(
        task.history()
            .iter()
            .map(|event| (event.task_id(), event.sequence(), event.transition()))
            .collect::<Vec<_>>(),
        vec![
            (
                task_id,
                1,
                TaskHistoryTransition::Task {
                    previous_state: TaskState::Pending,
                    new_state: TaskState::Running,
                },
            ),
            (
                task_id,
                2,
                TaskHistoryTransition::TurnCreated {
                    turn_id: first_turn_id,
                    initial_state: TurnState::Pending,
                },
            ),
            (
                task_id,
                3,
                TaskHistoryTransition::Turn {
                    turn_id: first_turn_id,
                    previous_state: TurnState::Pending,
                    new_state: TurnState::Running,
                },
            ),
            (
                task_id,
                4,
                TaskHistoryTransition::TurnCreated {
                    turn_id: second_turn_id,
                    initial_state: TurnState::Pending,
                },
            ),
            (
                task_id,
                5,
                TaskHistoryTransition::Turn {
                    turn_id: second_turn_id,
                    previous_state: TurnState::Pending,
                    new_state: TurnState::Cancelled,
                },
            ),
            (
                task_id,
                6,
                TaskHistoryTransition::Turn {
                    turn_id: first_turn_id,
                    previous_state: TurnState::Running,
                    new_state: TurnState::Succeeded,
                },
            ),
            (
                task_id,
                7,
                TaskHistoryTransition::Task {
                    previous_state: TaskState::Running,
                    new_state: TaskState::Succeeded,
                },
            ),
        ]
    );

    assert_eq!(task.events()[0].sequence(), 1);
    assert_eq!(task.events()[1].sequence(), 2);
    assert_eq!(
        task.get_turn(first_turn_id).unwrap().events()[1].sequence(),
        2
    );
    assert_eq!(
        task.get_turn(second_turn_id).unwrap().events()[0].sequence(),
        1
    );

    let mut copied_history = task.history().to_vec();
    copied_history.clear();
    assert_eq!(task.history().len(), 7);
}

#[test]
fn invalid_task_and_turn_transitions_leave_every_history_and_state_unchanged() {
    let mut task = Task::new("A task");
    task.transition(TaskState::Running).unwrap();
    let active_turn_id = task.create_turn().unwrap();
    task.transition_turn(active_turn_id, TurnState::Running)
        .unwrap();

    let task_state = task.state();
    let task_events = task.events().to_vec();
    let history = task.history().to_vec();
    let turns = task
        .turns()
        .iter()
        .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
        .collect::<Vec<_>>();

    assert!(task.transition(TaskState::Succeeded).is_err());
    assert_eq!(task.state(), task_state);
    assert_eq!(task.events(), task_events);
    assert_eq!(task.history(), history);
    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
            .collect::<Vec<_>>(),
        turns
    );

    assert!(task
        .transition_turn(active_turn_id, TurnState::Pending)
        .is_err());
    assert_eq!(task.state(), task_state);
    assert_eq!(task.events(), task_events);
    assert_eq!(task.history(), history);
    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| (turn.id(), turn.state(), turn.events().to_vec()))
            .collect::<Vec<_>>(),
        turns
    );
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
fn terminal_task_transitions_reject_active_turns_without_changing_histories() {
    let cases = [
        (TaskState::Pending, TurnState::Pending, TaskState::Cancelled),
        (TaskState::Running, TurnState::Pending, TaskState::Succeeded),
        (TaskState::Running, TurnState::Pending, TaskState::Failed),
        (TaskState::Running, TurnState::Pending, TaskState::Cancelled),
        (TaskState::Running, TurnState::Running, TaskState::Succeeded),
        (TaskState::Running, TurnState::Running, TaskState::Failed),
        (TaskState::Running, TurnState::Running, TaskState::Cancelled),
    ];

    for (task_state, active_turn_state, next) in cases {
        let mut task = Task::new("A task");
        let active_turn_id = task.create_turn().unwrap();
        let completed_turn_id = task.create_turn().unwrap();

        if task_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }
        if active_turn_state == TurnState::Running {
            task.transition_turn(active_turn_id, TurnState::Running)
                .unwrap();
        }
        task.transition_turn(completed_turn_id, TurnState::Running)
            .unwrap();
        task.transition_turn(completed_turn_id, TurnState::Succeeded)
            .unwrap();

        let previous_task_state = task.state();
        let previous_task_events = task.events().to_vec();
        let previous_turns: Vec<_> = task
            .turns()
            .iter()
            .map(|turn| (turn.state(), turn.events().to_vec()))
            .collect();

        let error = match task.transition(next) {
            Err(error) => error,
            Ok(()) => panic!("task transitioned to {next} while a turn was active"),
        };

        assert_eq!(error.from(), task_state);
        assert_eq!(error.to(), next);
        assert_eq!(
            error.blocking_turn(),
            Some((active_turn_id, active_turn_state))
        );
        let message = error.to_string();
        assert!(message.contains(active_turn_state.as_str()), "{message}");
        assert!(message.contains(&active_turn_id.to_string()), "{message}");
        assert_eq!(task.state(), previous_task_state);
        assert_eq!(task.events(), previous_task_events);
        let current_turns: Vec<_> = task
            .turns()
            .iter()
            .map(|turn| (turn.state(), turn.events().to_vec()))
            .collect();
        assert_eq!(current_turns, previous_turns);
    }
}

#[test]
fn terminal_task_transitions_succeed_after_all_turns_finish() {
    for next in [
        TaskState::Succeeded,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        let mut task = Task::new("A task");
        task.transition(TaskState::Running).unwrap();

        let succeeded_turn = task.create_turn().unwrap();
        task.transition_turn(succeeded_turn, TurnState::Running)
            .unwrap();
        task.transition_turn(succeeded_turn, TurnState::Succeeded)
            .unwrap();

        let failed_turn = task.create_turn().unwrap();
        task.transition_turn(failed_turn, TurnState::Running)
            .unwrap();
        task.transition_turn(failed_turn, TurnState::Failed)
            .unwrap();

        let cancelled_turn = task.create_turn().unwrap();
        task.transition_turn(cancelled_turn, TurnState::Cancelled)
            .unwrap();

        task.transition(next).unwrap();
        assert_eq!(task.state(), next);
        assert_eq!(task.events().last().unwrap().new_state(), next);
    }
}

#[test]
fn separate_tasks_keep_events_isolated() {
    let mut first = Task::new("First task");
    let mut second = Task::new("Second task");
    first.transition(TaskState::Running).unwrap();
    first.create_turn().unwrap();
    second.create_turn().unwrap();
    assert_eq!(first.events().len(), 1);
    assert!(second.events().is_empty());
    assert_eq!(first.events()[0].task_id(), first.id());
    assert_ne!(first.events()[0].task_id(), second.id());
    assert_eq!(
        first.history()[0].transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Pending,
            new_state: TaskState::Running,
        }
    );
    assert_eq!(first.history()[0].task_id(), first.id());
    assert_eq!(first.history().len(), 2);
    assert_eq!(first.history()[1].task_id(), first.id());
    assert_eq!(
        first.history()[1].transition(),
        TaskHistoryTransition::TurnCreated {
            turn_id: first.turns()[0].id(),
            initial_state: TurnState::Pending,
        }
    );
    assert_eq!(second.history().len(), 1);
    assert_eq!(second.history()[0].task_id(), second.id());
    assert_eq!(
        second.history()[0].transition(),
        TaskHistoryTransition::TurnCreated {
            turn_id: second.turns()[0].id(),
            initial_state: TurnState::Pending,
        }
    );
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

#[test]
fn tasks_create_stable_pending_turns_in_creation_order() {
    let mut first_task = Task::new("First task");
    let mut second_task = Task::new("Second task");
    let first_task_id = first_task.id();
    let second_task_id = second_task.id();

    let first_turn_id = first_task.create_turn().unwrap();
    let second_turn_id = first_task.create_turn().unwrap();
    let other_turn_id = second_task.create_turn().unwrap();

    assert_ne!(first_task_id, second_task_id);
    assert_ne!(first_turn_id, second_turn_id);
    assert_ne!(first_turn_id, other_turn_id);

    let turns = first_task.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].id(), first_turn_id);
    assert_eq!(turns[1].id(), second_turn_id);
    assert_eq!(turns[0].task_id(), first_task_id);
    assert_eq!(turns[1].task_id(), first_task_id);
    assert_eq!(turns[0].state(), TurnState::Pending);
    assert_eq!(turns[1].state(), TurnState::Pending);
    assert!(turns[0].events().is_empty());
    assert!(turns[1].events().is_empty());
    assert_eq!(
        first_task.get_turn(first_turn_id).unwrap().id(),
        first_turn_id
    );
    assert!(first_task.get_turn(other_turn_id).is_none());
    assert_eq!(
        first_task.transition_turn(other_turn_id, TurnState::Running),
        Err(TurnTransitionError::UnknownTurn {
            turn_id: other_turn_id
        })
    );

    first_task
        .transition_turn(first_turn_id, TurnState::Running)
        .unwrap();
    assert_eq!(first_task.turns()[0].id(), first_turn_id);
    assert_eq!(first_task.turns()[0].state(), TurnState::Running);
}

#[test]
fn valid_turn_transitions_record_ordered_immutable_events() {
    let cases = [
        (vec![], TurnState::Running),
        (vec![], TurnState::Cancelled),
        (vec![TurnState::Running], TurnState::Succeeded),
        (vec![TurnState::Running], TurnState::Failed),
        (vec![TurnState::Running], TurnState::Cancelled),
    ];

    for (prefix, next) in cases {
        let mut task = Task::new("A task");
        let task_id = task.id();
        let turn_id = task.create_turn().unwrap();
        let mut expected_states = prefix;
        expected_states.push(next);

        for state in &expected_states {
            task.transition_turn(turn_id, *state).unwrap();
        }

        let turn = task.get_turn(turn_id).unwrap();
        let events = turn.events();
        assert_eq!(events.len(), expected_states.len());
        let mut previous_state = TurnState::Pending;
        for (index, (event, new_state)) in events
            .iter()
            .zip(expected_states.iter().copied())
            .enumerate()
        {
            assert_eq!(event.task_id(), task_id);
            assert_eq!(event.turn_id(), turn_id);
            assert_eq!(event.sequence(), index as u64 + 1);
            assert_eq!(event.previous_state(), previous_state);
            assert_eq!(event.new_state(), new_state);
            previous_state = new_state;
        }
        assert_eq!(turn.state(), next);

        let mut copied_history = turn.events().to_vec();
        copied_history.clear();
        assert_eq!(turn.events().len(), expected_states.len());
    }
}

#[test]
fn invalid_turn_transitions_leave_state_and_history_unchanged() {
    let cases: &[(&[TurnState], TurnState)] = &[
        (&[], TurnState::Pending),
        (&[], TurnState::Succeeded),
        (&[], TurnState::Failed),
        (&[TurnState::Running], TurnState::Pending),
        (&[TurnState::Running], TurnState::Running),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Pending,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Running,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Succeeded,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Failed,
        ),
        (
            &[TurnState::Running, TurnState::Succeeded],
            TurnState::Cancelled,
        ),
        (&[TurnState::Running, TurnState::Failed], TurnState::Pending),
        (&[TurnState::Running, TurnState::Failed], TurnState::Running),
        (
            &[TurnState::Running, TurnState::Failed],
            TurnState::Succeeded,
        ),
        (&[TurnState::Running, TurnState::Failed], TurnState::Failed),
        (
            &[TurnState::Running, TurnState::Failed],
            TurnState::Cancelled,
        ),
        (&[TurnState::Cancelled], TurnState::Pending),
        (&[TurnState::Cancelled], TurnState::Running),
        (&[TurnState::Cancelled], TurnState::Succeeded),
        (&[TurnState::Cancelled], TurnState::Failed),
        (&[TurnState::Cancelled], TurnState::Cancelled),
    ];

    for (prefix, invalid) in cases {
        let mut task = Task::new("A task");
        let turn_id = task.create_turn().unwrap();
        for state in *prefix {
            task.transition_turn(turn_id, *state).unwrap();
        }
        let turn = task.get_turn(turn_id).unwrap();
        let previous_state = turn.state();
        let previous_events = turn.events().to_vec();

        let error = task.transition_turn(turn_id, *invalid).unwrap_err();
        assert_eq!(
            error,
            TurnTransitionError::InvalidTransition {
                from: previous_state,
                to: *invalid,
            }
        );
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), previous_state);
        assert_eq!(turn.events(), previous_events);
    }
}

#[test]
fn turns_keep_state_and_history_isolated_between_tasks() {
    let mut first_task = Task::new("First task");
    let mut second_task = Task::new("Second task");
    let first_task_id = first_task.id();
    let second_task_id = second_task.id();
    let first_turn_id = first_task.create_turn().unwrap();
    first_task.create_turn().unwrap();
    let other_turn_id = second_task.create_turn().unwrap();

    first_task
        .transition_turn(first_turn_id, TurnState::Running)
        .unwrap();

    assert_eq!(first_task.turns()[0].events()[0].task_id(), first_task_id);
    assert_eq!(first_task.turns()[0].events()[0].turn_id(), first_turn_id);
    assert!(first_task.turns()[1].events().is_empty());
    assert!(second_task.turns()[0].events().is_empty());
    assert!(first_task.get_turn(other_turn_id).is_none());
    assert!(second_task.get_turn(first_turn_id).is_none());
    assert_eq!(second_task.turns()[0].task_id(), second_task_id);
    assert_eq!(second_task.turns()[0].id(), other_turn_id);
}

#[test]
fn task_cancel_cancels_active_turns_before_task_and_preserves_terminal_turns() {
    let mut task = Task::new("A task");
    task.transition(TaskState::Running).unwrap();

    let pending_turn_id = task.create_turn().unwrap();
    let running_turn_id = task.create_turn().unwrap();
    let succeeded_turn_id = task.create_turn().unwrap();
    let failed_turn_id = task.create_turn().unwrap();
    let cancelled_turn_id = task.create_turn().unwrap();

    task.transition_turn(running_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(succeeded_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(succeeded_turn_id, TurnState::Succeeded)
        .unwrap();
    task.transition_turn(failed_turn_id, TurnState::Running)
        .unwrap();
    task.transition_turn(failed_turn_id, TurnState::Failed)
        .unwrap();
    task.transition_turn(cancelled_turn_id, TurnState::Cancelled)
        .unwrap();

    let task_events_before = task.events().to_vec();
    let succeeded_events_before = task.get_turn(succeeded_turn_id).unwrap().events().to_vec();
    let failed_events_before = task.get_turn(failed_turn_id).unwrap().events().to_vec();
    let cancelled_events_before = task.get_turn(cancelled_turn_id).unwrap().events().to_vec();

    task.cancel().unwrap();

    assert_eq!(task.state(), TaskState::Cancelled);
    assert_eq!(task.history().len(), 15);
    assert_eq!(
        task.history()[12].transition(),
        TaskHistoryTransition::Turn {
            turn_id: pending_turn_id,
            previous_state: TurnState::Pending,
            new_state: TurnState::Cancelled,
        }
    );
    assert_eq!(
        task.history()[13].transition(),
        TaskHistoryTransition::Turn {
            turn_id: running_turn_id,
            previous_state: TurnState::Running,
            new_state: TurnState::Cancelled,
        }
    );
    assert_eq!(
        task.history()[14].transition(),
        TaskHistoryTransition::Task {
            previous_state: TaskState::Running,
            new_state: TaskState::Cancelled,
        }
    );
    assert_eq!(task.history()[12].sequence(), 13);
    assert_eq!(task.history()[13].sequence(), 14);
    assert_eq!(task.history()[14].sequence(), 15);
    assert_eq!(task.events().len(), task_events_before.len() + 1);
    let task_event = task.events().last().unwrap();
    assert_eq!(task_event.task_id(), task.id());
    assert_eq!(task_event.sequence(), task_events_before.len() as u64 + 1);
    assert_eq!(task_event.previous_state(), TaskState::Running);
    assert_eq!(task_event.new_state(), TaskState::Cancelled);

    assert_eq!(
        task.turns()
            .iter()
            .map(|turn| turn.id())
            .collect::<Vec<_>>(),
        vec![
            pending_turn_id,
            running_turn_id,
            succeeded_turn_id,
            failed_turn_id,
            cancelled_turn_id,
        ]
    );
    for (turn_id, previous_state, previous_event_count) in [
        (pending_turn_id, TurnState::Pending, 0),
        (running_turn_id, TurnState::Running, 1),
    ] {
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), TurnState::Cancelled);
        let event = turn.events().last().unwrap();
        assert_eq!(event.task_id(), task.id());
        assert_eq!(event.turn_id(), turn_id);
        assert_eq!(event.sequence(), previous_event_count + 1);
        assert_eq!(event.previous_state(), previous_state);
        assert_eq!(event.new_state(), TurnState::Cancelled);
    }

    assert_eq!(
        task.get_turn(succeeded_turn_id).unwrap().state(),
        TurnState::Succeeded
    );
    assert_eq!(
        task.get_turn(succeeded_turn_id).unwrap().events(),
        succeeded_events_before
    );
    assert_eq!(
        task.get_turn(failed_turn_id).unwrap().state(),
        TurnState::Failed
    );
    assert_eq!(
        task.get_turn(failed_turn_id).unwrap().events(),
        failed_events_before
    );
    assert_eq!(
        task.get_turn(cancelled_turn_id).unwrap().state(),
        TurnState::Cancelled
    );
    assert_eq!(
        task.get_turn(cancelled_turn_id).unwrap().events(),
        cancelled_events_before
    );
}

#[test]
fn task_cancel_cancels_pending_turns_of_a_pending_task() {
    let mut task = Task::new("A pending task");
    let first_turn_id = task.create_turn().unwrap();
    let second_turn_id = task.create_turn().unwrap();
    let task_id = task.id();

    task.cancel().unwrap();

    assert_eq!(task.state(), TaskState::Cancelled);
    assert_eq!(task.events().len(), 1);
    assert_eq!(task.events()[0].task_id(), task_id);
    assert_eq!(task.events()[0].sequence(), 1);
    assert_eq!(task.events()[0].previous_state(), TaskState::Pending);
    assert_eq!(task.events()[0].new_state(), TaskState::Cancelled);
    assert_eq!(task.turns()[0].id(), first_turn_id);
    assert_eq!(task.turns()[1].id(), second_turn_id);
    for (sequence, turn_id) in [(1, first_turn_id), (1, second_turn_id)] {
        let turn = task.get_turn(turn_id).unwrap();
        assert_eq!(turn.state(), TurnState::Cancelled);
        assert_eq!(turn.events().len(), 1);
        assert_eq!(turn.events()[0].task_id(), task_id);
        assert_eq!(turn.events()[0].turn_id(), turn_id);
        assert_eq!(turn.events()[0].sequence(), sequence);
        assert_eq!(turn.events()[0].previous_state(), TurnState::Pending);
        assert_eq!(turn.events()[0].new_state(), TurnState::Cancelled);
    }
}

#[test]
fn task_cancel_succeeds_for_tasks_without_turns() {
    for initial_state in [TaskState::Pending, TaskState::Running] {
        let mut task = Task::new("A task without turns");
        if initial_state == TaskState::Running {
            task.transition(TaskState::Running).unwrap();
        }

        task.cancel().unwrap();

        assert_eq!(task.state(), TaskState::Cancelled);
        assert_eq!(
            task.events().last().unwrap().previous_state(),
            initial_state
        );
        assert_eq!(
            task.events().last().unwrap().new_state(),
            TaskState::Cancelled
        );
        assert!(task.turns().is_empty());
    }
}

#[test]
fn task_cancel_rejects_terminal_tasks_without_mutating_task_or_turn_history() {
    for (terminal_state, turn_terminal_state) in [
        (TaskState::Succeeded, TurnState::Succeeded),
        (TaskState::Failed, TurnState::Failed),
        (TaskState::Cancelled, TurnState::Cancelled),
    ] {
        let mut task = Task::new("A terminal task");
        let active_turn_id = task.create_turn().unwrap();
        if terminal_state == TaskState::Cancelled {
            task.cancel().unwrap();
        } else {
            task.transition(TaskState::Running).unwrap();
            task.transition_turn(active_turn_id, TurnState::Running)
                .unwrap();
            task.transition_turn(active_turn_id, turn_terminal_state)
                .unwrap();
            task.transition(terminal_state).unwrap();
        }

        let task_state_before = task.state();
        let task_events_before = task.events().to_vec();
        let turn_state_before = task.get_turn(active_turn_id).unwrap().state();
        let turn_events_before = task.get_turn(active_turn_id).unwrap().events().to_vec();

        let error = task.cancel().unwrap_err();

        assert_eq!(error.from(), terminal_state);
        assert_eq!(error.to(), TaskState::Cancelled);
        assert_eq!(error.blocking_turn(), None);
        assert!(error.to_string().contains("cannot transition task"));
        assert_eq!(task.state(), task_state_before);
        assert_eq!(task.events(), task_events_before);
        let turn = task.get_turn(active_turn_id).unwrap();
        assert_eq!(turn.state(), turn_state_before);
        assert_eq!(turn.events(), turn_events_before);
    }
}
