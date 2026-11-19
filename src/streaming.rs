//! Opt-in, transport-neutral streaming envelopes.
//!
//! This module does not own task state or grant tool permissions. Consumers
//! project existing runtime observations into events and deliver them using
//! their own transport. No event is persisted by this module.

mod emitter;
mod events;
mod projection;
mod responses;
mod sink;
mod sse;

pub use emitter::{EmitError, EventSink, StreamEmitter};
pub use events::{ItemKind, StreamEnvelope, StreamEvent};
pub use projection::{
    emit_activity_positions, emit_changed_activity_positions, snapshot_positions, ActivityPosition,
};
pub use responses::{
    emit_provider_text_delta, ArgumentsError, ResponsesArguments, ResponsesCallBatch,
    ResponsesFinalActions,
};
pub use sink::{write_activity_positions, JsonlSink, TextSink};
pub use sse::{
    consume_sse, provider_text_delta, SseDecodeError, SseDecoder, SseFrame, SseReadError,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink(Vec<StreamEnvelope>);
    impl EventSink for RecordingSink {
        type Error = ();
        fn send(&mut self, event: &StreamEnvelope) -> Result<(), ()> {
            self.0.push(event.clone());
            Ok(())
        }
    }

    #[test]
    fn activity_projection_reuses_snapshot_positions_without_mutating_session() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        session.create_task("private goal never emitted");
        session.create_task("another private goal");
        let before = session.snapshot();
        let positions = snapshot_positions(&session);
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        emit_activity_positions(&session, &mut emitter).unwrap();
        let events = emitter.into_inner().0;
        assert_eq!(events.len(), positions.len());
        for (index, (event, position)) in events.iter().zip(positions.iter()).enumerate() {
            assert_eq!(event.version, 1);
            assert_eq!(event.sequence, index as u64 + 1);
            assert_eq!(
                event.event,
                StreamEvent::ActivityPositionUpdated {
                    task_id: position.task_id.clone(),
                    lifecycle_position: position.lifecycle_position,
                    activity_position: position.activity_position,
                }
            );
            let serialized = serde_json::to_string(event).unwrap();
            assert!(!serialized.contains("private goal"));
        }
        assert_eq!(session.snapshot(), before);
    }

    #[test]
    fn activity_projection_stops_on_sink_failure_without_mutating_session() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        session.create_task("private goal");
        let before = session.snapshot();
        let mut emitter = StreamEmitter::new(FailingSink);
        assert_eq!(
            emit_activity_positions(&session, &mut emitter),
            Err(EmitError::Sink("disconnected"))
        );
        assert_eq!(session.snapshot(), before);
    }

    #[test]
    fn unchanged_snapshot_emits_no_duplicate_position_events() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        session.create_task("private");
        let positions = snapshot_positions(&session);
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        emit_changed_activity_positions(&session, &positions, &mut emitter).unwrap();
        assert!(emitter.into_inner().0.is_empty());
    }

    #[test]
    fn new_task_position_is_emitted_even_with_zero_high_water_marks() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        let previous = snapshot_positions(&session);
        session.create_task("private");
        let before = session.snapshot();
        let mut emitter = StreamEmitter::new(RecordingSink::default());
        emit_changed_activity_positions(&session, &previous, &mut emitter).unwrap();
        let events = emitter.into_inner().0;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(session.snapshot(), before);
        assert!(!serde_json::to_string(&events).unwrap().contains("private"));
    }

    #[test]
    fn changed_projection_failure_does_not_mutate_authoritative_snapshot() {
        let mut session = crate::task_lifecycle::TaskSession::new();
        session.create_task("private");
        let before = session.snapshot();
        let mut emitter = StreamEmitter::new(FailingSink);
        assert_eq!(
            emit_changed_activity_positions(&session, &[], &mut emitter),
            Err(EmitError::Sink("disconnected"))
        );
        assert_eq!(session.snapshot(), before);
    }

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
    fn sse_consumer_cancellation_stops_before_next_read() {
        struct OneReadOnly {
            reads: usize,
        }
        impl std::io::Read for OneReadOnly {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads > 1 {
                    panic!("SSE reader was polled after cancellation");
                }
                let bytes = b"data: first\n\ndata: second\n\n";
                buffer[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        let mut reader = OneReadOnly { reads: 0 };
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Err("cancelled")
        });
        assert!(matches!(result, Err(SseReadError::Consumer("cancelled"))));
        assert_eq!(delivered, 1);
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn sse_sink_backpressure_propagates_without_consuming_more_input() {
        struct OneReadOnly {
            reads: usize,
        }
        impl std::io::Read for OneReadOnly {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads > 1 {
                    panic!("SSE reader was polled after sink failure");
                }
                let bytes = b"event: response.output_text.delta\ndata: {\"delta\":\"hello\"}\n\n";
                buffer[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        struct RejectSink;
        impl EventSink for RejectSink {
            type Error = &'static str;
            fn send(&mut self, _: &StreamEnvelope) -> Result<(), Self::Error> {
                Err("backpressure")
            }
        }
        let mut reader = OneReadOnly { reads: 0 };
        let mut emitter = StreamEmitter::new(RejectSink);
        let result = consume_sse(&mut reader, 1024, |frame| {
            emit_provider_text_delta(&mut emitter, "message", &frame).map(|_| ())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Consumer(EmitError::Sink("backpressure")))
        ));
        assert_eq!(reader.reads, 1);
        assert_eq!(emitter.next_sequence(), 1);
    }

    #[test]
    fn sse_decoder_handles_split_crlf_and_multiple_data_lines() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\r\n")
            .unwrap()
            .is_empty());
        assert!(decoder.push(b"data: one\r\ndata: tw").unwrap().is_empty());
        let frames = decoder.push(b"o\r\n\r\n").unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("response.output_text.delta".into()),
                data: "one\ntwo".into(),
            }]
        );
    }

    #[test]
    fn sse_decoder_ignores_heartbeat_comments_without_leaking_event_state() {
        let mut decoder = SseDecoder::new(1024);
        let frames = decoder
            .push(b": heartbeat\n\nevent: response.output_text.delta\ndata: one\n\n: ping\n\ndata: two\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].event, None);
        assert_eq!(frames[1].data, "two");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_rejects_oversized_frame_across_chunks_and_resets() {
        let mut decoder = SseDecoder::new(12);
        assert!(decoder.push(b"data: long").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"er-than-limit"),
            Err(SseDecodeError::FrameTooLarge)
        );
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: ok\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
    }

    #[test]
    fn sse_decoder_rejects_oversized_incomplete_frame() {
        let mut decoder = SseDecoder::new(4);
        assert_eq!(decoder.push(b"12345"), Err(SseDecodeError::FrameTooLarge));
    }

    #[test]
    fn sse_decoder_rejects_partial_event_at_eof() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\ndata: {\"delta\":\"partial\"}")
            .unwrap()
            .is_empty());
        assert_eq!(decoder.finish(), Err(SseDecodeError::TruncatedFrame));
    }

    #[test]
    fn sse_decoder_recovers_cleanly_after_invalid_utf8() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\n")
            .unwrap()
            .is_empty());
        assert_eq!(
            decoder.push(b"data: \xff\n"),
            Err(SseDecodeError::InvalidUtf8)
        );
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: recovered\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].data, "recovered");
    }

    #[test]
    fn sse_decoder_rejects_truncated_utf8_and_recovers_after_error() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder.push(b"data: \xe2\x82").unwrap().is_empty());
        assert_eq!(decoder.finish(), Err(SseDecodeError::TruncatedFrame));
        assert_eq!(decoder.push(b"\xff\n"), Err(SseDecodeError::InvalidUtf8));
        assert_eq!(decoder.finish(), Ok(()));
        let frames = decoder.push(b"data: valid\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "valid");
    }

    #[test]
    fn sse_reader_rejects_truncated_provider_frame_at_eof() {
        let mut reader = std::io::Cursor::new(b"event: response.completed\ndata: {}".to_vec());
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Ok::<(), ()>(())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Decode(SseDecodeError::TruncatedFrame))
        ));
        assert_eq!(delivered, 0);
    }

    #[test]
    fn sse_reader_rejects_invalid_utf8_before_dispatch() {
        let mut reader = std::io::Cursor::new(b"data: \xff\n\n".to_vec());
        let mut delivered = 0;
        let result = consume_sse(&mut reader, 1024, |_| {
            delivered += 1;
            Ok::<(), ()>(())
        });
        assert!(matches!(
            result,
            Err(SseReadError::Decode(SseDecodeError::InvalidUtf8))
        ));
        assert_eq!(delivered, 0);
    }

    #[test]
    fn sse_decoder_accepts_frame_at_exact_byte_limit() {
        let frame = b"data: ok\n\n";
        let mut decoder = SseDecoder::new(frame.len());
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "ok");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_preserves_multiline_data_across_chunk_boundaries() {
        let mut decoder = SseDecoder::new(1024);
        assert!(decoder
            .push(b"event: response.output_text.delta\r")
            .unwrap()
            .is_empty());
        assert!(decoder.push(b"\ndata: first\r\n").unwrap().is_empty());
        assert!(decoder.push(b"data: second\r\n\r").unwrap().is_empty());
        let frames = decoder.push(b"\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(frames[0].data, "first\nsecond");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_decoder_ignores_unknown_fields_and_resets_between_events() {
        let mut decoder = SseDecoder::new(1024);
        let frames = decoder
            .push(b"id: 7\nretry: 1000\nevent: first\ndata: one\n\ndata: two\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("first"));
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].event, None);
        assert_eq!(frames[1].data, "two");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_reader_delivers_frames_without_waiting_for_eof() {
        let mut reader = std::io::Cursor::new(b"data: first\n\ndata: second\n\n".to_vec());
        let mut seen = Vec::new();
        let result = consume_sse(&mut reader, 1024, |frame| {
            seen.push(frame.data);
            Ok::<(), ()>(())
        });
        assert!(result.is_ok());
        assert_eq!(seen, vec!["first", "second"]);
    }

    #[test]
    fn sse_reader_propagates_consumer_cancellation() {
        let mut reader = std::io::Cursor::new(b"data: first\n\ndata: second\n\n".to_vec());
        let result = consume_sse(&mut reader, 1024, |_| Err("cancelled"));
        assert!(matches!(result, Err(SseReadError::Consumer("cancelled"))));
    }

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
