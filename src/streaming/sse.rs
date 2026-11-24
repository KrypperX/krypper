/// A complete server-sent event frame, without provider-specific interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Maximum number of complete SSE frames accepted from one provider response.
/// The byte budget is independent and remains the existing 2 MiB response cap.
pub(crate) const MAX_SSE_FRAME_COUNT: usize = 65_536;

/// Incremental, bounded SSE framing. Does not execute or authorize model actions.
pub struct SseDecoder {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    limit: usize,
    frame_bytes: usize,
    completed_frames: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseDecodeError {
    FrameTooLarge,
    InvalidUtf8,
    TruncatedFrame,
}

impl SseDecoder {
    pub fn new(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            event: None,
            data: Vec::new(),
            limit,
            frame_bytes: 0,
            completed_frames: 0,
        }
    }

    /// Reject an EOF in the middle of an SSE frame instead of silently
    /// treating a partial provider event as a successful stream.
    pub fn finish(&self) -> Result<(), SseDecodeError> {
        if self.pending.is_empty() && self.data.is_empty() && self.event.is_none() {
            Ok(())
        } else {
            Err(SseDecodeError::TruncatedFrame)
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, SseDecodeError> {
        let mut frames = Vec::new();
        for &byte in chunk {
            if self.frame_bytes >= self.limit {
                self.pending.clear();
                self.data.clear();
                self.event = None;
                self.frame_bytes = 0;
                return Err(SseDecodeError::FrameTooLarge);
            }
            self.frame_bytes += 1;
            self.pending.push(byte);
            if byte != b'\n' {
                continue;
            }
            let mut line = std::mem::take(&mut self.pending);
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                self.frame_bytes = 0;
                self.completed_frames = self.completed_frames.saturating_add(1);
                if !self.data.is_empty() {
                    frames.push(SseFrame {
                        event: self.event.take(),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                } else {
                    self.event = None;
                }
                continue;
            }
            if line.first() == Some(&b':') {
                continue;
            }
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(_) => {
                    self.pending.clear();
                    self.data.clear();
                    self.event = None;
                    self.frame_bytes = 0;
                    return Err(SseDecodeError::InvalidUtf8);
                }
            };
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        Ok(frames)
    }

    fn completed_frames(&self) -> usize {
        self.completed_frames
    }
}

/// Read SSE frames directly from a streaming HTTP response body or any reader.
///
/// Frames are delivered immediately as they arrive; the entire response is
/// never buffered. `max_stream_bytes` limits both an individual frame and the
/// complete response body. At most 65,536 complete frames, including empty and
/// heartbeat frames, are accepted. The callback's error
/// stops consumption (for cancellation, sink backpressure, or a disconnected
/// consumer). Callers must validate provider-specific event payloads before
/// using them for model decisions.
pub fn consume_sse<R, F, E>(
    reader: &mut R,
    max_stream_bytes: usize,
    mut on_frame: F,
) -> Result<(), SseReadError<E>>
where
    R: std::io::Read + ?Sized,
    F: FnMut(SseFrame) -> Result<(), E>,
{
    let mut decoder = SseDecoder::new(max_stream_bytes);
    let mut buffer = [0u8; 4096];
    let mut total_bytes = 0usize;
    loop {
        let read_limit = max_stream_bytes
            .saturating_sub(total_bytes)
            .saturating_add(1)
            .min(buffer.len());
        let count = reader
            .read(&mut buffer[..read_limit])
            .map_err(SseReadError::Io)?;
        if count == 0 {
            return decoder.finish().map_err(SseReadError::Decode);
        }
        total_bytes = total_bytes.saturating_add(count);
        if total_bytes > max_stream_bytes {
            return Err(SseReadError::StreamTooLarge);
        }
        let frames = decoder
            .push(&buffer[..count])
            .map_err(SseReadError::Decode)?;
        if decoder.completed_frames() > MAX_SSE_FRAME_COUNT {
            return Err(SseReadError::TooManyFrames);
        }
        for frame in frames {
            on_frame(frame).map_err(SseReadError::Consumer)?;
        }
    }
}

#[derive(Debug)]
pub enum SseReadError<E> {
    Io(std::io::Error),
    Decode(SseDecodeError),
    StreamTooLarge,
    TooManyFrames,
    Consumer(E),
}

/// A provider-neutral text delta extracted from a recognized SSE event.
/// Tool-call argument deltas are deliberately excluded: they must be
/// assembled and validated by the provider's existing action parser.
pub fn provider_text_delta(frame: &SseFrame) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&frame.data).ok()?;
    match frame.event.as_deref() {
        Some("response.output_text.delta") => value
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Some("content_block_delta") => {
            let delta = value.get("delta")?;
            if delta.get("type")?.as_str()? != "text_delta" {
                return None;
            }
            delta
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }
        _ => None,
    }
}
