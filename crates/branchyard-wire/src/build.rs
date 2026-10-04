//! Writing a request head, validating what goes into it.

use crate::body::is_token_byte;
use crate::error::WireError;

/// Build `METHOD target HTTP/1.1` and its header fields, ending with the
/// blank line. `headers` are written in order, so the caller puts `Host`
/// and `Connection` among them; `content_length` adds a `Content-Length`.
///
/// Refused rather than written: a method that is not a token, a target
/// with whitespace or control bytes, a header name that is not a token,
/// a value with a CR, LF or NUL, and a caller-supplied `Content-Length`
/// or `Transfer-Encoding` (the body's framing is the codec's to say).
pub fn request_head<'a, I>(
    method: &str,
    target: &str,
    headers: I,
    content_length: Option<u64>,
) -> Result<Vec<u8>, WireError>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    if method.is_empty() || !method.bytes().all(|b| is_token_byte(&b)) {
        return Err(WireError::BadRequestLine(format!("method {method:?}")));
    }
    if target.is_empty() || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(WireError::BadRequestLine(format!("target {target:?}")));
    }
    let mut head = format!("{method} {target} HTTP/1.1\r\n");
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(|b| is_token_byte(&b)) {
            return Err(WireError::BadHeader(format!("name {name:?}")));
        }
        if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(WireError::BadHeader(format!(
                "the value of {name} has a line break"
            )));
        }
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err(WireError::BadHeader(format!("{name} is set by the codec")));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(length) = content_length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("\r\n");
    Ok(head.into_bytes())
}
