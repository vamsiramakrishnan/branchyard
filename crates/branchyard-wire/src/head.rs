//! Message heads: read up to the blank line, parse with `httparse`, and
//! work out how the body is framed (RFC 9112 section 6.3).

use std::io::{BufRead, Read};

use crate::error::WireError;

/// How many header fields a head may have.
const MAX_FIELDS: usize = 128;

/// Header fields in wire order, names as sent.
pub type Headers = Vec<(String, String)>;

/// The first field named `name` (case-insensitively).
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    /// The minor version: 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub minor: u8,
    pub headers: Headers,
}

/// A parsed response head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub reason: String,
    pub minor: u8,
    pub headers: Headers,
}

/// How a message body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// No body.
    None,
    /// Exactly this many bytes.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// To the end of the connection (responses only).
    UntilClose,
}

/// Read one head: every line up to and including the blank one, at most
/// `max` bytes. `None` when the peer closed before sending anything.
pub fn read_head<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>, WireError> {
    let mut raw = Vec::new();
    loop {
        let before = raw.len();
        // Bounded, so one endless line cannot grow the buffer past `max`.
        let room = (max + 1).saturating_sub(raw.len()) as u64;
        let n = reader.by_ref().take(room).read_until(b'\n', &mut raw)?;
        if n == 0 {
            return match raw.is_empty() {
                true => Ok(None),
                false => Err(WireError::TruncatedHead),
            };
        }
        if raw.len() > max {
            return Err(WireError::HeadTooLarge { limit: max });
        }
        let line = &raw[before..];
        if line == b"\r\n" || line == b"\n" {
            return Ok(Some(raw));
        }
        if line.last() != Some(&b'\n') {
            // The bounded read stopped mid-line: the head ended there.
            return Err(WireError::TruncatedHead);
        }
    }
}

fn fields(parsed: &[httparse::Header<'_>]) -> Headers {
    parsed
        .iter()
        .map(|h| {
            (
                h.name.to_owned(),
                String::from_utf8_lossy(h.value).trim().to_owned(),
            )
        })
        .collect()
}

fn head_error(error: httparse::Error) -> WireError {
    match error {
        httparse::Error::TooManyHeaders => WireError::TooManyHeaders,
        other => WireError::MalformedHead(other.to_string()),
    }
}

/// Parse the bytes `read_head` returned as a request head.
pub fn parse_request_head(raw: &[u8]) -> Result<RequestHead, WireError> {
    let mut slots = [httparse::EMPTY_HEADER; MAX_FIELDS];
    let mut parsed = httparse::Request::new(&mut slots);
    match parsed.parse(raw).map_err(head_error)? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(WireError::TruncatedHead),
    }
    Ok(RequestHead {
        method: parsed.method.ok_or(WireError::MissingMethod)?.to_owned(),
        target: parsed.path.ok_or(WireError::MissingTarget)?.to_owned(),
        minor: parsed.version.unwrap_or(1),
        headers: fields(parsed.headers),
    })
}

/// Parse the bytes `read_head` returned as a response head.
pub fn parse_response_head(raw: &[u8]) -> Result<ResponseHead, WireError> {
    let mut slots = [httparse::EMPTY_HEADER; MAX_FIELDS];
    let mut parsed = httparse::Response::new(&mut slots);
    match parsed.parse(raw).map_err(head_error)? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(WireError::TruncatedHead),
    }
    Ok(ResponseHead {
        status: parsed.code.ok_or(WireError::MissingStatus)?,
        reason: parsed.reason.unwrap_or("").to_owned(),
        minor: parsed.version.unwrap_or(1),
        headers: fields(parsed.headers),
    })
}

/// Read and parse a request head; `None` when the peer closed first.
pub fn read_request_head<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> Result<Option<RequestHead>, WireError> {
    match read_head(reader, max)? {
        Some(raw) => parse_request_head(&raw).map(Some),
        None => Ok(None),
    }
}

/// Read and parse a response head, passing over interim `1xx` responses
/// (`100 Continue`) except `101 Switching Protocols`, which is final.
pub fn read_response_head<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> Result<ResponseHead, WireError> {
    loop {
        let raw = read_head(reader, max)?.ok_or(WireError::ConnectionClosed)?;
        let head = parse_response_head(&raw)?;
        if (100..200).contains(&head.status) && head.status != 101 {
            continue;
        }
        return Ok(head);
    }
}

/// The codings of every `Transfer-Encoding` field, in order.
fn codings(headers: &[(String, String)]) -> Option<Vec<String>> {
    let mut found = false;
    let mut out = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("transfer-encoding") {
            found = true;
            out.extend(
                value
                    .split(',')
                    .map(|c| c.trim().to_ascii_lowercase())
                    .filter(|c| !c.is_empty()),
            );
        }
    }
    found.then_some(out)
}

/// The one `Content-Length`, if there is one: plain decimal digits that fit
/// `u64`, in a single field.
fn content_length(headers: &[(String, String)]) -> Result<Option<u64>, WireError> {
    let mut values = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.as_str());
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(WireError::DuplicateContentLength);
    }
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(WireError::BadContentLength(value.to_owned()));
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| WireError::BadContentLength(value.to_owned()))
}

/// `chunked` must be the last coding, and appear once.
fn chunked_last(codings: &[String]) -> Result<bool, WireError> {
    let last = codings.last().map(String::as_str) == Some("chunked");
    let count = codings.iter().filter(|c| *c == "chunked").count();
    match (last, count) {
        (true, 1) | (false, 0) => Ok(last),
        _ => Err(WireError::UnsupportedTransferEncoding(codings.join(", "))),
    }
}

/// How a request's body is framed. A request with both `Content-Length`
/// and `Transfer-Encoding` is refused, as is one whose transfer coding does
/// not end in `chunked`: its length cannot be known.
pub fn request_framing(headers: &[(String, String)]) -> Result<Framing, WireError> {
    let length = content_length(headers)?;
    match codings(headers) {
        Some(_) if length.is_some() => Err(WireError::ConflictingLength),
        Some(list) => match chunked_last(&list)? {
            true => Ok(Framing::Chunked),
            false => Err(WireError::UnsupportedTransferEncoding(list.join(", "))),
        },
        None => Ok(length.map_or(Framing::None, Framing::Length)),
    }
}

/// How a response's body is framed (RFC 9112 section 6.3): none for `HEAD`
/// answers and for `1xx`, `204` and `304`; otherwise `chunked` if it is the
/// last transfer coding (which overrides a stray `Content-Length`), else
/// `Content-Length`, else until the connection closes.
pub fn response_framing(
    status: u16,
    headers: &[(String, String)],
    head_only: bool,
) -> Result<Framing, WireError> {
    if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Framing::None);
    }
    if let Some(list) = codings(headers) {
        return match chunked_last(&list)? {
            true => Ok(Framing::Chunked),
            false => Ok(Framing::UntilClose),
        };
    }
    Ok(content_length(headers)?.map_or(Framing::UntilClose, Framing::Length))
}
