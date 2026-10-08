//! Incremental server-sent-events decoding.
//!
//! Both wire protocols stream SSE, but they frame it differently — Anthropic
//! names every event, OpenAI does not — so this stays a dumb frame splitter: it
//! hands back the raw `event`/`data` pair and lets the wire module interpret it.

/// One complete event from the stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the server sent one.
    pub event: Option<String>,
    /// Every `data:` line of the frame, joined with newlines.
    pub data: String,
}

/// Feeds response bytes in and yields complete events out, buffering whatever
/// tail is still incomplete.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet terminated by a newline.
    pending: Vec<u8>,
    data: String,
    event: Option<String>,
    /// Whether this frame has seen a `data:` line — an empty one still counts.
    has_data: bool,
}

impl SseDecoder {
    /// An empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `chunk`, appending every complete event to `out`.
    ///
    /// Takes bytes rather than `&str` because a chunk boundary can fall inside
    /// a multi-byte character: decoding has to wait for the whole line.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        self.pending.extend_from_slice(chunk);

        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            self.handle_line(&text, out);
        }
    }

    /// Flush a frame the server ended without a terminating blank line.
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            let text = String::from_utf8_lossy(&tail).into_owned();
            self.handle_line(&text, out);
        }
        self.dispatch(out);
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        // A line starting with a colon is a comment. Providers use it as a
        // keep-alive while the model is still thinking.
        if line.starts_with(':') {
            return;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if !self.has_data && self.event.is_none() {
            return;
        }
        out.push(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        });
        self.has_data = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&str]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk.as_bytes(), &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn decodes_a_named_event() {
        let events = decode(&["event: message_start\ndata: {\"a\":1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn decodes_an_unnamed_event() {
        let events = decode(&["data: {\"b\":2}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "{\"b\":2}");
    }

    #[test]
    fn a_data_value_needs_no_space_after_the_colon() {
        let events = decode(&["data:{\"c\":3}\n\n"]);
        assert_eq!(events[0].data, "{\"c\":3}");
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let events = decode(&["event: ping\r\ndata: {}\r\n\r\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn joins_multiple_data_lines() {
        let events = decode(&["data: line one\ndata: line two\n\n"]);
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn ignores_comments_and_heartbeats() {
        let events = decode(&[": keep-alive\n\n: another\ndata: real\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn an_empty_data_line_still_dispatches() {
        let events = decode(&["data:\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn reassembles_frames_split_across_chunks() {
        // Every possible split of one frame, including inside the CRLF.
        let frame = "event: content_block_delta\r\ndata: {\"t\":\"hi\"}\r\n\r\n";
        for split in 0..frame.len() {
            let (head, tail) = frame.split_at(split);
            let events = decode(&[head, tail]);
            assert_eq!(events.len(), 1, "split at {split}");
            assert_eq!(events[0].event.as_deref(), Some("content_block_delta"));
            assert_eq!(events[0].data, "{\"t\":\"hi\"}", "split at {split}");
        }
    }

    #[test]
    fn survives_a_chunk_boundary_inside_a_character() {
        let frame = "data: {\"t\":\"日本語\"}\n\n";
        let bytes = frame.as_bytes();
        // Split one byte into the first three-byte character.
        let cut = frame.find('日').expect("character present") + 1;

        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        decoder.push(&bytes[..cut], &mut out);
        assert!(out.is_empty(), "the line is not complete yet");

        decoder.push(&bytes[cut..], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "{\"t\":\"日本語\"}");
    }

    #[test]
    fn flushes_a_frame_without_a_trailing_blank_line() {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        decoder.push(b"data: [DONE]", &mut out);
        assert!(out.is_empty(), "nothing is complete yet");
        decoder.finish(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "[DONE]");
    }

    #[test]
    fn decodes_several_events_from_one_chunk() {
        let events = decode(&["data: one\n\ndata: two\n\n: ping\n\ndata: three\n\n"]);
        let data: Vec<_> = events.iter().map(|event| event.data.as_str()).collect();
        assert_eq!(data, vec!["one", "two", "three"]);
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let events = decode(&["id: 42\nretry: 1000\ndata: kept\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "kept");
    }
}
