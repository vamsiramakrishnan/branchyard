//! Source titles at the HTTP boundary: the headers an upload names its
//! source with, and the `Content-Disposition` a download names it back with.
//! `X-Source-Title` carries visible ASCII; `X-Source-Title-Encoded` carries
//! any title as an RFC 8187 extended value and, when sent, is the only one
//! read.
use crate::error::ApiError;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet};

const MAX_ENCODED_BYTES: usize = 4096;
const MAX_TITLE_BYTES: usize = 512;
const EXPECTED: &str = "send UTF-8''<percent-encoded title> (RFC 8187), a title of 1 to 512 bytes of UTF-8 without control or bidirectional characters";

/// RFC 8187 `attr-char`.
const fn is_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

/// What an extended value percent-encodes: every byte that is not `attr-char`.
const NOT_ATTR_CHAR: &AsciiSet = &{
    let mut attr_char = AsciiSet::EMPTY;
    let mut byte = 0;
    while byte < 0x80 {
        if is_attr_char(byte) {
            attr_char = attr_char.add(byte);
        }
        byte += 1;
    }
    attr_char.complement()
};

/// The title an upload names: `X-Source-Title-Encoded` when it is sent,
/// refused rather than replaced when invalid, else `X-Source-Title`.
pub(super) fn upload_title(headers: &HeaderMap) -> Result<String, ApiError> {
    if let Some(encoded) = headers.get("x-source-title-encoded") {
        return decode(encoded).map_err(|refusal| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_source_title",
                format!("X-Source-Title-Encoded {refusal}; {EXPECTED}"),
            )
        });
    }
    headers
        .get("x-source-title")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= MAX_TITLE_BYTES)
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::bad_request(
                "X-Source-Title is required, at most 512 bytes of visible ASCII; send any other title in X-Source-Title-Encoded",
            )
        })
}

/// `Content-Disposition` for a download of `name` (RFC 6266): `attachment`
/// with a quoted ASCII `filename` and an RFC 8187 `filename*`, both naming
/// what follows the last `/` or `\`.
pub(super) fn attachment(name: &str) -> Result<HeaderValue, ApiError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if base.is_empty() {
        return Ok(HeaderValue::from_static("attachment"));
    }
    let fallback: String = base
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && !matches!(c, '"' | '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = utf8_percent_encode(base, NOT_ATTR_CHAR);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .map_err(|error| ApiError::internal(format!("download name: {error}")))
}

/// Why an encoded title was refused: one rule each.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    TooLong,
    NotExtendedValue,
    Charset,
    MalformedEscape,
    InvalidUtf8,
    Length(usize),
    Control(char),
    Bidirectional(char),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::TooLong => write!(f, "is longer than {MAX_ENCODED_BYTES} bytes"),
            Refusal::NotExtendedValue => f.write_str(
                "is not an extended value: a charset, an optional language and the title, separated by single quotes",
            ),
            Refusal::Charset => f.write_str("names an unsupported charset: only UTF-8 is accepted"),
            Refusal::MalformedEscape => f.write_str(
                "holds a malformed escape: each % takes two hex digits, and every other character is an RFC 8187 attr-char",
            ),
            Refusal::InvalidUtf8 => f.write_str("decodes to invalid UTF-8"),
            Refusal::Length(bytes) => write!(
                f,
                "decodes to {bytes} bytes, outside the title length of 1 to {MAX_TITLE_BYTES} bytes"
            ),
            Refusal::Control(c) => write!(
                f,
                "decodes to a title holding control character U+{:04X}",
                u32::from(*c)
            ),
            Refusal::Bidirectional(c) => write!(
                f,
                "decodes to a title holding bidirectional control U+{:04X}",
                u32::from(*c)
            ),
        }
    }
}

/// Parse an extended value in two steps: the escapes, then the bytes they
/// decode to. The title is kept as decoded, not normalised.
fn decode(value: &HeaderValue) -> Result<String, Refusal> {
    if value.len() > MAX_ENCODED_BYTES {
        return Err(Refusal::TooLong);
    }
    let raw = value.to_str().map_err(|_| Refusal::MalformedEscape)?;
    let mut parts = raw.splitn(3, '\'');
    let (Some(charset), Some(language), Some(escaped)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::NotExtendedValue);
    };
    if !charset.eq_ignore_ascii_case("UTF-8") {
        return Err(Refusal::Charset);
    }
    if !language
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Refusal::NotExtendedValue);
    }
    check_escapes(escaped)?;
    let title = percent_decode_str(escaped)
        .decode_utf8()
        .map_err(|_| Refusal::InvalidUtf8)?;
    check_title(&title)?;
    Ok(title.into_owned())
}

/// Only `attr-char` and `%` followed by two hex digits. `percent_decode_str`
/// passes a stray `%`, `%z1` or a truncated `%4` through unchanged, so this
/// refuses them before anything is decoded.
fn check_escapes(escaped: &str) -> Result<(), Refusal> {
    let mut bytes = escaped.bytes();
    while let Some(byte) = bytes.next() {
        let valid = match byte {
            b'%' => matches!(
                (bytes.next(), bytes.next()),
                (Some(high), Some(low)) if high.is_ascii_hexdigit() && low.is_ascii_hexdigit()
            ),
            _ => is_attr_char(byte),
        };
        if !valid {
            return Err(Refusal::MalformedEscape);
        }
    }
    Ok(())
}

fn check_title(title: &str) -> Result<(), Refusal> {
    if title.is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err(Refusal::Length(title.len()));
    }
    for c in title.chars() {
        if c.is_control() {
            return Err(Refusal::Control(c));
        }
        if matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
            return Err(Refusal::Bidirectional(c));
        }
    }
    Ok(())
}

/// Each title beside its extended value as Python's `urllib.parse.quote`
/// writes it, with the `attr-char` punctuation marked safe.
#[cfg(test)]
pub(super) const TITLES: [(&str, &str); 5] = [
    (
        "Étude de marché — T3.xlsx",
        "%C3%89tude%20de%20march%C3%A9%20%E2%80%94%20T3.xlsx",
    ),
    ("季度报告.docx", "%E5%AD%A3%E5%BA%A6%E6%8A%A5%E5%91%8A.docx"),
    (
        "تقرير الربع.pdf",
        "%D8%AA%D9%82%D8%B1%D9%8A%D8%B1%20%D8%A7%D9%84%D8%B1%D8%A8%D8%B9.pdf",
    ),
    ("Backlog 📊.csv", "Backlog%20%F0%9F%93%8A.csv"),
    (
        "reports/Q3\\Board \"draft\" notes.txt",
        "reports%2FQ3%5CBoard%20%22draft%22%20notes.txt",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    fn parse(value: &str) -> Result<String, Refusal> {
        decode(&header(value))
    }

    #[test]
    fn unicode_titles_decode_byte_for_byte_and_encode_back() {
        for (title, escaped) in TITLES {
            assert_eq!(
                parse(&format!("UTF-8''{escaped}")).unwrap().as_bytes(),
                title.as_bytes()
            );
            assert_eq!(
                utf8_percent_encode(title, NOT_ATTR_CHAR).to_string(),
                escaped
            );
        }
        let decomposed = "e\u{301}tude.txt";
        assert_eq!(
            parse("UTF-8''e%CC%81tude.txt").unwrap(),
            decomposed,
            "not normalised"
        );
        let longest = "é".repeat(256);
        assert_eq!(
            parse(&format!("UTF-8''{}", "%C3%A9".repeat(256))).unwrap(),
            longest
        );
    }

    #[test]
    fn the_charset_is_utf8_in_any_case_and_a_language_is_ignored() {
        assert_eq!(parse("utf-8'fr-CA'%C3%89tude.txt").unwrap(), "Étude.txt");
        assert_eq!(parse("Utf-8'en'Q3%20report.pdf").unwrap(), "Q3 report.pdf");
    }

    #[test]
    fn each_refusal_names_its_rule() {
        use Refusal::*;
        let cases = [
            ("UTF-8''100%", MalformedEscape),
            ("UTF-8''%z1.txt", MalformedEscape),
            ("UTF-8''report%4", MalformedEscape),
            ("UTF-8''a'b.txt", MalformedEscape),
            ("UTF-8''Q3 report.pdf", MalformedEscape),
            ("UTF-8''%C3%28.txt", InvalidUtf8),
            ("ISO-8859-1''%E9tude.txt", Charset),
            ("UTF-8''", Length(0)),
            ("UTF-8''beep%07.txt", Control('\u{7}')),
            ("UTF-8''next%C2%85line.txt", Control('\u{85}')),
            ("UTF-8''invoice%E2%80%AEfdp.exe", Bidirectional('\u{202E}')),
            ("UTF-8''isolate%E2%81%A6.txt", Bidirectional('\u{2066}')),
            ("report.pdf", NotExtendedValue),
            ("UTF-8'en_US'report.pdf", NotExtendedValue),
        ];
        for (value, refusal) in cases {
            assert_eq!(parse(value), Err(refusal), "{value}");
        }
        assert_eq!(
            parse(&format!("UTF-8''{}", "a".repeat(513))),
            Err(Length(513))
        );
        assert_eq!(
            parse(&format!("UTF-8''{}", "a".repeat(MAX_ENCODED_BYTES))),
            Err(TooLong)
        );
        let raw = HeaderValue::from_bytes("UTF-8''café.txt".as_bytes()).unwrap();
        assert_eq!(decode(&raw), Err(MalformedEscape));
        for (refusal, rule) in [
            (MalformedEscape, "malformed escape"),
            (Charset, "unsupported charset"),
            (InvalidUtf8, "invalid UTF-8"),
            (Length(513), "title length"),
            (Control('\u{85}'), "control character U+0085"),
            (Bidirectional('\u{202E}'), "bidirectional control U+202E"),
        ] {
            assert!(refusal.to_string().contains(rule), "{refusal}");
        }
    }

    #[test]
    fn the_encoder_leaves_exactly_attr_char() {
        let attr_char: String = (0u8..0x80)
            .filter(|byte| {
                utf8_percent_encode(&char::from(*byte).to_string(), NOT_ATTR_CHAR)
                    .to_string()
                    .len()
                    == 1
            })
            .map(char::from)
            .collect();
        assert_eq!(
            attr_char,
            "!#$&+-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ^_`abcdefghijklmnopqrstuvwxyz|~"
        );
        assert_eq!(
            utf8_percent_encode("it's (a)*", NOT_ATTR_CHAR).to_string(),
            "it%27s%20%28a%29%2A"
        );
    }

    #[test]
    fn downloads_name_the_basename_with_an_ascii_fallback() {
        let header = |name: &str| attachment(name).unwrap().to_str().unwrap().to_owned();
        assert_eq!(
            header("季度报告.docx"),
            "attachment; filename=\"____.docx\"; filename*=UTF-8''%E5%AD%A3%E5%BA%A6%E6%8A%A5%E5%91%8A.docx"
        );
        assert_eq!(
            header("reports/Q3\\Board \"draft\" notes.txt"),
            "attachment; filename=\"Board _draft_ notes.txt\"; filename*=UTF-8''Board%20%22draft%22%20notes.txt"
        );
        assert_eq!(
            header("tab\there.txt"),
            "attachment; filename=\"tab_here.txt\"; filename*=UTF-8''tab%09here.txt"
        );
        assert_eq!(header("reports/"), "attachment");
    }
}
