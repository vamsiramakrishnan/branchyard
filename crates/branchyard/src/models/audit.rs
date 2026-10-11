//! The contents of calls, kept when `[models] audit = "contents"`: one JSON
//! line per call appended to `.branchyard/models/audit/<branch>/<turn>.jsonl`,
//! its directory readable by this user only (0700) and the file too (0600).
//! They are never written to the event log. `docs/model-gateway.md`
//! ("Interception records") says what is and is not in a line.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::sync::Mutex;

use branchyard_support::LockExt as _;
use serde_json::{json, Value};

/// The most of a response kept for a line; a longer response is cut at a
/// chunk and the line says so.
pub(crate) const MAX_RESPONSE: usize = 64 * 1024 * 1024;

/// One call's contents, as a line is written from them.
pub(crate) struct Entry<'a> {
    /// When the call ended, in milliseconds since the epoch.
    pub at_ms: u64,
    pub model: &'a str,
    pub backend: &'a str,
    /// The request body as the harness sent it.
    pub request: &'a [u8],
    /// The response body as the backend sent it.
    pub response: &'a [u8],
    /// The response is a stream of server-sent events, kept as its raw
    /// text.
    pub sse: bool,
    /// The response was longer than [`MAX_RESPONSE`] and is cut.
    pub truncated: bool,
}

impl Entry<'_> {
    /// `{"at_ms", "model", "backend", "request", "response"}`, plus
    /// `"response_truncated": true` when the response is cut. A body is
    /// its JSON when it parses (and is not a stream), else its text (bytes
    /// that are not UTF-8 are replaced), and `null` when there is none.
    pub(crate) fn line(&self) -> Value {
        let mut line = json!({
            "at_ms": self.at_ms,
            "model": self.model,
            "backend": self.backend,
            "request": body(self.request, false),
            "response": body(self.response, self.sse),
        });
        if self.truncated {
            line["response_truncated"] = Value::Bool(true);
        }
        line
    }
}

fn body(bytes: &[u8], raw: bool) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    if !raw {
        if let Ok(value) = serde_json::from_slice(bytes) {
            return value;
        }
    }
    Value::String(String::from_utf8_lossy(bytes).into_owned())
}

/// Calls of every turn in this process append through one lock, so lines
/// are never interleaved.
static WRITING: Mutex<()> = Mutex::new(());

/// Append `line` and a newline to `path`, creating its directories (0700)
/// and the file (0600).
pub(crate) fn append(path: &Path, line: &Value) -> io::Result<()> {
    let mut text = serde_json::to_vec(line)?;
    text.push(b'\n');
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let _writing = WRITING.lock_recovering("model audit file");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::Scratch;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_line_holds_the_request_and_the_response_as_json_or_as_text() {
        let json = Entry {
            at_ms: 7,
            model: "claude-sonnet-4-6",
            backend: "anthropic",
            request:
                br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}"#,
            response: br#"{"content":[{"type":"text","text":"hello"}]}"#,
            sse: false,
            truncated: false,
        }
        .line();
        assert_eq!(
            json,
            serde_json::json!({
                "at_ms": 7, "model": "claude-sonnet-4-6", "backend": "anthropic",
                "request": {"model": "claude-sonnet-4-6",
                    "messages": [{"role": "user", "content": "hi"}]},
                "response": {"content": [{"type": "text", "text": "hello"}]}
            })
        );
        // A stream is kept as its raw text, not parsed, and a body that is
        // not JSON as its text.
        let stream = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let sse = Entry {
            at_ms: 8,
            model: "m",
            backend: "b",
            request: b"model=m&x=\xff",
            response: stream.as_bytes(),
            sse: true,
            truncated: true,
        }
        .line();
        assert_eq!(sse["response"], stream);
        assert_eq!(sse["request"], "model=m&x=\u{fffd}");
        assert_eq!(sse["response_truncated"], true);
        // No body is null.
        let none = Entry {
            at_ms: 9,
            model: "",
            backend: "b",
            request: b"",
            response: b"",
            sse: false,
            truncated: false,
        }
        .line();
        assert_eq!(none["request"], Value::Null);
        assert_eq!(none["response"], Value::Null);
        assert_eq!(none.get("response_truncated"), None);
    }

    #[test]
    fn lines_are_appended_to_a_private_file_in_a_private_directory() {
        let scratch = Scratch::new("model-audit");
        let file = scratch
            .path()
            .join(".branchyard/models/audit/by-branch/2.jsonl");
        append(&file, &serde_json::json!({"n": 1, "text": "a\nb"})).unwrap();
        append(&file, &serde_json::json!({"n": 2})).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        assert_eq!(lines[0]["text"], "a\nb");
        assert_eq!(lines[1]["n"], 2);
        assert_eq!(mode(&file), 0o600);
        for dir in ["models/audit/by-branch", "models/audit", "models"] {
            let dir = scratch.path().join(".branchyard").join(dir);
            assert_eq!(mode(&dir), 0o700, "{}", dir.display());
        }
    }
}
