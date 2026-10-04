//! Inbound email as a trigger source, through the inbound-mail webhooks of
//! Postmark, Mailgun and SendGrid; nothing polls a mailbox. Each provider
//! receives the mail, parses it, and posts it to the trigger's URL:
//!
//! | Source | How a delivery is authenticated | Replay window | Body |
//! |---|---|---|---|
//! | Postmark | HTTP Basic, the trigger's secret as the password (any user), or the secret as the URL's last segment: Postmark signs nothing | none | JSON |
//! | Mailgun | HMAC-SHA256 with the webhook signing key: over `timestamp` and `token` (a form), or over `X-Mailgun-Timestamp` and the body (JSON, to a URL ending in `json`) | the signed timestamp; a form's token is spent once | form (`multipart/form-data` with attachments) or JSON |
//! | SendGrid | as Postmark: Inbound Parse signs nothing by default | none | `multipart/form-data` |
//!
//! A delivery is read into one [`EmailMessage`]: addresses lowercased,
//! the plain text body (or the HTML body with its markup removed) cut at
//! 64 KiB, and the attachments' names, types and sizes, never their
//! contents. Its event ID is the `Message-ID` header, so a provider's
//! retry of one message is one event; without one, a hash of the sender,
//! subject, date and text. The provider's SPF and DKIM verdicts, where it
//! gives them, become the labels `spf:<verdict>` and `dkim:<verdict>`, and
//! its spam flag the label `spam`, for conditions.
//!
//! The From address is what the message says. A provider's signature or a
//! password proves the provider delivered it, not who wrote it: an
//! allowlisted sender's address can be forged unless its domain publishes
//! DMARC and the provider enforces it. `label=dkim:pass` asks for the
//! provider's DKIM verdict on the From domain where it gives one.

use base64::Engine;
use branchyard_client::triggers::{EmailAttachment, EmailMessage, EventSource, TriggerEvent};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::events::{form_decode, sign, Delivery, Refused};

/// The event kind of every email.
pub const KIND: &str = "email.received";
/// How much of the text body is kept.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

fn refused(code: &'static str, message: impl Into<String>) -> Refused {
    Refused {
        code,
        message: message.into(),
    }
}

/// A verified delivery, and the one-time token it spent (Mailgun's form
/// `token`), which the caller records so a replay of the token with
/// another body is refused.
#[derive(Debug)]
pub struct Received {
    pub delivery: Delivery,
    pub nonce: Option<String>,
}

/// A file part of a multipart body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePart {
    pub field: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
}

/// Constant-time equality of two secrets: their MACs under one key,
/// compared by `verify_slice`, so neither length nor content leaks.
#[allow(clippy::expect_used)] // ratchet: branchyard-server
fn same(a: &str, b: &str) -> bool {
    use hmac::{Hmac, KeyInit, Mac};
    let mac = |text: &str| {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(b"branchyard email secret").expect("any key length");
        mac.update(text.as_bytes());
        mac
    };
    let expected = mac(a).finalize().into_bytes();
    mac(b).verify_slice(&expected).is_ok()
}

/// The password of an `Authorization: Basic` header.
fn basic_password(header: &str) -> Option<String> {
    let encoded = header.trim().strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    text.split_once(':')
        .map(|(_, password)| password.to_owned())
}

/// Verify a delivery of email `source` and read it. `path_token` is the
/// URL's segment after `/fire/`, if any (`json` for Mailgun's JSON form).
pub fn receive(
    source: EventSource,
    header: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
    secret: &str,
    path_token: Option<&str>,
    now_ms: u64,
    window_seconds: u64,
) -> Result<Received, Refused> {
    let content_type = header("content-type").unwrap_or_default();
    let fresh = |seconds: &str| -> Result<(), Refused> {
        let at: u64 = seconds.trim().parse().map_err(|_| {
            refused(
                "invalid_signature",
                "the delivery's timestamp is not a number",
            )
        })?;
        let age = now_ms.abs_diff(at.saturating_mul(1000));
        match age <= window_seconds.saturating_mul(1000) {
            true => Ok(()),
            false => Err(refused(
                "stale_delivery",
                format!(
                    "the delivery's timestamp is {}s from this server's clock, outside the \
                     {window_seconds}s replay window",
                    age / 1000
                ),
            )),
        }
    };
    let bad_signature = || {
        refused(
            "invalid_signature",
            "the delivery's signature does not match this trigger's signing key",
        )
    };
    match source {
        EventSource::Postmark | EventSource::Sendgrid => {
            let by_path = path_token.map(|t| same(t, secret));
            let by_password = header("authorization")
                .and_then(|h| basic_password(&h))
                .map(|p| same(&p, secret));
            if !(by_path == Some(true) || (by_path.is_none() && by_password == Some(true))) {
                return Err(refused(
                    "invalid_signature",
                    format!(
                        "{} signs nothing: a delivery needs HTTP Basic authentication with the \
                         trigger's secret as the password, or the secret as the webhook URL's \
                         last segment",
                        source.as_str()
                    ),
                ));
            }
            let (fields, files) = match source {
                EventSource::Postmark => (json_object(body)?, Vec::new()),
                _ => form(&content_type, body)?,
            };
            let delivery =
                read(source, &fields, &files).map_err(|e| refused("invalid_request", e))?;
            Ok(Received {
                delivery,
                nonce: None,
            })
        }
        EventSource::Mailgun => {
            if path_token.is_some_and(|t| t != "json") {
                return Err(refused(
                    "invalid_signature",
                    "Mailgun signs its deliveries: post to the trigger's webhook URL, or to it \
                     with /json for Mailgun's JSON form",
                ));
            }
            if content_type.trim_start().starts_with("application/json") {
                let signature = header("x-mailgun-signature").ok_or_else(bad_signature)?;
                let timestamp = header("x-mailgun-timestamp").ok_or_else(bad_signature)?;
                let expected = sign(secret, &[timestamp.trim().as_bytes(), body]);
                if !same(&expected, signature.trim()) {
                    return Err(bad_signature());
                }
                fresh(&timestamp)?;
                let fields = json_object(body)?;
                let delivery =
                    read(source, &fields, &[]).map_err(|e| refused("invalid_request", e))?;
                return Ok(Received {
                    delivery,
                    nonce: None,
                });
            }
            let (fields, files) = form(&content_type, body)?;
            let field = |name: &str| fields.get(name).and_then(Value::as_str).unwrap_or("");
            let (timestamp, token, signature) =
                (field("timestamp"), field("token"), field("signature"));
            if timestamp.is_empty() || token.is_empty() || signature.is_empty() {
                return Err(bad_signature());
            }
            let expected = sign(secret, &[timestamp.as_bytes(), token.as_bytes()]);
            if !same(&expected, signature.trim()) {
                return Err(bad_signature());
            }
            fresh(timestamp)?;
            let nonce = Some(token.to_owned());
            let delivery =
                read(source, &fields, &files).map_err(|e| refused("invalid_request", e))?;
            Ok(Received { delivery, nonce })
        }
        other => Err(refused(
            "invalid_request",
            format!("{} is not an email source", other.as_str()),
        )),
    }
}

fn json_object(body: &[u8]) -> Result<Map<String, Value>, Refused> {
    match serde_json::from_slice(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(refused("invalid_request", "the body is not a JSON object")),
        Err(e) => Err(refused(
            "invalid_request",
            format!("the body is not JSON: {e}"),
        )),
    }
}

/// A form body's text fields and file parts.
fn form(content_type: &str, body: &[u8]) -> Result<(Map<String, Value>, Vec<FilePart>), Refused> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "multipart/form-data" => {
            let boundary = parameter(content_type, "boundary").ok_or_else(|| {
                refused("invalid_request", "multipart/form-data without a boundary")
            })?;
            multipart(body, &boundary).map_err(|e| refused("invalid_request", e))
        }
        "application/x-www-form-urlencoded" | "" => Ok((urlencoded(body), Vec::new())),
        other => Err(refused(
            "invalid_request",
            format!("a form was expected, not {other}"),
        )),
    }
}

/// `name=value` of a header's parameters, unquoted.
fn parameter(header: &str, name: &str) -> Option<String> {
    header.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim().eq_ignore_ascii_case(name)).then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// `application/x-www-form-urlencoded` fields; the first of a repeated name.
pub fn urlencoded(body: &[u8]) -> Map<String, Value> {
    let mut map = Map::new();
    for pair in body.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
        let (name, value) = match pair.iter().position(|b| *b == b'=') {
            Some(at) => (&pair[..at], &pair[at + 1..]),
            None => (pair, &[][..]),
        };
        let name = String::from_utf8_lossy(&form_decode(name)).into_owned();
        let value = String::from_utf8_lossy(&form_decode(value)).into_owned();
        map.entry(name).or_insert(Value::String(value));
    }
    map
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| at + from)
}

/// A `multipart/form-data` body: text fields (the first of a repeated
/// name) and file parts, whose contents are measured and dropped.
pub fn multipart(
    body: &[u8],
    boundary: &str,
) -> Result<(Map<String, Value>, Vec<FilePart>), String> {
    if boundary.is_empty() || boundary.len() > 200 {
        return Err("the multipart boundary is empty or too long".into());
    }
    let delimiter = format!("--{boundary}");
    let mut at = find(body, delimiter.as_bytes(), 0).ok_or("no multipart boundary in the body")?;
    let (mut fields, mut files) = (Map::new(), Vec::new());
    loop {
        at += delimiter.len();
        if body[at..].starts_with(b"--") {
            break;
        }
        // The rest of the boundary line.
        let line_end = find(body, b"\r\n", at).ok_or("a truncated multipart body")?;
        let start = line_end + 2;
        let headers_end =
            find(body, b"\r\n\r\n", start).ok_or("a multipart part without headers")?;
        let headers = String::from_utf8_lossy(&body[start..headers_end]).into_owned();
        let content_start = headers_end + 4;
        let next = find(body, format!("\r\n{delimiter}").as_bytes(), content_start)
            .ok_or("a multipart part without its closing boundary")?;
        let content = &body[content_start..next];
        let (mut disposition, mut content_type) = (String::new(), String::new());
        for line in headers.split("\r\n") {
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "content-disposition" => disposition = value.trim().to_owned(),
                    "content-type" => content_type = value.trim().to_owned(),
                    _ => {}
                }
            }
        }
        let name = parameter(&disposition, "name").unwrap_or_default();
        match parameter(&disposition, "filename") {
            Some(filename) => files.push(FilePart {
                field: name,
                filename,
                content_type,
                size: content.len() as u64,
            }),
            None if !name.is_empty() => {
                fields.entry(name).or_insert_with(|| {
                    Value::String(String::from_utf8_lossy(content).into_owned())
                });
            }
            None => {}
        }
        at = next + 2;
    }
    Ok((fields, files))
}

/// Read a test event: a JSON object as the source would post it (for a
/// form, its fields), without attachments' parts.
pub fn read_value(source: EventSource, payload: &Value) -> Result<Delivery, String> {
    let fields = payload
        .as_object()
        .ok_or("the event is not a JSON object")?;
    read(source, fields, &[])
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Headers as (lowercase name, value) pairs from Postmark's `Headers`,
/// Mailgun's `message-headers` or SendGrid's raw `headers`.
fn headers_of(source: EventSource, fields: &Map<String, Value>) -> Vec<(String, String)> {
    let pairs = |list: &Value| -> Vec<(String, String)> {
        list.as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Array(pair) if pair.len() == 2 => Some((
                            text_of(pair.first()).to_ascii_lowercase(),
                            text_of(pair.get(1)),
                        )),
                        Value::Object(o) => Some((
                            text_of(o.get("Name")).to_ascii_lowercase(),
                            text_of(o.get("Value")),
                        )),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    match source {
        EventSource::Postmark => fields.get("Headers").map(pairs).unwrap_or_default(),
        EventSource::Mailgun => match fields.get("message-headers") {
            Some(Value::String(text)) => serde_json::from_str::<Value>(text)
                .map(|v| pairs(&v))
                .unwrap_or_default(),
            Some(list) => pairs(list),
            None => Vec::new(),
        },
        _ => raw_headers(&text_of(fields.get("headers"))),
    }
}

/// A raw header block's fields, unfolded.
fn raw_headers(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = out.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            out.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    out
}

/// Split an address list on commas outside quotes and angle brackets.
fn split_list(text: &str) -> Vec<&str> {
    let (mut out, mut start, mut quoted, mut angle) = (Vec::new(), 0, false, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// The address of `Name <addr>` or `addr`, lowercased, if it is one.
pub fn address(text: &str) -> Option<String> {
    let text = text.trim();
    let inner = match (text.rfind('<'), text.rfind('>')) {
        (Some(open), Some(close)) if open < close => &text[open + 1..close],
        _ => text,
    };
    let inner = inner.trim().to_ascii_lowercase();
    let (local, domain) = inner.rsplit_once('@')?;
    let ok = !local.is_empty()
        && !domain.is_empty()
        && !inner
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',');
    ok.then_some(inner)
}

fn display_name(text: &str) -> Option<String> {
    let open = text.rfind('<')?;
    let name = text[..open].trim().trim_matches('"').trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn addresses(text: &str) -> Vec<String> {
    split_list(text).into_iter().filter_map(address).collect()
}

/// `Message-ID` without its angle brackets; a long or odd one is hashed.
fn message_id(text: &str) -> Option<String> {
    let id = text
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if id.is_empty() {
        return None;
    }
    let plain = id.len() <= 200 && id.chars().all(|c| c.is_ascii_graphic());
    Some(match plain {
        true => id.to_owned(),
        false => format!(
            "sha256:{}",
            &hex::encode(Sha256::digest(id.as_bytes()))[..32]
        ),
    })
}

/// The first word of a verdict such as `Pass (mail.example: ...)`,
/// lowercased.
fn verdict(text: &str) -> Option<String> {
    let word: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!word.is_empty()).then(|| word.to_ascii_lowercase())
}

/// The text of an HTML body: markup removed (and what `script`, `style`
/// and `head` hold, and comments), block ends as line breaks, character
/// references decoded, blank lines dropped. The result is plain text,
/// never markup.
#[allow(clippy::expect_used)] // ratchet: branchyard-server
pub fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < html.len() {
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |end| end + 3);
            continue;
        }
        if let Some(inside) = rest.strip_prefix('<') {
            let end = rest.find('>').map_or(rest.len(), |end| end + 1);
            let tag: String = rest[1..end.saturating_sub(1).max(1)]
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let closing = inside.starts_with('/');
            if !closing && matches!(tag.as_str(), "script" | "style" | "head" | "title") {
                let close = format!("</{tag}");
                i += rest.find(&close).map_or(rest.len(), |at| {
                    at + rest[at..].find('>').map_or(rest.len() - at, |e| e + 1)
                });
                continue;
            }
            if matches!(
                tag.as_str(),
                "br" | "p"
                    | "div"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "pre"
                    | "table"
                    | "ul"
                    | "ol"
                    | "hr"
            ) {
                out.push('\n');
            }
            i += end;
            continue;
        }
        let c = html[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    let decoded = decode_entities(&out);
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32)
                .filter(|c| !c.is_control() || matches!(c, '\n' | '\t')),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` cut at [`MAX_TEXT_BYTES`] on a character boundary, and whether
/// it was cut.
fn bounded(text: String) -> (String, bool) {
    if text.len() <= MAX_TEXT_BYTES {
        return (text, false);
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// The decoded length of a base64 text.
fn base64_len(text: &str) -> u64 {
    let n = text.bytes().filter(|b| !b.is_ascii_whitespace()).count() as u64;
    let padding = text
        .trim_end()
        .bytes()
        .rev()
        .take_while(|b| *b == b'=')
        .count() as u64;
    (n / 4 * 3).saturating_sub(padding)
}

/// Read a delivery's fields (and file parts) into an event.
#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-server
pub fn read(
    source: EventSource,
    fields: &Map<String, Value>,
    files: &[FilePart],
) -> Result<Delivery, String> {
    let get = |name: &str| text_of(fields.get(name));
    let headers = headers_of(source, fields);
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let mut email = EmailMessage::default();
    let (from, html, plain);
    match source {
        EventSource::Postmark => {
            let full = fields.get("FromFull");
            from = full
                .and_then(|f| f.get("Email"))
                .map(|e| text_of(Some(e)))
                .unwrap_or_else(|| get("From"));
            email.from_name = full
                .and_then(|f| f.get("Name"))
                .map(|n| text_of(Some(n)))
                .filter(|n| !n.is_empty())
                .or_else(|| display_name(&get("From")));
            let list = |full: &str, plain: &str| -> Vec<String> {
                match fields.get(full).and_then(Value::as_array) {
                    Some(items) => items
                        .iter()
                        .filter_map(|i| address(&text_of(i.get("Email"))))
                        .collect(),
                    None => addresses(&get(plain)),
                }
            };
            email.to = list("ToFull", "To");
            email.cc = list("CcFull", "Cc");
            email.envelope_to = addresses(&get("OriginalRecipient"));
            email.subject = get("Subject");
            email.date = Some(get("Date")).filter(|d| !d.is_empty());
            email.message_id = header("message-id").and_then(|m| message_id(&m));
            plain = get("TextBody");
            html = get("HtmlBody");
            email.attachments = fields
                .get("Attachments")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|a| EmailAttachment {
                            name: text_of(a.get("Name")),
                            content_type: text_of(a.get("ContentType")),
                            size: a.get("ContentLength").and_then(Value::as_u64).or_else(|| {
                                a.get("Content").and_then(Value::as_str).map(base64_len)
                            }),
                        })
                        .collect()
                })
                .unwrap_or_default();
            email.spf = header("received-spf").and_then(|v| verdict(&v));
            email.dkim = header("authentication-results").and_then(|v| {
                let at = v.to_ascii_lowercase().find("dkim=")?;
                verdict(&v[at + 5..])
            });
            email.spam = header("x-spam-status")
                .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("yes"));
        }
        EventSource::Mailgun => {
            from = header("from").unwrap_or_else(|| get("from"));
            email.from_name = display_name(&from);
            email.to = addresses(&header("to").unwrap_or_else(|| get("To")));
            email.cc = addresses(&header("cc").unwrap_or_else(|| get("Cc")));
            email.envelope_to = addresses(&get("recipient"));
            email.subject = header("subject").unwrap_or_else(|| get("subject"));
            email.date = header("date")
                .or_else(|| Some(get("Date")))
                .filter(|d| !d.is_empty());
            email.message_id = header("message-id")
                .or_else(|| Some(get("Message-Id")))
                .and_then(|m| message_id(&m));
            plain = get("body-plain");
            html = get("body-html");
            email.attachments = match fields.get("attachments").and_then(Value::as_array) {
                // JSON: the contents are in the body; only measured here.
                Some(items) => items
                    .iter()
                    .map(|a| EmailAttachment {
                        name: text_of(a.get("filename")),
                        content_type: text_of(a.get("content-type")),
                        size: a.get("content").and_then(Value::as_str).map(base64_len),
                    })
                    .collect(),
                None => files
                    .iter()
                    .filter(|f| f.field.starts_with("attachment"))
                    .map(|f| EmailAttachment {
                        name: f.filename.clone(),
                        content_type: f.content_type.clone(),
                        size: Some(f.size),
                    })
                    .collect(),
            };
            let flag = |name: &str| {
                header(name)
                    .or_else(|| Some(get(name)))
                    .filter(|v| !v.is_empty())
            };
            email.spf = flag("x-mailgun-spf").and_then(|v| verdict(&v));
            email.dkim = flag("x-mailgun-dkim-check-result").and_then(|v| verdict(&v));
            email.spam =
                flag("x-mailgun-sflag").is_some_and(|v| v.trim().eq_ignore_ascii_case("yes"));
        }
        EventSource::Sendgrid => {
            from = get("from");
            email.from_name = display_name(&from);
            email.to = addresses(&get("to"));
            email.cc = addresses(&get("cc"));
            let envelope: Value = serde_json::from_str(&get("envelope")).unwrap_or_default();
            email.envelope_to = envelope
                .get("to")
                .and_then(Value::as_array)
                .map(|to| {
                    to.iter()
                        .filter_map(|t| address(&text_of(Some(t))))
                        .collect()
                })
                .unwrap_or_default();
            email.subject = get("subject");
            email.date = header("date");
            email.message_id = header("message-id").and_then(|m| message_id(&m));
            plain = get("text");
            html = get("html");
            let info: Value = serde_json::from_str(&get("attachment-info")).unwrap_or_default();
            email.attachments = files
                .iter()
                .map(|f| {
                    let described = info.get(&f.field);
                    EmailAttachment {
                        name: described
                            .and_then(|d| d.get("filename"))
                            .map(|n| text_of(Some(n)))
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| f.filename.clone()),
                        content_type: described
                            .and_then(|d| d.get("type"))
                            .map(|t| text_of(Some(t)))
                            .filter(|t| !t.is_empty())
                            .unwrap_or_else(|| f.content_type.clone()),
                        size: Some(f.size),
                    }
                })
                .collect();
            if email.attachments.is_empty() {
                // A test event, or no file parts: the descriptions alone.
                if let Some(described) = info.as_object() {
                    email.attachments = described
                        .values()
                        .map(|d| EmailAttachment {
                            name: text_of(d.get("filename")),
                            content_type: text_of(d.get("type")),
                            size: None,
                        })
                        .collect();
                }
            }
            email.spf = verdict(&get("SPF"));
            let domain = address(&from)
                .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_owned()))
                .unwrap_or_default();
            // `{@example.com : pass, @other.example : fail}`
            email.dkim = get("dkim")
                .trim_matches(['{', '}'])
                .split(',')
                .find_map(|entry| {
                    let (d, v) = entry.split_once(':')?;
                    (d.trim()
                        .trim_start_matches('@')
                        .eq_ignore_ascii_case(&domain))
                    .then(|| verdict(v))
                    .flatten()
                });
            email.spam = get("spam_score")
                .trim()
                .parse::<f64>()
                .is_ok_and(|s| s >= 5.0);
        }
        other => return Err(format!("{} is not an email source", other.as_str())),
    }
    email.from = address(&from).ok_or_else(|| {
        format!(
            "the message has no From address one can read ({:?})",
            from.chars().take(80).collect::<String>()
        )
    })?;
    let text = match plain.trim().is_empty() {
        true => strip_html(&html),
        false => plain.replace("\r\n", "\n"),
    };
    (email.text, email.text_truncated) = bounded(text);
    let id = email.message_id.clone().unwrap_or_else(|| {
        let digest = Sha256::digest(
            format!(
                "{}\n{}\n{}\n{}",
                email.from,
                email.subject,
                email.date.as_deref().unwrap_or(""),
                email.text
            )
            .as_bytes(),
        );
        format!("sha256:{}", &hex::encode(digest)[..32])
    });
    let mut labels = Vec::new();
    if let Some(spf) = &email.spf {
        labels.push(format!("spf:{spf}"));
    }
    if let Some(dkim) = &email.dkim {
        labels.push(format!("dkim:{dkim}"));
    }
    if email.spam {
        labels.push("spam".into());
    }
    let delivery = match source {
        EventSource::Postmark => Some(get("MessageID")).filter(|d| !d.is_empty()),
        _ => None,
    };
    let event = TriggerEvent {
        source: source.as_str().into(),
        kind: KIND.into(),
        id,
        delivery,
        author: Some(email.from.clone()),
        title: Some(email.subject.clone()).filter(|s| !s.is_empty()),
        text: Some(email.text.clone()).filter(|t| !t.is_empty()),
        labels,
        payload: serde_json::to_value(&email).unwrap_or_default(),
        email: Some(email),
        ..TriggerEvent::default()
    };
    Ok(Delivery::Event(Box::new(event)))
}

/// Whether `address` is one `allowed` names: an exact address, or a
/// `@domain` (that domain exactly, not its subdomains), ignoring case.
#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-server
pub fn allowed(allowed: &[String], address: &str) -> bool {
    let address = address.to_ascii_lowercase();
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    allowed.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        match entry.strip_prefix('@') {
            Some(d) => d == domain,
            None => entry == address,
        }
    })
}

/// Whether an allowlist entry is an address or a `@domain`.
pub fn valid_entry(entry: &str) -> bool {
    let entry = entry.trim();
    match entry.strip_prefix('@') {
        Some(domain) => {
            domain.contains('.')
                && !domain.starts_with('.')
                && domain
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        }
        None => address(entry).as_deref() == Some(&entry.to_ascii_lowercase()),
    }
}

/// Every recipient an email names: To, Cc and the envelope's.
pub fn recipients(email: &EmailMessage) -> Vec<&str> {
    let mut all: Vec<&str> = email
        .to
        .iter()
        .chain(&email.cc)
        .chain(&email.envelope_to)
        .map(String::as_str)
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const NOW: u64 = 1_790_000_000_000;

    fn headers(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn basic(user: &str, password: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        )
    }

    fn event(received: Received) -> TriggerEvent {
        match received.delivery {
            Delivery::Event(e) => *e,
            other => panic!("not an event: {other:?}"),
        }
    }

    /// A Postmark inbound message, as its documentation shows one.
    fn postmark() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "FromName": "Alice Example",
            "MessageStream": "inbound",
            "From": "alice@example.com",
            "FromFull": {"Email": "Alice@Example.com", "Name": "Alice Example", "MailboxHash": ""},
            "To": "\"Ops\" <ops+deploy@by.example>",
            "ToFull": [{"Email": "ops+deploy@by.example", "Name": "Ops", "MailboxHash": "deploy"}],
            "Cc": "", "CcFull": [],
            "OriginalRecipient": "ops+deploy@by.example",
            "Subject": "Deploy failed on prod",
            "MessageID": "73e6d360-66eb-11e1-8e72-a8904824019b",
            "Date": "Thu, 1 Oct 2026 10:00:00 +0000",
            "TextBody": "The deploy of 4711 failed.\r\nSee the log.",
            "HtmlBody": "<p>The deploy failed</p>",
            "StrippedTextReply": "",
            "Headers": [
                {"Name": "Received-SPF", "Value": "Pass (sender SPF authorized) identity=mailfrom"},
                {"Name": "X-Spam-Status", "Value": "No"},
                {"Name": "Authentication-Results", "Value": "mx.example; dkim=pass header.d=example.com"},
                {"Name": "Message-ID", "Value": "<CAF1234@mail.example.com>"}
            ],
            "Attachments": [
                {"Name": "deploy.log", "Content": "aGVsbG8gd29ybGQ=", "ContentType": "text/plain", "ContentLength": 11}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn postmark_needs_its_password_or_path_token_and_is_read() {
        let body = postmark();
        let ok = headers(&[
            ("content-type", "application/json".into()),
            ("authorization", basic("branchyard", "s3cret")),
        ]);
        let e =
            event(receive(EventSource::Postmark, &ok, &body, "s3cret", None, NOW, 300).unwrap());
        assert_eq!((e.source.as_str(), e.kind.as_str()), ("postmark", KIND));
        assert_eq!(e.id, "CAF1234@mail.example.com", "the Message-ID header");
        assert_eq!(
            e.delivery.as_deref(),
            Some("73e6d360-66eb-11e1-8e72-a8904824019b")
        );
        assert_eq!(e.author.as_deref(), Some("alice@example.com"));
        assert_eq!(e.title.as_deref(), Some("Deploy failed on prod"));
        assert_eq!(e.labels, ["spf:pass", "dkim:pass"]);
        let m = e.email.as_ref().unwrap();
        assert_eq!(m.from_name.as_deref(), Some("Alice Example"));
        assert_eq!(m.to, ["ops+deploy@by.example"]);
        assert_eq!(m.envelope_to, ["ops+deploy@by.example"]);
        assert_eq!(m.text, "The deploy of 4711 failed.\nSee the log.");
        assert_eq!(
            m.attachments,
            [EmailAttachment {
                name: "deploy.log".into(),
                content_type: "text/plain".into(),
                size: Some(11)
            }]
        );
        // Contents are never kept: not in the payload either.
        let payload = serde_json::to_string(&e.payload).unwrap();
        assert!(!payload.contains("aGVsbG8"), "{payload}");
        assert!(!payload.contains("<p>"), "{payload}");

        // The secret as the URL's last segment, without a password.
        let plain = headers(&[("content-type", "application/json".into())]);
        assert!(receive(
            EventSource::Postmark,
            &plain,
            &body,
            "s3cret",
            Some("s3cret"),
            NOW,
            300
        )
        .is_ok());
        // Unauthenticated, a wrong password, a wrong path token, or a
        // right password beside a wrong path token: refused.
        for (h, token) in [
            (headers(&[]), None),
            (
                headers(&[("authorization", basic("branchyard", "nope"))]),
                None,
            ),
            (headers(&[]), Some("nope")),
            (
                headers(&[("authorization", basic("x", "s3cret"))]),
                Some("nope"),
            ),
            (headers(&[("authorization", "Bearer s3cret".into())]), None),
        ] {
            let err =
                receive(EventSource::Postmark, &h, &body, "s3cret", token, NOW, 300).unwrap_err();
            assert_eq!(err.code, "invalid_signature");
            assert!(err.message.contains("signs nothing"), "{}", err.message);
        }
    }

    fn mailgun_form(secret: &str, timestamp: u64, token: &str, body_plain: &str) -> Vec<u8> {
        let signature = sign(
            secret,
            &[timestamp.to_string().as_bytes(), token.as_bytes()],
        );
        let headers = serde_json::json!([
            ["From", "Bob <bob@partner.example>"],
            ["To", "ops@by.example, \"Doe, Jane\" <jane@by.example>"],
            ["Subject", "Build broken"],
            ["Message-Id", "<20261001.abc@partner.example>"],
            ["X-Mailgun-Spf", "Pass"],
            ["X-Mailgun-Dkim-Check-Result", "Fail"],
            ["X-Mailgun-Sflag", "No"]
        ])
        .to_string();
        let mut body = String::new();
        for (name, value) in [
            ("recipient", "ops@by.example"),
            ("sender", "bounce@partner.example"),
            ("from", "Bob <bob@partner.example>"),
            ("subject", "Build broken"),
            ("body-plain", body_plain),
            ("message-headers", headers.as_str()),
            ("timestamp", &timestamp.to_string()),
            ("token", token),
            ("signature", &signature),
        ] {
            if !body.is_empty() {
                body.push('&');
            }
            body.push_str(name);
            body.push('=');
            body.push_str(&branchyard_client::http::encode(value).replace("%20", "+"));
        }
        body.into_bytes()
    }

    #[test]
    fn mailgun_forms_are_signed_over_timestamp_and_token_within_the_window() {
        let ts = NOW / 1000 - 30;
        let body = mailgun_form("key-1", ts, "tok-abc", "It broke.");
        let h = headers(&[("content-type", "application/x-www-form-urlencoded".into())]);
        let received = receive(EventSource::Mailgun, &h, &body, "key-1", None, NOW, 300).unwrap();
        assert_eq!(received.nonce.as_deref(), Some("tok-abc"));
        let e = event(received);
        assert_eq!(e.id, "20261001.abc@partner.example");
        assert_eq!(e.author.as_deref(), Some("bob@partner.example"));
        assert_eq!(e.labels, ["spf:pass", "dkim:fail"]);
        let m = e.email.unwrap();
        assert_eq!(m.to, ["ops@by.example", "jane@by.example"]);
        assert_eq!(m.envelope_to, ["ops@by.example"]);
        assert_eq!(m.text, "It broke.");

        // Another signing key, a changed token, a stale timestamp.
        let err = receive(EventSource::Mailgun, &h, &body, "key-2", None, NOW, 300).unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        let tampered = String::from_utf8(body.clone())
            .unwrap()
            .replace("token=tok-abc", "token=tok-abd");
        let err = receive(
            EventSource::Mailgun,
            &h,
            tampered.as_bytes(),
            "key-1",
            None,
            NOW,
            300,
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        let old = mailgun_form("key-1", NOW / 1000 - 3600, "tok-old", "x");
        let err = receive(EventSource::Mailgun, &h, &old, "key-1", None, NOW, 300).unwrap_err();
        assert_eq!(err.code, "stale_delivery");
        // Unsigned: refused.
        let unsigned = b"from=bob%40partner.example&subject=hi&body-plain=x";
        let err = receive(EventSource::Mailgun, &h, unsigned, "key-1", None, NOW, 300).unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        // The URL's last segment may only be json for Mailgun.
        assert!(receive(
            EventSource::Mailgun,
            &h,
            &body,
            "key-1",
            Some("key-1"),
            NOW,
            300
        )
        .is_err());
    }

    #[test]
    fn mailgun_json_is_signed_over_the_timestamp_and_body() {
        let body = serde_json::to_vec(&serde_json::json!({
            "from": "Bob <bob@partner.example>",
            "recipient": "ops@by.example",
            "subject": "Report",
            "body-plain": "",
            "body-html": "<html><head><title>x</title><style>p{}</style></head><body><p>Hello &amp; <b>welcome</b></p><script>alert(1)</script><p>Bye&nbsp;now</p></body></html>",
            "Message-Id": "<j1@partner.example>",
            "attachments": [{"filename": "a.pdf", "content-type": "application/pdf", "content": "QUJDRA=="}]
        }))
        .unwrap();
        let ts = (NOW / 1000).to_string();
        let signature = sign("key-1", &[ts.as_bytes(), &body]);
        let h = headers(&[
            ("content-type", "application/json".into()),
            ("x-mailgun-signature", signature.clone()),
            ("x-mailgun-timestamp", ts.clone()),
        ]);
        let received = receive(
            EventSource::Mailgun,
            &h,
            &body,
            "key-1",
            Some("json"),
            NOW,
            300,
        )
        .unwrap();
        assert_eq!(received.nonce, None, "the body itself is signed");
        let e = event(received);
        assert_eq!(e.id, "j1@partner.example");
        let m = e.email.unwrap();
        assert_eq!(m.text, "Hello & welcome\nBye now");
        assert_eq!(m.attachments[0].size, Some(4));
        // The body is signed: changing it breaks the signature.
        let mut tampered = body.clone();
        let at = tampered.len() - 3;
        tampered[at] ^= 1;
        let err =
            receive(EventSource::Mailgun, &h, &tampered, "key-1", None, NOW, 300).unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        let late = headers(&[
            ("content-type", "application/json".into()),
            ("x-mailgun-signature", signature),
            ("x-mailgun-timestamp", ts),
        ]);
        let err = receive(
            EventSource::Mailgun,
            &late,
            &body,
            "key-1",
            None,
            NOW + 301_000,
            300,
        )
        .unwrap_err();
        assert_eq!(err.code, "stale_delivery");
    }

    fn sendgrid(boundary: &str) -> Vec<u8> {
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .bytes(),
            );
        };
        field(
            "headers",
            "Received: from mx.example\r\nMessage-ID:\r\n <sg-1@example.com>\r\nDate: Thu, 1 Oct 2026 10:00:00 +0000\r\nFrom: Carol <carol@example.com>\r\n",
        );
        field("dkim", "{@example.com : pass}");
        field("to", "agent@by.example");
        field("from", "Carol <carol@example.com>");
        field("subject", "Please fix #12");
        field("text", "");
        field("html", "<div>Line one<br>Line two &lt;not a tag&gt;</div>");
        field(
            "envelope",
            "{\"to\":[\"agent@by.example\"],\"from\":\"carol@example.com\"}",
        );
        field("SPF", "pass");
        field("spam_score", "0.1");
        field("attachments", "1");
        field(
            "attachment-info",
            "{\"attachment1\":{\"filename\":\"trace.txt\",\"name\":\"trace.txt\",\"type\":\"text/plain\"}}",
        );
        body.extend(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"attachment1\"; filename=\"trace.txt\"\r\nContent-Type: text/plain\r\n\r\nstack\r\ntrace\r\n--{boundary}--\r\n"
            )
            .bytes(),
        );
        body
    }

    #[test]
    fn sendgrid_parses_multipart_and_needs_its_password_or_path_token() {
        let body = sendgrid("xYzZY");
        let content_type = "multipart/form-data; boundary=xYzZY".to_owned();
        let h = headers(&[
            ("content-type", content_type.clone()),
            ("authorization", basic("u", "sg-secret")),
        ]);
        let e = event(
            receive(
                EventSource::Sendgrid,
                &h,
                &body,
                "sg-secret",
                None,
                NOW,
                300,
            )
            .unwrap(),
        );
        assert_eq!(e.id, "sg-1@example.com", "a folded Message-ID header");
        assert_eq!(e.labels, ["spf:pass", "dkim:pass"]);
        let m = e.email.unwrap();
        assert_eq!(m.from, "carol@example.com");
        assert_eq!(m.text, "Line one\nLine two <not a tag>");
        assert_eq!(m.envelope_to, ["agent@by.example"]);
        assert_eq!(
            m.attachments,
            [EmailAttachment {
                name: "trace.txt".into(),
                content_type: "text/plain".into(),
                size: Some(12)
            }]
        );
        let unauthenticated = headers(&[("content-type", content_type)]);
        let err = receive(
            EventSource::Sendgrid,
            &unauthenticated,
            &body,
            "sg-secret",
            None,
            NOW,
            300,
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        assert!(receive(
            EventSource::Sendgrid,
            &unauthenticated,
            &body,
            "sg-secret",
            Some("sg-secret"),
            NOW,
            300
        )
        .is_ok());
    }

    #[test]
    fn without_a_message_id_the_id_is_a_hash_of_the_message() {
        let fields = |text: &str| serde_json::json!({"From": "a@example.com", "Subject": "s", "TextBody": text});
        let id = |v: Value| match read_value(EventSource::Postmark, &v).unwrap() {
            Delivery::Event(e) => e.id,
            _ => panic!(),
        };
        assert_eq!(id(fields("x")), id(fields("x")));
        assert_ne!(id(fields("x")), id(fields("y")));
        assert!(id(fields("x")).starts_with("sha256:"));
        assert!(read_value(
            EventSource::Postmark,
            &serde_json::json!({"From": "nobody"})
        )
        .is_err());
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_TEXT_BYTES);
        let v = serde_json::json!({"From": "a@example.com", "TextBody": text});
        let Delivery::Event(e) = read_value(EventSource::Postmark, &v).unwrap() else {
            panic!()
        };
        let m = e.email.unwrap();
        assert!(m.text_truncated);
        assert!(m.text.len() <= MAX_TEXT_BYTES && m.text.len() > MAX_TEXT_BYTES - 2);
    }

    #[test]
    fn html_is_reduced_to_text() {
        assert_eq!(
            strip_html("<html><head><script>x()</script></head><body><!-- c --><h1>T&#105;tle</h1><p>a&lt;b&gt;c &#x26; d</p><ul><li>one</li><li>two</li></ul></body></html>"),
            "Title\na<b>c & d\none\ntwo"
        );
        assert_eq!(strip_html("<SCRIPT>evil()</SCRIPT>ok"), "ok");
        assert_eq!(strip_html("unclosed <b"), "unclosed");
        assert_eq!(strip_html("&bogus; &#0; &amp"), "&bogus; &#0; &amp");
    }

    #[test]
    fn multipart_refuses_what_it_cannot_read() {
        assert!(multipart(b"no boundary here", "b").is_err());
        assert!(multipart(
            b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\nvalue",
            "b"
        )
        .is_err());
        let (fields, files) =
            multipart(b"--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1\r\n--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n2\r\n--b--\r\n", "b").unwrap();
        assert_eq!(fields["x"], "1", "the first of a repeated name");
        assert!(files.is_empty());
    }

    #[test]
    fn allowlists_take_addresses_and_domains() {
        let list = vec!["alice@example.com".to_owned(), "@corp.example".to_owned()];
        assert!(allowed(&list, "Alice@Example.com"));
        assert!(allowed(&list, "bob@corp.example"));
        assert!(!allowed(&list, "bob@evil.corp.example"), "not subdomains");
        assert!(!allowed(&list, "bob@example.com"));
        assert!(!allowed(&list, "alice@example.com.evil"));
        for good in ["a@b.example", "@b.example"] {
            assert!(valid_entry(good), "{good}");
        }
        for bad in [
            "@",
            "@nodot",
            "a b@c.example",
            "example.com",
            "@.example",
            "<a@b.c>",
        ] {
            assert!(!valid_entry(bad), "{bad}");
        }
        assert_eq!(
            address("\"Doe, J\" <J@X.example>").as_deref(),
            Some("j@x.example")
        );
        assert_eq!(
            addresses("a@x.example, \"Doe, J\" <j@x.example>, bogus"),
            ["a@x.example", "j@x.example"]
        );
    }
}
