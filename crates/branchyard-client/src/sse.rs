//! A Server-Sent Events parser: `id`, `event` and `data` fields, comments
//! and blank-line dispatch, per the WHATWG event-stream format. `retry` is
//! ignored; the caller chooses its own backoff.

use std::io::{self, BufRead, Read};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    /// `message` when the stream names none.
    pub event: String,
    pub data: String,
}

pub struct SseReader<R> {
    reader: R,
    /// Longest line accepted, so a broken server cannot exhaust memory.
    max_line: usize,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(reader: R) -> Self {
        SseReader {
            reader,
            max_line: 16 * 1024 * 1024,
        }
    }

    /// The next dispatched event, or `None` at the end of the stream. An
    /// event cut off by the end of the stream is dropped, as the format
    /// requires.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut event = SseEvent::default();
        let mut has_data = false;
        let mut has_id = false;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = Read::take(&mut self.reader, self.max_line as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                return Ok(None);
            }
            if line.len() > self.max_line {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event-stream line too long",
                ));
            }
            if line.last() != Some(&b'\n') {
                // End of stream in the middle of a line.
                return Ok(None);
            }
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if has_data || has_id {
                    if event.event.is_empty() {
                        event.event = "message".into();
                    }
                    return Ok(Some(event));
                }
                continue;
            }
            if line[0] == b':' {
                continue;
            }
            let text = String::from_utf8_lossy(&line);
            let (field, value) = match text.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (text.as_ref(), ""),
            };
            match field {
                "data" => {
                    if has_data {
                        event.data.push('\n');
                    }
                    event.data.push_str(value);
                    has_data = true;
                }
                "event" => event.event = value.to_owned(),
                "id" if !value.contains('\0') => {
                    event.id = Some(value.to_owned());
                    has_id = true;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(text: &str) -> Vec<SseEvent> {
        let mut reader = SseReader::new(text.as_bytes());
        let mut out = Vec::new();
        while let Some(event) = reader.next_event().unwrap() {
            out.push(event);
        }
        out
    }

    #[test]
    fn parses_fields_comments_and_multiline_data() {
        let got = events(
            ": keepalive\n\nevent: activity\nid: 7\ndata: {\"a\":1}\n\n\
             data:first\r\ndata: second\r\n\r\nid: 8\n\n",
        );
        assert_eq!(
            got,
            [
                SseEvent {
                    id: Some("7".into()),
                    event: "activity".into(),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    id: None,
                    event: "message".into(),
                    data: "first\nsecond".into()
                },
                SseEvent {
                    id: Some("8".into()),
                    event: "message".into(),
                    data: String::new()
                },
            ]
        );
    }

    #[test]
    fn a_truncated_event_is_dropped() {
        assert!(events("data: partial\n").is_empty());
        assert!(events("data: partial").is_empty());
    }
}
