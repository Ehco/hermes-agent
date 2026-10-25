//! Minimal incremental Server-Sent-Events parser shared by both transports.
//!
//! Faithful to the SSE wire rules both providers rely on: fields are
//! terminated by LF (with CRLF tolerated), `data:` payloads accumulate across
//! lines within one event, a blank line dispatches the event, and `:` comment
//! lines (Anthropic keep-alives) are ignored. `event:`/`id:`/`retry:` fields
//! are captured/ignored as appropriate.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Value of the last `event:` field in the block, if any.
    pub event: Option<String>,
    /// Joined `data:` payload (multiple `data:` lines joined with `\n`).
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data_lines: Vec<String>,
    /// Set once any field was seen for the current block.
    started: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw stream bytes; returns every event completed by them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(ev) = self.line(&line[..line.len() - 1]) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush a stream that ended without a trailing blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(ev) = self.line(&line) {
                out.push(ev);
            }
        }
        if let Some(ev) = self.take_event() {
            out.push(ev);
        }
        out
    }

    /// Handle one line (without its terminator). `Some` = event complete.
    fn line(&mut self, line: &[u8]) -> Option<SseEvent> {
        let text = String::from_utf8_lossy(line);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        if text.is_empty() {
            return self.take_event();
        }
        if text.starts_with(':') {
            return None; // comment / keep-alive
        }
        self.started = true;
        let (field, value) = match text.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (text, ""),
        };
        match field {
            "data" => self.data_lines.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            // "id" / "retry" / unknown fields: ignored.
            _ => {}
        }
        None
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        if !self.started {
            return None;
        }
        let event = self.event.take();
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.started = false;
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<SseEvent> {
        SseParser::new().finish_with(bytes)
    }

    impl SseParser {
        fn finish_with(mut self, bytes: &[u8]) -> Vec<SseEvent> {
            let mut evs = self.feed(bytes);
            evs.extend(self.finish());
            evs
        }
    }

    #[test]
    fn parses_openai_data_lines_and_done_sentinel() {
        let evs = events(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n");
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].data, "{\"a\":1}");
        assert_eq!(evs[1].data, "{\"b\":2}");
        assert_eq!(evs[2].data, "[DONE]");
    }

    #[test]
    fn handles_crlf_and_no_space_after_colon() {
        let evs = events(b"data:{\"x\":1}\r\n\r\ndata: [DONE]\r\n\r\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "{\"x\":1}");
    }

    #[test]
    fn keeps_event_names_and_multiline_data() {
        let evs = events(
            b": keep-alive comment\n\
              event: content_block_delta\n\
              id: 42\n\
              data: {\"i\":1}\n\
              data: {\"i\":2}\n\n",
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("content_block_delta"));
        assert_eq!(evs[0].data, "{\"i\":1}\n{\"i\":2}");
    }

    #[test]
    fn reassembles_events_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"par").is_empty());
        // The event completes as soon as its terminating newline arrives,
        // possibly mid-chunk.
        let first = p.feed(b"t\":1}\n\ndata: [");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].data, "{\"part\":1}");
        let second = p.feed(b"DONE]\n\n");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].data, "[DONE]");
    }

    #[test]
    fn flushes_trailing_partial_event_at_eof() {
        let mut p = SseParser::new();
        let mut evs = p.feed(b"data: tail-no-newline");
        assert!(evs.is_empty());
        evs.extend(p.finish());
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "tail-no-newline");
    }

    #[test]
    fn ignores_blank_lines_and_comments_without_fields() {
        let evs = events(b"\n\n: ping\n\n\n");
        assert!(evs.is_empty());
    }
}
