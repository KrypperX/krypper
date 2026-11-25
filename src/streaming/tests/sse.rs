use super::*;
use proptest::prelude::*;

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

fn sse_line() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..20).prop_map(|characters| {
        characters
            .into_iter()
            .filter(|character| !matches!(character, '\r' | '\n'))
            .collect()
    })
}

struct ChunkedReader<'a> {
    input: &'a [u8],
    sizes: &'a [usize],
    offset: usize,
    next_size: usize,
}

impl std::io::Read for ChunkedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.input.len() {
            return Ok(0);
        }
        let size = self.sizes[self.next_size % self.sizes.len()].max(1);
        self.next_size += 1;
        let count = size.min(buffer.len()).min(self.input.len() - self.offset);
        buffer[..count].copy_from_slice(&self.input[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn sse_decoding_is_independent_of_byte_chunk_boundaries(
        frames in prop::collection::vec(
            (prop::option::of(sse_line()), prop::collection::vec(sse_line(), 1..4)),
            0..8,
        ),
        crlf in any::<bool>(),
        chunk_sizes in prop::collection::vec(1usize..16, 1..12),
    ) {
        let ending = if crlf { "\r\n" } else { "\n" };
        let mut wire = String::new();
        let mut expected = Vec::with_capacity(frames.len());
        for (event, data_lines) in frames {
            if let Some(event_value) = &event {
                wire.push_str("event:");
                wire.push_str(event_value);
                wire.push_str(ending);
            }
            for line in &data_lines {
                wire.push_str("data:");
                wire.push_str(line);
                wire.push_str(ending);
            }
            wire.push_str(ending);
            expected.push(SseFrame {
                event: event.map(|value| {
                    value
                        .strip_prefix(' ')
                        .unwrap_or(&value)
                        .to_owned()
                }),
                data: data_lines
                    .iter()
                    .map(|line| line.strip_prefix(' ').unwrap_or(line))
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
        }
        let bytes = wire.as_bytes();

        let mut bytewise_reader = ChunkedReader {
            input: bytes,
            sizes: &[1],
            offset: 0,
            next_size: 0,
        };
        let mut bytewise_frames = Vec::new();
        let bytewise_result = consume_sse(
            &mut bytewise_reader,
            bytes.len().saturating_add(1),
            |frame| {
                bytewise_frames.push(frame);
                Ok::<(), ()>(())
            },
        );
        prop_assert!(bytewise_result.is_ok());
        prop_assert_eq!(bytewise_frames, expected.clone());

        let mut chunked_reader = ChunkedReader {
            input: bytes,
            sizes: &chunk_sizes,
            offset: 0,
            next_size: 0,
        };
        let mut chunked_frames = Vec::new();
        let chunked_result = consume_sse(
            &mut chunked_reader,
            bytes.len().saturating_add(1),
            |frame| {
                chunked_frames.push(frame);
                Ok::<(), ()>(())
            },
        );
        prop_assert!(chunked_result.is_ok());
        prop_assert_eq!(chunked_frames, expected);
    }
}
