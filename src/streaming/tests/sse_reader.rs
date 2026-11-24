use super::*;

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
fn sse_reader_rejects_truncated_frame_at_eof() {
    let mut input = std::io::Cursor::new(b"event: content_block_delta\ndata: {\"delta\":".to_vec());
    let result = consume_sse(&mut input, 1024, |_frame| Ok::<_, ()>(()));
    assert!(matches!(
        result,
        Err(SseReadError::Decode(SseDecodeError::TruncatedFrame))
    ));
}

#[test]
fn sse_reader_accepts_complete_frame_at_eof() {
    let mut input = std::io::Cursor::new(b"data: ok\n\n".to_vec());
    let mut frames = Vec::new();
    consume_sse(&mut input, 1024, |frame| {
        frames.push(frame);
        Ok::<_, ()>(())
    })
    .expect("complete stream");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "ok");
}

#[test]
fn sse_reader_rejects_cumulative_bytes_over_limit_across_small_events() {
    let input = b"data: x\n\n".repeat(32);
    let mut reader = std::io::Cursor::new(input);
    let mut delivered = 0;
    let result = consume_sse(&mut reader, 64, |_| {
        delivered += 1;
        Ok::<(), ()>(())
    });

    assert!(matches!(result, Err(SseReadError::StreamTooLarge)));
    assert_eq!(delivered, 0);
}

#[test]
fn sse_reader_rejects_excessive_empty_and_heartbeat_frames() {
    let input = b":\n\n".repeat(super::super::sse::MAX_SSE_FRAME_COUNT + 1);
    let mut reader = std::io::Cursor::new(input);
    let result = consume_sse(&mut reader, 2 * 1024 * 1024, |_| Ok::<(), ()>(()));

    assert!(matches!(result, Err(SseReadError::TooManyFrames)));
}

#[test]
fn sse_reader_accepts_a_long_stream_within_both_limits() {
    let input = b"data: x\n\n".repeat(20_000);
    let mut reader = std::io::Cursor::new(input);
    let mut delivered = 0;
    consume_sse(&mut reader, 2 * 1024 * 1024, |_| {
        delivered += 1;
        Ok::<(), ()>(())
    })
    .expect("long stream remains within byte and frame limits");

    assert_eq!(delivered, 20_000);
}

#[test]
fn sse_decoder_limits_each_frame_not_the_entire_read_chunk() {
    let mut decoder = SseDecoder::new(9);
    let frames = decoder.push(b"data: a\n\ndata: b\n\n").unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].data, "a");
    assert_eq!(frames[1].data, "b");
}

#[test]
fn sse_decoder_resets_after_invalid_utf8() {
    let mut decoder = SseDecoder::new(32);
    assert_eq!(
        decoder.push(b"data: \xff\n"),
        Err(SseDecodeError::InvalidUtf8)
    );
    let frames = decoder.push(b"data: ok\n\n").unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "ok");
}
