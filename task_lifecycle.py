from dataclasses import dataclass, field
from uuid import uuid4


_VALID_TRANSITIONS = {
    "pending": {"running", "cancelled"},
    "running": {"succeeded", "failed", "cancelled"},
    "succeeded": set(),
    "failed": set(),
    "cancelled": set(),
}


@dataclass(frozen=True)
class TaskEvent:
    task_id: str
    sequence: int
    previous_state: str
    new_state: str


@dataclass
class Task:
    description: str
    id: str = field(default_factory=lambda: str(uuid4()), init=False)
    state: str = field(default="pending", init=False)
    events: list[TaskEvent] = field(default_factory=list, init=False)

    def transition(self, new_state: str) -> None:
        if not isinstance(new_state, str) or new_state not in _VALID_TRANSITIONS.get(self.state, ()):
            raise ValueError(f"cannot transition task from {self.state!r} to {new_state!r}")

        event = TaskEvent(
            task_id=self.id,
            sequence=len(self.events) + 1,
            previous_state=self.state,
            new_state=new_state,
        )
        self.events.append(event)
        self.state = new_state
