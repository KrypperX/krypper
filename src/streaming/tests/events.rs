use super::*;

#[test]
fn events_are_ordered_and_json_round_trip() {
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    assert_eq!(
        emitter.emit(StreamEvent::ItemStarted {
            item_id: "i".into(),
            kind: ItemKind::Message
        }),
        Ok(1)
    );
    assert_eq!(
        emitter.emit(StreamEvent::TextDelta {
            item_id: "i".into(),
            text: "hello".into()
        }),
        Ok(2)
    );
    let sink = emitter.into_inner();
    assert_eq!(sink.0.len(), 2);
    let json = serde_json::to_string(&sink.0[1]).unwrap();
    assert_eq!(
        serde_json::from_str::<StreamEnvelope>(&json).unwrap(),
        sink.0[1]
    );
}
