//! Incremental server-sent-events parser.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseRecord {
    pub event: Option<String>,
    pub data: String,
}

/// Feed arbitrary byte chunks with [`SseParser::push`]; records come out as their terminating blank
/// line arrives. Chunks may split lines, `\r\n` pairs and UTF-8 sequences anywhere.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    started: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseRecord> {
        self.buf.extend_from_slice(chunk);
        let mut records = Vec::new();
        let mut start = 0;
        while let Some(rel) = self.buf[start..].iter().position(|&b| b == b'\n' || b == b'\r') {
            let end = start + rel;
            let next = if self.buf[end] == b'\r' {
                match self.buf.get(end + 1) {
                    Some(b'\n') => end + 2,
                    Some(_) => end + 1,
                    // A trailing `\r` may be the first half of `\r\n`; wait for the next chunk.
                    None => break,
                }
            } else {
                end + 1
            };
            // A line terminator never falls inside a UTF-8 sequence, so each line is complete text.
            let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
            start = next;
            if let Some(record) = self.line(&line) {
                records.push(record);
            }
        }
        self.buf.drain(..start);
        records
    }

    /// Call at end of stream: yields a record whose terminating blank line never arrived.
    pub fn finish(&mut self) -> Option<SseRecord> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            let line = line.trim_end_matches('\r');
            if let Some(record) = self.line(line) {
                return Some(record);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseRecord> {
        let line = if self.started {
            line
        } else {
            self.started = true;
            line.strip_prefix('\u{feff}').unwrap_or(line)
        };
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseRecord> {
        let event = self.event.take();
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        Some(SseRecord {
            event,
            data: std::mem::take(&mut self.data),
        })
    }
}

/// Splits a byte stream of newline-delimited JSON into its lines, each as a record with no event
/// name. Chunks may split a line or a UTF-8 sequence anywhere; blank lines are skipped.
#[derive(Debug, Default)]
pub struct JsonLines {
    buf: Vec<u8>,
}

impl JsonLines {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseRecord> {
        self.buf.extend_from_slice(chunk);
        let mut records = Vec::new();
        let mut start = 0;
        while let Some(rel) = self.buf[start..].iter().position(|&b| b == b'\n') {
            records.extend(record(&self.buf[start..start + rel]));
            start += rel + 1;
        }
        self.buf.drain(..start);
        records
    }

    /// Call at end of stream: yields a last line that had no newline after it.
    pub fn finish(&mut self) -> Option<SseRecord> {
        record(&std::mem::take(&mut self.buf))
    }
}

fn record(line: &[u8]) -> Option<SseRecord> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    (!text.is_empty()).then(|| SseRecord {
        event: None,
        data: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(event: Option<&str>, data: &str) -> SseRecord {
        SseRecord {
            event: event.map(str::to_string),
            data: data.to_string(),
        }
    }

    fn parse_in_chunks(input: &[u8], size: usize) -> Vec<SseRecord> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for chunk in input.chunks(size) {
            out.extend(parser.push(chunk));
        }
        out.extend(parser.finish());
        out
    }

    #[test]
    fn parses_events_data_comments_and_multiline_data() {
        let input = b": keep-alive\n\nevent: delta\ndata: {\"a\":1}\n\ndata: line one\ndata: line two\n\ndata:no-space\n\n";
        assert_eq!(
            parse_in_chunks(input, input.len()),
            vec![
                rec(Some("delta"), "{\"a\":1}"),
                rec(None, "line one\nline two"),
                rec(None, "no-space")
            ]
        );
    }

    #[test]
    fn output_is_identical_for_every_chunk_size() {
        let input =
            "\u{feff}event: a\r\ndata: h\u{e9}llo \u{1f600}\r\n\r\ndata: x\rdata: y\r\rdata: tail\n\n"
                .as_bytes();
        let expected = vec![
            rec(Some("a"), "h\u{e9}llo \u{1f600}"),
            rec(None, "x\ny"),
            rec(None, "tail"),
        ];
        for size in 1..=input.len() {
            assert_eq!(parse_in_chunks(input, size), expected, "chunk size {size}");
        }
    }

    #[test]
    fn event_without_data_is_dropped_and_does_not_leak_into_next_record() {
        assert_eq!(
            parse_in_chunks(b"event: ping\n\ndata: d\n\n", 5),
            vec![rec(None, "d")]
        );
    }

    #[test]
    fn finish_flushes_unterminated_record() {
        assert_eq!(
            parse_in_chunks(b"event: e\ndata: partial", 4),
            vec![rec(Some("e"), "partial")]
        );
    }
}
