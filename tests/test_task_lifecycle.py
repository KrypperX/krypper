import unittest

from task_lifecycle import Task, TaskSession


class TaskLifecycleTests(unittest.TestCase):
    def test_new_task_keeps_description_and_starts_pending(self):
        task = Task("Review the release notes")

        self.assertEqual(task.description, "Review the release notes")
        self.assertEqual(task.state, "pending")
        self.assertEqual(task.events, [])

    def test_tasks_have_unique_stable_identifiers(self):
        first = Task("First task")
        second = Task("Second task")
        first_id = first.id

        first.transition("running")
        first.transition("succeeded")

        self.assertTrue(first_id)
        self.assertNotEqual(first.id, second.id)
        self.assertEqual(first.id, first_id)

    def test_each_valid_transition_updates_state_and_records_event(self):
        transitions = (
            ((), "running"),
            ((), "cancelled"),
            (("running",), "succeeded"),
            (("running",), "failed"),
            (("running",), "cancelled"),
        )

        for prior_states, new_state in transitions:
            with self.subTest(prior_states=prior_states, new_state=new_state):
                task = Task("A task")
                for state in prior_states:
                    task.transition(state)

                previous_state = task.state
                task.transition(new_state)

                self.assertEqual(task.state, new_state)
                self.assertEqual(len(task.events), len(prior_states) + 1)
                event = task.events[-1]
                self.assertEqual(event.task_id, task.id)
                self.assertEqual(event.previous_state, previous_state)
                self.assertEqual(event.new_state, new_state)

    def test_events_preserve_transition_order(self):
        task = Task("A task")

        task.transition("running")
        task.transition("succeeded")

        self.assertEqual(
            [
                (event.sequence, event.previous_state, event.new_state)
                for event in task.events
            ],
            [(1, "pending", "running"), (2, "running", "succeeded")],
        )
        self.assertTrue(all(event.task_id == task.id for event in task.events))

    def test_invalid_transitions_leave_state_and_events_unchanged(self):
        invalid_transitions = (
            ((), "succeeded"),
            (("running",), "pending"),
            (("running", "succeeded"), "failed"),
            (("running", "failed"), "cancelled"),
            (("cancelled",), "running"),
        )

        for prior_states, new_state in invalid_transitions:
            with self.subTest(prior_states=prior_states, new_state=new_state):
                task = Task("A task")
                for state in prior_states:
                    task.transition(state)
                original_state = task.state
                original_events = list(task.events)

                with self.assertRaises(ValueError):
                    task.transition(new_state)

                self.assertEqual(task.state, original_state)
                self.assertEqual(task.events, original_events)

    def test_task_events_are_isolated(self):
        first = Task("First task")
        second = Task("Second task")

        first.transition("running")

        self.assertEqual(first.state, "running")
        self.assertEqual(len(first.events), 1)
        self.assertEqual(second.state, "pending")
        self.assertEqual(second.events, [])
        self.assertEqual(first.events[0].task_id, first.id)
        self.assertNotEqual(first.events[0].task_id, second.id)


class TaskSessionTests(unittest.TestCase):
    def test_new_sessions_have_unique_stable_identifiers_and_no_tasks(self):
        first = TaskSession()
        second = TaskSession()
        first_id = first.id

        self.assertTrue(first_id)
        self.assertNotEqual(first.id, second.id)
        self.assertEqual(first.id, first_id)
        self.assertEqual(first.list_tasks(), [])

    def test_create_task_keeps_description_and_returns_pending_task(self):
        session = TaskSession()

        task = session.create_task("Review the release notes")
        task_id = task.id

        self.assertIsInstance(task, Task)
        self.assertEqual(task.description, "Review the release notes")
        self.assertEqual(task.state, "pending")
        self.assertTrue(task_id)
        task.transition("running")
        self.assertEqual(task.id, task_id)

    def test_session_retrieves_task_by_identifier(self):
        session = TaskSession()
        task = session.create_task("Review the release notes")

        self.assertIs(session.get_task(task.id), task)

    def test_session_lists_tasks_in_creation_order(self):
        session = TaskSession()
        first = session.create_task("First task")
        second = session.create_task("Second task")

        self.assertEqual(session.list_tasks(), [first, second])

    def test_unknown_task_identifier_returns_none(self):
        session = TaskSession()

        self.assertIsNone(session.get_task("unknown"))

    def test_tasks_and_lifecycle_events_are_isolated_between_sessions(self):
        first_session = TaskSession()
        second_session = TaskSession()
        first = first_session.create_task("A task")
        second = second_session.create_task("A task")

        first.transition("running")

        self.assertNotEqual(first.id, second.id)
        self.assertIsNone(first_session.get_task(second.id))
        self.assertIsNone(second_session.get_task(first.id))
        self.assertEqual(first.state, "running")
        self.assertEqual(len(first.events), 1)
        self.assertEqual(second.state, "pending")
        self.assertEqual(second.events, [])


if __name__ == "__main__":
    unittest.main()
