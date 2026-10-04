//! Message bodies: the chunked transfer coding (the only decoder in the
//! workspace), `Content-Length` bodies that must be complete, and one
//! `Body` reader over any framing.

use std::io::{self, BufRead, Read, Write};

use crate::error::WireError;
use crate::head::Framing;

/// The longest chunk-size line (digits, extensions and CRLF) accepted.
const MAX_SIZE_LINE: usize = 4096;
/// The longest single trailer line, and all trailers together.
const MAX_TRAILER_LINE: usize = 8 * 1024;
const MAX_TRAILERS: usize = 64 * 1024;
/// The default cap on one chunk: far above any real one, below what
/// would overflow a 64-bit length arithmetic.
pub const MAX_CHUNK: u64 = 1 << 40;

/// Read one line, at most `max` bytes with its terminator, into `line`.
/// Returns how many bytes it took; the line is complete only if it ends
/// in `\n`.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    max: usize,
    line: &mut Vec<u8>,
) -> io::Result<usize> {
    line.clear();
    reader.by_ref().take(max as u64 + 1).read_until(b'\n', line)
}

/// Parse a chunk-size line (without its CRLF): hexadecimal digits, then
/// optionally `;extensions`. Nothing else is a size.
pub fn parse_chunk_size(line: &[u8]) -> Result<u64, WireError> {
    let bad = || WireError::BadChunkSize(String::from_utf8_lossy(line).into_owned());
    // Extensions are allowed and ignored, but may not hide a line break
    // or other control bytes.
    if line
        .iter()
        .any(|b| (*b < b' ' && *b != b'\t') || *b == 0x7f)
    {
        return Err(bad());
    }
    let size = match line.iter().position(|b| *b == b';') {
        Some(i) => &line[..i],
        None => line,
    };
    // Whitespace may pad the size only on the extension side.
    let size = match size.iter().rposition(|b| *b != b' ' && *b != b'\t') {
        Some(end) => &size[..=end],
        None => return Err(bad()),
    };
    if !size.iter().all(u8::is_ascii_hexdigit) {
        return Err(bad());
    }
    let text = std::str::from_utf8(size).map_err(|_| bad())?;
    u64::from_str_radix(text, 16).map_err(|_| bad())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Expecting a chunk-size line.
    Size,
    /// This many bytes of the current chunk are left.
    Data(u64),
    /// The current chunk's data is read; its CRLF is next.
    Terminator,
    Done,
    Failed,
}

/// Decodes `Transfer-Encoding: chunked`.
///
/// Strict: a chunk size is hex digits only (extensions after `;` are
/// ignored), every line ends CRLF, a chunk's data is followed by CRLF,
/// trailers are `name: value` lines within a cap and end with the blank
/// line, and the stream ending anywhere before that is an error.
pub struct ChunkedReader<R> {
    reader: R,
    state: State,
    max_chunk: u64,
    /// Bytes decoded so far, and the most to allow.
    total: u64,
    limit: u64,
}

impl<R: BufRead> ChunkedReader<R> {
    pub fn new(reader: R) -> Self {
        ChunkedReader {
            reader,
            state: State::Size,
            max_chunk: MAX_CHUNK,
            total: 0,
            limit: u64::MAX,
        }
    }

    /// Refuse a chunk larger than `max`.
    pub fn max_chunk(mut self, max: u64) -> Self {
        self.max_chunk = max;
        self
    }

    /// Refuse a body longer than `limit` bytes.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }

    fn fail(&mut self, error: WireError) -> io::Error {
        self.state = State::Failed;
        error.into()
    }

    fn size_line(&mut self) -> io::Result<u64> {
        let mut line = Vec::new();
        let n = read_line_bounded(&mut self.reader, MAX_SIZE_LINE, &mut line)?;
        if n == 0 {
            return Err(self.fail(WireError::TruncatedBody("no chunk-size line")));
        }
        let Some(text) = line.strip_suffix(b"\r\n") else {
            return Err(self.fail(match line.last() {
                Some(b'\n') | None => WireError::BadChunkSize(lossy(&line)),
                Some(_) if line.len() > MAX_SIZE_LINE => WireError::BadChunkSize(lossy(&line)),
                Some(_) => WireError::TruncatedBody("the chunk-size line"),
            }));
        };
        let size = parse_chunk_size(text).map_err(|e| self.fail(e))?;
        if size > self.max_chunk {
            let limit = self.max_chunk;
            return Err(self.fail(WireError::ChunkTooLarge { size, limit }));
        }
        Ok(size)
    }

    /// After the zero chunk: trailer lines up to the blank one.
    fn trailers(&mut self) -> io::Result<()> {
        let mut line = Vec::new();
        let mut total = 0;
        loop {
            let n = read_line_bounded(&mut self.reader, MAX_TRAILER_LINE, &mut line)?;
            if n == 0 {
                return Err(self.fail(WireError::TruncatedBody("no final CRLF after the trailers")));
            }
            let Some(text) = line.strip_suffix(b"\r\n") else {
                return Err(self.fail(match line.last() {
                    Some(b'\n') => WireError::BadTrailer("a line ends without CR".into()),
                    _ if line.len() > MAX_TRAILER_LINE => {
                        WireError::BadTrailer("a line is too long".into())
                    }
                    _ => WireError::TruncatedBody("a trailer line"),
                }));
            };
            if text.is_empty() {
                return Ok(());
            }
            total += n;
            if total > MAX_TRAILERS {
                return Err(self.fail(WireError::BadTrailer("the trailers are too large".into())));
            }
            let named = text
                .iter()
                .position(|b| *b == b':')
                .is_some_and(|i| i > 0 && text[..i].iter().all(is_token_byte));
            if !named {
                return Err(self.fail(WireError::BadTrailer(lossy(text))));
            }
        }
    }

    /// Consume the CRLF that ends a chunk's data.
    fn terminator(&mut self) -> io::Result<()> {
        let mut crlf = [0u8; 2];
        match self.reader.read_exact(&mut crlf) {
            Ok(()) if &crlf == b"\r\n" => Ok(()),
            Ok(()) => Err(self.fail(WireError::BadChunkTerminator)),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                Err(self.fail(WireError::TruncatedBody("no CRLF after chunk data")))
            }
            Err(e) => Err(e),
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A byte allowed in an HTTP token (a header or method name).
pub fn is_token_byte(b: &u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b)
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state {
                State::Done => return Ok(0),
                State::Failed => {
                    return Err(
                        WireError::MalformedHead("the chunked body already failed".into()).into(),
                    )
                }
                _ if buf.is_empty() => return Ok(0),
                State::Terminator => {
                    self.terminator()?;
                    self.state = State::Size;
                }
                State::Size => match self.size_line()? {
                    0 => {
                        self.trailers()?;
                        self.state = State::Done;
                    }
                    size => {
                        if self.total.saturating_add(size) > self.limit {
                            let limit = self.limit;
                            return Err(self.fail(WireError::BodyTooLarge { limit }));
                        }
                        self.state = State::Data(size);
                    }
                },
                State::Data(left) => {
                    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.reader.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(self.fail(WireError::TruncatedBody("inside a chunk")));
                    }
                    self.total += n as u64;
                    self.state = match left - n as u64 {
                        0 => State::Terminator,
                        left => State::Data(left),
                    };
                    return Ok(n);
                }
            }
        }
    }
}

/// A `Content-Length` body: exactly `length` bytes, and the stream
/// ending before them is an error rather than a short body.
pub struct LengthReader<R> {
    reader: R,
    left: u64,
}

impl<R: Read> LengthReader<R> {
    pub fn new(reader: R, length: u64) -> Self {
        LengthReader {
            reader,
            left: length,
        }
    }
}

impl<R: Read> Read for LengthReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.reader.read(&mut buf[..want])?;
        if n == 0 {
            return Err(WireError::TruncatedBody("fewer bytes than Content-Length").into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// A message body with its framing undone.
pub enum Body<R> {
    Empty,
    Length(LengthReader<R>),
    Chunked(ChunkedReader<R>),
    /// Until the connection closes.
    Eof(R),
}

impl<R: BufRead> Body<R> {
    pub fn new(reader: R, framing: Framing) -> Self {
        match framing {
            Framing::None => Body::Empty,
            Framing::Length(length) => Body::Length(LengthReader::new(reader, length)),
            Framing::Chunked => Body::Chunked(ChunkedReader::new(reader)),
            Framing::UntilClose => Body::Eof(reader),
        }
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Empty => Ok(0),
            Body::Length(r) => r.read(buf),
            Body::Chunked(r) => r.read(buf),
            Body::Eof(r) => r.read(buf),
        }
    }
}

/// Read a whole body of `framing`, refusing one over `limit` bytes. A body
/// that declares a length over the limit is refused before it is read.
pub fn read_body<R: BufRead>(
    reader: R,
    framing: Framing,
    limit: usize,
) -> Result<Vec<u8>, WireError> {
    let limit = limit as u64;
    let too_large = WireError::BodyTooLarge { limit };
    if matches!(framing, Framing::Length(n) if n > limit) {
        return Err(too_large);
    }
    let mut body = match framing {
        Framing::Chunked => Body::Chunked(ChunkedReader::new(reader).limit(limit)),
        other => Body::new(reader, other),
    };
    let mut out = Vec::new();
    (&mut body).take(limit + 1).read_to_end(&mut out)?;
    match out.len() as u64 > limit {
        true => Err(too_large),
        false => Ok(out),
    }
}

/// Write `bytes` as one chunk; nothing for an empty slice, which would end
/// the body.
pub fn write_chunk<W: Write>(out: &mut W, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    out.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    out.write_all(bytes)?;
    out.write_all(b"\r\n")?;
    out.flush()
}

/// The chunk that ends a chunked body.
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";
