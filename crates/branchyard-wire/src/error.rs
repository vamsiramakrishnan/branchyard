//! What can be wrong on the wire, as a value a test can match on.

use std::fmt;
use std::io;

/// A malformed or unacceptable message. Every variant is a refusal to
/// guess: none of them is ever turned into a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The peer closed before sending a byte of the head.
    ConnectionClosed,
    /// The peer closed in the middle of a head.
    TruncatedHead,
    /// The head is longer than the limit the caller set.
    HeadTooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// More header fields than the codec holds.
    TooManyHeaders,
    /// The head does not parse; the text says where.
    MalformedHead(String),
    /// A request line without a method.
    MissingMethod,
    /// A request line without a target.
    MissingTarget,
    /// A status line without a status code.
    MissingStatus,
    /// `Content-Length` is not a plain decimal number that fits `u64`.
    BadContentLength(String),
    /// More than one `Content-Length` field.
    DuplicateContentLength,
    /// `Content-Length` and `Transfer-Encoding` together, the classic
    /// request-smuggling shape.
    ConflictingLength,
    /// A `Transfer-Encoding` that cannot be framed: `chunked` not last, or
    /// a request whose last coding is not `chunked`.
    UnsupportedTransferEncoding(String),
    /// A chunk-size line that is not hexadecimal digits (then optional
    /// extensions), is empty, overflows `u64`, or is too long.
    BadChunkSize(String),
    /// A chunk larger than the cap.
    ChunkTooLarge {
        /// The chunk's declared size.
        size: u64,
        /// The cap.
        limit: u64,
    },
    /// Chunk data not followed by CRLF.
    BadChunkTerminator,
    /// A trailer line that is not `name: value`, or trailers over the cap.
    BadTrailer(String),
    /// The body ended before its framing said it would.
    TruncatedBody(&'static str),
    /// A body longer than the caller's limit.
    BodyTooLarge {
        /// The limit, in bytes.
        limit: u64,
    },
    /// A header name or value that cannot be written to the wire.
    BadHeader(String),
    /// A request method or target that cannot be written.
    BadRequestLine(String),
    /// Not an http(s) URL this codec accepts.
    BadUrl(String),
    /// A transport error under the codec.
    Io {
        /// What kind of I/O error it was.
        kind: io::ErrorKind,
        /// The I/O error's message.
        message: String,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use WireError::*;
        match self {
            ConnectionClosed => write!(f, "the connection closed before a message arrived"),
            TruncatedHead => write!(f, "the message ended in its head"),
            HeadTooLarge { limit } => write!(f, "the head is larger than {limit} bytes"),
            TooManyHeaders => write!(f, "the head has too many header fields"),
            MalformedHead(why) => write!(f, "the head is malformed: {why}"),
            MissingMethod => write!(f, "the request line has no method"),
            MissingTarget => write!(f, "the request line has no target"),
            MissingStatus => write!(f, "the status line has no status code"),
            BadContentLength(value) => write!(f, "bad Content-Length {value:?}"),
            DuplicateContentLength => write!(f, "more than one Content-Length"),
            ConflictingLength => write!(f, "both Content-Length and Transfer-Encoding"),
            UnsupportedTransferEncoding(value) => {
                write!(f, "unsupported Transfer-Encoding {value:?}")
            }
            BadChunkSize(line) => write!(f, "bad chunk size {line:?}"),
            ChunkTooLarge { size, limit } => {
                write!(f, "a chunk of {size} bytes exceeds the {limit} byte cap")
            }
            BadChunkTerminator => write!(f, "chunk data is not followed by CRLF"),
            BadTrailer(why) => write!(f, "bad trailer: {why}"),
            TruncatedBody(what) => write!(f, "the body ended early: {what}"),
            BodyTooLarge { limit } => write!(f, "the body is larger than {limit} bytes"),
            BadHeader(why) => write!(f, "bad header: {why}"),
            BadRequestLine(why) => write!(f, "bad request line: {why}"),
            BadUrl(why) => write!(f, "{why}"),
            Io { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(error: io::Error) -> Self {
        // A wire error that crossed an `io::Read` comes back as itself.
        if let Some(inner) = error.get_ref().and_then(|e| e.downcast_ref::<WireError>()) {
            return inner.clone();
        }
        WireError::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<WireError> for io::Error {
    fn from(error: WireError) -> Self {
        let kind = match &error {
            WireError::Io { kind, message } => return io::Error::new(*kind, message.clone()),
            WireError::ConnectionClosed
            | WireError::TruncatedHead
            | WireError::TruncatedBody(_) => io::ErrorKind::UnexpectedEof,
            _ => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, error)
    }
}

/// The wire error an `io::Error` carries, if it came out of this crate.
pub fn wire_error(error: &io::Error) -> Option<&WireError> {
    error.get_ref().and_then(|e| e.downcast_ref::<WireError>())
}
