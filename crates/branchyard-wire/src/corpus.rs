//! Valid and malformed HTTP/1.1 messages, as bytes, for every consumer of
//! the codec to run its own entry point against: the gateway's request
//! reader, the model upstream's and the client's response readers, the
//! sync mock server. Add a vector here and every consumer's test sees it.

/// A chunked body (what follows the head) and the bytes it decodes to.
pub const CHUNKED_VALID: &[(&str, &[u8], &[u8])] = &[
    ("one chunk", b"5\r\nhello\r\n0\r\n\r\n", b"hello"),
    ("empty body", b"0\r\n\r\n", b""),
    ("two chunks", b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n", b"abcde"),
    ("extension on the last chunk", b"0;ext\r\n\r\n", b""),
    (
        "extension with a value",
        b"5;name=val\r\nhello\r\n0\r\n\r\n",
        b"hello",
    ),
    (
        "upper-case hex",
        b"A\r\n0123456789\r\n0\r\n\r\n",
        b"0123456789",
    ),
    ("leading zeros", b"0005\r\nhello\r\n0\r\n\r\n", b"hello"),
    (
        "blank before the extension",
        b"5 ;x\r\nhello\r\n0\r\n\r\n",
        b"hello",
    ),
    ("a trailer", b"3\r\nabc\r\n0\r\nX-Sum: 1\r\n\r\n", b"abc"),
];

/// A chunked body that must not decode: each is wrong in its own way.
pub const CHUNKED_MALFORMED: &[(&str, &[u8])] = &[
    ("empty size line", b"\r\nabc\r\n0\r\n\r\n"),
    ("only an extension", b";ext\r\nabc\r\n0\r\n\r\n"),
    ("size not hex", b"zz\r\nabc\r\n0\r\n\r\n"),
    ("signed size", b"+5\r\nhello\r\n0\r\n\r\n"),
    ("negative size", b"-1\r\nhello\r\n0\r\n\r\n"),
    ("0x prefix", b"0x5\r\nhello\r\n0\r\n\r\n"),
    ("space inside the size", b"5 5\r\nhello\r\n0\r\n\r\n"),
    ("leading space", b" 5\r\nhello\r\n0\r\n\r\n"),
    (
        "size overflows u64",
        b"10000000000000000\r\nabc\r\n0\r\n\r\n",
    ),
    ("size over the cap", b"FFFFFFFFFFFFFFFF\r\nabc\r\n0\r\n\r\n"),
    ("bare LF after the size", b"5\nhello\r\n0\r\n\r\n"),
    (
        "control byte in an extension",
        b"5;a\x01b\r\nhello\r\n0\r\n\r\n",
    ),
    ("size line never ends", b"5;aaaaaaaaaaaaaaaaaaaaaaaa"),
    ("no chunks at all", b""),
    ("EOF inside a chunk", b"a\r\nhello"),
    ("EOF before the chunk's CRLF", b"5\r\nhello"),
    ("data not followed by CRLF", b"5\r\nhelloXX0\r\n\r\n"),
    ("no last chunk", b"5\r\nhello\r\n"),
    ("EOF after the last chunk's size", b"0\r\n"),
    ("truncated trailer", b"0\r\nX-T: 1"),
    ("trailer never ends", b"0\r\nX-T: 1\r\n"),
    ("trailer without a name", b"0\r\nnot-a-trailer\r\n\r\n"),
    ("trailer with a bad name", b"0\r\nbad name: 1\r\n\r\n"),
];

/// A message: start line, header lines, then the body.
fn message(start: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{start}\r\n").into_bytes();
    for header in headers {
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

const REQUEST: &str = "POST /v1/x HTTP/1.1";
const RESPONSE: &str = "HTTP/1.1 200 OK";

/// Heads whose framing is wrong, with the body that follows them. The same
/// lines are wrong in a request and in a response.
const BAD_FRAMING: &[(&str, &[&str], &[u8])] = &[
    (
        "duplicate Content-Length, equal",
        &["Content-Length: 5", "Content-Length: 5"],
        b"hello",
    ),
    (
        "duplicate Content-Length, differing",
        &["Content-Length: 5", "Content-Length: 6"],
        b"hello!",
    ),
    ("signed Content-Length", &["Content-Length: +5"], b"hello"),
    ("negative Content-Length", &["Content-Length: -1"], b""),
    ("hex Content-Length", &["Content-Length: 0x5"], b"hello"),
    ("empty Content-Length", &["Content-Length:"], b""),
    ("list Content-Length", &["Content-Length: 5, 5"], b"hello"),
    (
        "Content-Length overflows u64",
        &["Content-Length: 99999999999999999999999"],
        b"",
    ),
    (
        "chunked is not the last coding",
        &["Transfer-Encoding: chunked, gzip"],
        b"5\r\nhello\r\n0\r\n\r\n",
    ),
    (
        "chunked twice",
        &["Transfer-Encoding: chunked", "Transfer-Encoding: chunked"],
        b"5\r\nhello\r\n0\r\n\r\n",
    ),
    (
        "Content-Length longer than the body",
        &["Content-Length: 10"],
        b"abc",
    ),
];

/// Request messages that must be refused, by name.
pub fn requests_malformed() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, headers, body) in BAD_FRAMING {
        out.push((name.to_string(), message(REQUEST, headers, body)));
    }
    for (name, body) in CHUNKED_MALFORMED {
        out.push((
            format!("chunked: {name}"),
            message(REQUEST, &["Transfer-Encoding: chunked"], body),
        ));
    }
    out.push((
        "Content-Length and Transfer-Encoding together".into(),
        message(
            REQUEST,
            &["Content-Length: 5", "Transfer-Encoding: chunked"],
            b"5\r\nhello\r\n0\r\n\r\n",
        ),
    ));
    out.push((
        "Transfer-Encoding and Content-Length together".into(),
        message(
            REQUEST,
            &["Transfer-Encoding: chunked", "Content-Length: 5"],
            b"0\r\n\r\n",
        ),
    ));
    out.push((
        "a transfer coding that is not chunked".into(),
        message(REQUEST, &["Transfer-Encoding: gzip"], b"hello"),
    ));
    out.push((
        "no method".into(),
        message(" /v1/x HTTP/1.1", &["Host: h"], b""),
    ));
    out.push((
        "no target".into(),
        message("POST  HTTP/1.1", &["Host: h"], b""),
    ));
    out.push((
        "unknown version".into(),
        message("POST /v1/x HTTP/9.9", &["Host: h"], b""),
    ));
    out.push((
        "space before a header colon".into(),
        message(REQUEST, &["Host : h"], b""),
    ));
    out.push((
        "a header without a colon".into(),
        message(REQUEST, &["Host"], b""),
    ));
    out.push((
        "EOF inside the head".into(),
        b"POST /v1/x HTTP/1.1\r\nHost: h\r\n".to_vec(),
    ));
    out.push((
        "a head over the limit".into(),
        message(
            REQUEST,
            &[&format!("X-Big: {}", "a".repeat(80 * 1024))],
            b"",
        ),
    ));
    let many: Vec<String> = (0..200).map(|i| format!("X-{i}: v")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    out.push((
        "too many header fields".into(),
        message(REQUEST, &many, b""),
    ));
    out
}

/// Request messages that must be read, with the body they carry.
pub fn requests_valid() -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let mut out = vec![
        (
            "no body".to_owned(),
            message("GET /v1/x HTTP/1.1", &["Host: h"], b""),
            Vec::new(),
        ),
        (
            "Content-Length".to_owned(),
            message(REQUEST, &["Content-Length: 5"], b"hello"),
            b"hello".to_vec(),
        ),
        (
            "Content-Length: 0".to_owned(),
            message(REQUEST, &["Content-Length: 0"], b""),
            Vec::new(),
        ),
        (
            "chunked, mixed case field".to_owned(),
            message(
                REQUEST,
                &["transfer-ENCODING: Chunked"],
                b"5\r\nhello\r\n0\r\n\r\n",
            ),
            b"hello".to_vec(),
        ),
        (
            "gzip then chunked".to_owned(),
            message(
                REQUEST,
                &["Transfer-Encoding: gzip, chunked"],
                b"2\r\nhi\r\n0\r\n\r\n",
            ),
            b"hi".to_vec(),
        ),
    ];
    for (name, body, decoded) in CHUNKED_VALID {
        out.push((
            format!("chunked: {name}"),
            message(REQUEST, &["Transfer-Encoding: chunked"], body),
            decoded.to_vec(),
        ));
    }
    out
}

/// Response messages whose head or whole body must be refused.
pub fn responses_malformed() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, headers, body) in BAD_FRAMING {
        // A transfer coding that does not end in chunked is, in a
        // response, a body to the end of the stream: not an error.
        if name.contains("chunked is not the last") {
            continue;
        }
        out.push((name.to_string(), message(RESPONSE, headers, body)));
    }
    for (name, body) in CHUNKED_MALFORMED {
        out.push((
            format!("chunked: {name}"),
            message(RESPONSE, &["Transfer-Encoding: chunked"], body),
        ));
    }
    out.push(("no status code".into(), b"HTTP/1.1 \r\n\r\n".to_vec()));
    out.push((
        "unknown version".into(),
        message("HTTP/9.9 200 OK", &["Content-Length: 0"], b""),
    ));
    out.push((
        "EOF inside the head".into(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n".to_vec(),
    ));
    out
}

/// Response messages that must be read, with the body they carry.
pub fn responses_valid() -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let mut out = vec![
        (
            "Content-Length".to_owned(),
            message(RESPONSE, &["Content-Length: 5"], b"hello"),
            b"hello".to_vec(),
        ),
        (
            "until close".to_owned(),
            message(RESPONSE, &["Connection: close"], b"hello"),
            b"hello".to_vec(),
        ),
        (
            "after 100 Continue".to_owned(),
            [
                b"HTTP/1.1 100 Continue\r\n\r\n".to_vec(),
                message(RESPONSE, &["Content-Length: 2"], b"ok"),
            ]
            .concat(),
            b"ok".to_vec(),
        ),
        (
            "chunked wins over a stray Content-Length".to_owned(),
            message(
                RESPONSE,
                &["Content-Length: 99", "Transfer-Encoding: chunked"],
                b"5\r\nhello\r\n0\r\n\r\n",
            ),
            b"hello".to_vec(),
        ),
        (
            "a coding other than chunked reads to the end".to_owned(),
            message(RESPONSE, &["Transfer-Encoding: gzip"], b"xyz"),
            b"xyz".to_vec(),
        ),
        (
            "no content".to_owned(),
            message("HTTP/1.1 204 No Content", &["Content-Length: 7"], b""),
            Vec::new(),
        ),
    ];
    for (name, body, decoded) in CHUNKED_VALID {
        out.push((
            format!("chunked: {name}"),
            message(RESPONSE, &["Transfer-Encoding: chunked"], body),
            decoded.to_vec(),
        ));
    }
    out
}
