use super::*;

#[test]
fn provider_text_delta_accepts_only_known_text_events() {
    let openai = SseFrame {
        event: Some("response.output_text.delta".into()),
        data: r#"{"delta":"hello"}"#.into(),
    };
    assert_eq!(provider_text_delta(&openai).as_deref(), Some("hello"));
    let anthropic = SseFrame {
        event: Some("content_block_delta".into()),
        data: r#"{"delta":{"type":"text_delta","text":"world"}}"#.into(),
    };
    assert_eq!(provider_text_delta(&anthropic).as_deref(), Some("world"));
    let tool_args = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: r#"{"delta":"secret"}"#.into(),
    };
    assert_eq!(provider_text_delta(&tool_args), None);
}

#[test]
fn provider_delta_bridge_emits_only_text_and_preserves_sequence() {
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    let tool_args = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: r#"{"delta":"do not emit"}"#.into(),
    };
    assert_eq!(
        emit_provider_text_delta(&mut emitter, "answer", &tool_args),
        Ok(false)
    );
    let text = SseFrame {
        event: Some("response.output_text.delta".into()),
        data: r#"{"delta":"hello"}"#.into(),
    };
    assert_eq!(
        emit_provider_text_delta(&mut emitter, "answer", &text),
        Ok(true)
    );
    let sink = emitter.into_inner();
    assert_eq!(sink.0.len(), 1);
    assert_eq!(sink.0[0].sequence, 1);
    assert_eq!(
        sink.0[0].event,
        StreamEvent::TextDelta {
            item_id: "answer".into(),
            text: "hello".into()
        }
    );
}

#[test]
fn provider_text_delta_rejects_malformed_or_unrecognized_payloads() {
    let invalid_json = SseFrame {
        event: Some("response.output_text.delta".into()),
        data: "{invalid".into(),
    };
    assert_eq!(provider_text_delta(&invalid_json), None);
    let wrong_type = SseFrame {
        event: Some("response.output_text.delta".into()),
        data: r#"{"delta":42}"#.into(),
    };
    assert_eq!(provider_text_delta(&wrong_type), None);
    let wrong_anthropic_delta = SseFrame {
        event: Some("content_block_delta".into()),
        data: r#"{"delta":{"type":"input_json_delta","text":"secret"}}"#.into(),
    };
    assert_eq!(provider_text_delta(&wrong_anthropic_delta), None);
    let unknown = SseFrame {
        event: Some("unknown.event".into()),
        data: r#"{"delta":"ignored"}"#.into(),
    };
    assert_eq!(provider_text_delta(&unknown), None);
}

#[test]
fn anthropic_text_delta_bridge_ignores_non_text_chunks() {
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    let tool_chunk = SseFrame {
        event: Some("content_block_delta".into()),
        data: r#"{"delta":{"type":"input_json_delta","partial_json":"secret"}}"#.into(),
    };
    assert_eq!(
        emit_provider_text_delta(&mut emitter, "message", &tool_chunk),
        Ok(false)
    );
    let text_chunk = SseFrame {
        event: Some("content_block_delta".into()),
        data: r#"{"delta":{"type":"text_delta","text":"hello"}}"#.into(),
    };
    assert_eq!(
        emit_provider_text_delta(&mut emitter, "message", &text_chunk),
        Ok(true)
    );
    let sink = emitter.into_inner();
    assert_eq!(sink.0.len(), 1);
    assert_eq!(sink.0[0].sequence, 1);
    assert_eq!(
        sink.0[0].event,
        StreamEvent::TextDelta {
            item_id: "message".into(),
            text: "hello".into()
        }
    );
}

#[test]
fn fragmented_provider_frames_keep_text_order_and_exclude_tool_arguments() {
    let payload = concat!(
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"hel\"}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"delta\":\"private args\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"lo\"}\n\n"
    );
    let mut decoder = SseDecoder::new(1024);
    let mut emitter = StreamEmitter::new(RecordingSink::default());
    for chunk in payload.as_bytes().chunks(3) {
        for frame in decoder.push(chunk).expect("decode fragmented SSE") {
            emit_provider_text_delta(&mut emitter, "message", &frame)
                .expect("emit recognized text");
        }
    }
    let events = emitter.into_inner().0;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].sequence, 2);
    assert_eq!(
        events[0].event,
        StreamEvent::TextDelta {
            item_id: "message".into(),
            text: "hel".into()
        }
    );
    assert_eq!(
        events[1].event,
        StreamEvent::TextDelta {
            item_id: "message".into(),
            text: "lo".into()
        }
    );
}
