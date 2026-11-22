use super::*;

#[test]
fn jsonl_sink_emits_one_parseable_line_per_event() {
    let mut emitter = StreamEmitter::new(JsonlSink::new(Vec::<u8>::new()));
    emitter
        .emit(StreamEvent::ItemStarted {
            item_id: "message".into(),
            kind: ItemKind::Message,
        })
        .unwrap();
    emitter
        .emit(StreamEvent::ItemCompleted {
            item_id: "message".into(),
        })
        .unwrap();
    let bytes = emitter.into_inner().into_inner();
    let lines: Vec<_> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(lines.len(), 2);
    let first: StreamEnvelope = serde_json::from_slice(lines[0]).unwrap();
    let second: StreamEnvelope = serde_json::from_slice(lines[1]).unwrap();
    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
}

#[test]
fn jsonl_sink_propagates_write_failure_without_advancing_sequence() {
    struct RejectWriter;
    impl std::io::Write for RejectWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("consumer disconnected"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut emitter = StreamEmitter::new(JsonlSink::new(RejectWriter));
    let result = emitter.emit(StreamEvent::ItemCompleted {
        item_id: "test".into(),
    });
    assert!(result.is_err());
    assert_eq!(emitter.next_sequence(), 1);
}

#[test]
fn failed_delivery_is_not_acknowledged() {
    let mut emitter = StreamEmitter::new(FailingSink);
    assert_eq!(
        emitter.emit(StreamEvent::StreamError {
            code: "failed".into()
        }),
        Err(EmitError::Sink("disconnected"))
    );
}

/// Regression: accepted envelopes remain contiguous after a rejected send.
/// This does not assert atomic writes or automatic replay by a real sink.
#[test]
fn failed_sink_delivery_does_not_advance_sequence_on_retry() {
    let mut emitter = StreamEmitter::new(FailOnceSink {
        attempts: 0,
        delivered: Vec::new(),
    });
    assert_eq!(
        emitter.emit(StreamEvent::ItemStarted {
            item_id: "message".into(),
            kind: ItemKind::Message,
        }),
        Err(EmitError::Sink("backpressure"))
    );
    assert_eq!(
        emitter.emit(StreamEvent::ItemStarted {
            item_id: "message".into(),
            kind: ItemKind::Message,
        }),
        Ok(1)
    );
    assert_eq!(
        emitter.emit(StreamEvent::ItemCompleted {
            item_id: "message".into(),
        }),
        Ok(2)
    );
    let sink = emitter.into_inner();
    assert_eq!(sink.attempts, 3);
    assert_eq!(sink.delivered.len(), 2);
    assert_eq!(sink.delivered[0].sequence, 1);
    assert_eq!(sink.delivered[1].sequence, 2);
}

#[test]
fn text_sink_renders_only_public_event_content() {
    let mut emitter = StreamEmitter::new(TextSink::new(Vec::<u8>::new()));
    emitter
        .emit(StreamEvent::TurnStarted {
            task_id: "private-task".into(),
            turn_id: "turn".into(),
        })
        .unwrap();
    emitter
        .emit(StreamEvent::TextDelta {
            item_id: "message".into(),
            text: "hello".into(),
        })
        .unwrap();
    emitter
        .emit(StreamEvent::TurnCompleted {
            task_id: "private-task".into(),
            turn_id: "turn".into(),
            status: "completed".into(),
        })
        .unwrap();
    let output = String::from_utf8(emitter.into_inner().into_inner()).unwrap();
    assert!(output.contains("hello"));
    assert!(output.contains("\n[turn: completed]\n"));
    assert!(!output.contains("private-task"));
}

#[test]
fn activity_jsonl_supports_dyn_writer_and_excludes_task_goal() {
    let mut session = crate::task_lifecycle::TaskSession::new();
    session.create_task("private goal must not appear");
    let mut bytes = Vec::<u8>::new();
    let writer: &mut dyn std::io::Write = &mut bytes;
    write_activity_positions(&session, writer).expect("activity JSONL write");
    let output = String::from_utf8(bytes).expect("UTF-8 JSONL");
    assert!(!output.contains("private goal must not appear"));
    for line in output.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("valid activity JSONL");
        assert!(record.get("task_id").is_some());
        assert!(record.get("lifecycle_position").is_some());
        assert!(record.get("activity_position").is_some());
    }
}

#[test]
fn jsonl_sink_preserves_sequence_and_event_types() {
    let mut emitter = StreamEmitter::new(JsonlSink::new(Vec::<u8>::new()));
    assert_eq!(
        emitter
            .emit(StreamEvent::ItemStarted {
                item_id: "a".into(),
                kind: ItemKind::Message,
            })
            .expect("JSONL sink write"),
        1
    );
    assert_eq!(
        emitter
            .emit(StreamEvent::ItemCompleted {
                item_id: "a".into(),
            })
            .expect("JSONL sink write"),
        2
    );
    let output = String::from_utf8(emitter.into_inner().into_inner()).unwrap();
    let records: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["sequence"], 1);
    assert_eq!(records[1]["sequence"], 2);
    assert_eq!(records[0]["type"], "item_started");
    assert_eq!(records[1]["type"], "item_completed");
}
