//! A minimal Server-Sent Events parser for the bridge's `/v1/events` subscription.

/// Accumulates response chunks and yields complete `(event, data)` pairs. Chunks may split
/// anywhere, including inside a line or a multi-byte character; comment lines (the server's
/// keep-alives) and events without data are skipped.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

/// The event name when a message names none (the SSE default).
const DEFAULT_EVENT: &str = "message";
/// Bytes buffered for one unfinished line before the parser gives up on it, so a
/// misbehaving server cannot grow the buffer without bound.
const MAX_LINE: usize = 1 << 20;

impl SseParser {
    /// Feeds `chunk`; returns each event it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut out);
        }
        if self.buf.len() > MAX_LINE {
            self.buf.clear();
        }
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<(String, String)>) {
        if line.is_empty() {
            let event = self.event.take();
            let data = self.data.take();
            if let Some(data) = data {
                out.push((event.unwrap_or_else(|| DEFAULT_EVENT.to_string()), data));
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_split_chunks_and_comments() {
        let mut p = SseParser::default();
        // A keep-alive comment, then an event split mid-line and mid-character.
        assert!(p.push(b": keep-alive\n\n").is_empty());
        assert!(p.push(b"event: Wake").is_empty());
        assert!(p.push(b"Req\n").is_empty());
        let text = "data: {\"text\":\"h\u{e9}llo\"}\n".as_bytes();
        let (a, b) = text.split_at(text.len() - 6);
        assert!(p.push(a).is_empty());
        assert!(p.push(b).is_empty());
        let got = p.push(b"\n");
        assert_eq!(
            got,
            [(
                "WakeReq".to_string(),
                "{\"text\":\"h\u{e9}llo\"}".to_string()
            )]
        );
        // CRLF endings, several data lines, two events in one chunk, default event name.
        let got = p.push(b"event: A\r\ndata: 1\r\ndata: 2\r\n\r\ndata: x\n\n");
        assert_eq!(
            got,
            [
                ("A".to_string(), "1\n2".to_string()),
                ("message".to_string(), "x".to_string())
            ]
        );
        // An event with no data is dropped; "data:" with no space is accepted.
        assert!(p.push(b"event: Empty\n\n").is_empty());
        assert_eq!(
            p.push(b"event: B\ndata:y\n\n"),
            [("B".to_string(), "y".to_string())]
        );
    }
}
