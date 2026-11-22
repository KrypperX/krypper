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
