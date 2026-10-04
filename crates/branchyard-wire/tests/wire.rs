//! The codec against its own corpus, typed errors for the named
//! malformed shapes, partial reads, and a table of mutations.

use std::io::{BufRead, BufReader, Cursor, Read};

use branchyard_wire::corpus;
use branchyard_wire::*;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1 << 20;

fn chunked(wire: &[u8]) -> Result<Vec<u8>, WireError> {
    read_body(Cursor::new(wire.to_vec()), Framing::Chunked, MAX_BODY)
}

/// Read a whole request: head, framing, body.
fn request(wire: &[u8]) -> Result<Option<(RequestHead, Vec<u8>)>, WireError> {
    let mut reader = BufReader::new(Cursor::new(wire.to_vec()));
    let Some(head) = read_request_head(&mut reader, MAX_HEAD)? else {
        return Ok(None);
    };
    let framing = request_framing(&head.headers)?;
    let body = read_body(&mut reader, framing, MAX_BODY)?;
    Ok(Some((head, body)))
}

/// Read a whole response: head, framing, body.
fn response(wire: &[u8]) -> Result<(ResponseHead, Vec<u8>), WireError> {
    let mut reader = BufReader::new(Cursor::new(wire.to_vec()));
    let head = read_response_head(&mut reader, MAX_HEAD)?;
    let framing = response_framing(head.status, &head.headers, false)?;
    let body = read_body(&mut reader, framing, MAX_BODY)?;
    Ok((head, body))
}

#[test]
fn every_valid_chunked_vector_decodes() {
    for (name, wire, decoded) in corpus::CHUNKED_VALID {
        assert_eq!(chunked(wire).as_deref(), Ok(*decoded), "{name}");
    }
}

#[test]
fn every_malformed_chunked_vector_is_a_typed_error() {
    for (name, wire) in corpus::CHUNKED_MALFORMED {
        let got = chunked(wire);
        assert!(got.is_err(), "{name}: decoded to {got:?}");
        assert!(
            !matches!(got, Err(WireError::Io { .. })),
            "{name}: an untyped error {got:?}"
        );
    }
}

#[test]
fn named_chunked_shapes_have_their_variants() {
    use WireError::*;
    let kind = |wire: &[u8]| chunked(wire).unwrap_err();
    assert!(matches!(kind(b"\r\nabc"), BadChunkSize(_)));
    assert!(matches!(kind(b";ext\r\n"), BadChunkSize(_)));
    assert!(matches!(kind(b"zz\r\n"), BadChunkSize(_)));
    assert!(matches!(kind(b"10000000000000000\r\n"), BadChunkSize(_)));
    assert!(matches!(
        kind(b"FFFFFFFFFFFFFFFF\r\n"),
        ChunkTooLarge { .. }
    ));
    assert!(matches!(kind(b"5\nhello"), BadChunkSize(_)));
    assert!(matches!(kind(b"5\r\nhelloXX"), BadChunkTerminator));
    assert!(matches!(kind(b"a\r\nhello"), TruncatedBody(_)));
    assert!(matches!(kind(b"0\r\nX-T: 1"), TruncatedBody(_)));
    assert!(matches!(kind(b"0\r\n"), TruncatedBody(_)));
    assert!(matches!(kind(b"0\r\nnope\r\n\r\n"), BadTrailer(_)));
    assert!(matches!(kind(b""), TruncatedBody(_)));
}

#[test]
fn chunk_sizes_parse_only_as_hex() {
    assert_eq!(parse_chunk_size(b"0"), Ok(0));
    assert_eq!(parse_chunk_size(b"ff"), Ok(255));
    assert_eq!(parse_chunk_size(b"FF;a=b"), Ok(255));
    assert_eq!(parse_chunk_size(b"0;ext"), Ok(0));
    assert_eq!(parse_chunk_size(b"FFFFFFFFFFFFFFFF"), Ok(u64::MAX));
    for bad in [
        "",
        " ",
        ";",
        "g",
        "+1",
        "-1",
        "0x1",
        "1 1",
        " 1",
        "FFFFFFFFFFFFFFFFF",
        "1\r",
    ] {
        assert!(parse_chunk_size(bad.as_bytes()).is_err(), "{bad:?}");
    }
}

#[test]
fn a_chunk_over_the_caller_cap_is_refused() {
    let mut reader =
        ChunkedReader::new(Cursor::new(b"10\r\n0123456789abcdef\r\n0\r\n\r\n".to_vec()))
            .max_chunk(8);
    let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
    assert_eq!(
        wire_error(&error),
        Some(&WireError::ChunkTooLarge { size: 16, limit: 8 })
    );
}

#[test]
fn a_chunked_body_over_the_limit_is_refused() {
    let wire = b"4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n".to_vec();
    assert_eq!(
        read_body(Cursor::new(wire.clone()), Framing::Chunked, 8)
            .unwrap()
            .len(),
        8
    );
    assert_eq!(
        read_body(Cursor::new(wire), Framing::Chunked, 7),
        Err(WireError::BodyTooLarge { limit: 7 })
    );
    assert_eq!(
        read_body(Cursor::new(vec![b'x'; 9]), Framing::Length(9), 8),
        Err(WireError::BodyTooLarge { limit: 8 })
    );
}

#[test]
fn a_failed_chunked_reader_stays_failed() {
    let mut reader = ChunkedReader::new(Cursor::new(b"zz\r\nabc".to_vec()));
    let mut buf = [0u8; 8];
    assert!(reader.read(&mut buf).is_err());
    assert!(reader.read(&mut buf).is_err());
}

#[test]
fn a_chunked_reader_stops_at_its_end() {
    // Bytes after the last chunk belong to whatever comes next.
    let mut rest = BufReader::new(Cursor::new(b"3\r\nabc\r\n0\r\n\r\nNEXT".to_vec()));
    let mut body = Vec::new();
    ChunkedReader::new(&mut rest)
        .read_to_end(&mut body)
        .unwrap();
    assert_eq!(body, b"abc");
    let mut next = String::new();
    rest.read_line(&mut next).unwrap();
    assert_eq!(next, "NEXT");
}

#[test]
fn the_requests_in_the_corpus() {
    for (name, wire, body) in corpus::requests_valid() {
        let (_, got) = request(&wire)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .unwrap();
        assert_eq!(got, body, "{name}");
    }
    for (name, wire) in corpus::requests_malformed() {
        let got = request(&wire);
        assert!(got.is_err(), "{name}: read as {got:?}");
    }
}

#[test]
fn the_responses_in_the_corpus() {
    for (name, wire, body) in corpus::responses_valid() {
        let (_, got) = response(&wire).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got, body, "{name}");
    }
    for (name, wire) in corpus::responses_malformed() {
        let got = response(&wire);
        assert!(got.is_err(), "{name}: read as {got:?}");
    }
}

#[test]
fn smuggling_shapes_are_typed() {
    use WireError::*;
    let framing = |headers: &[(&str, &str)]| {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        request_framing(&headers)
    };
    assert_eq!(
        framing(&[("Content-Length", "5"), ("Transfer-Encoding", "chunked")]),
        Err(ConflictingLength)
    );
    assert_eq!(
        framing(&[("Content-Length", "5"), ("content-length", "5")]),
        Err(DuplicateContentLength)
    );
    assert!(matches!(
        framing(&[("Content-Length", "+5")]),
        Err(BadContentLength(_))
    ));
    assert!(matches!(
        framing(&[("Content-Length", "99999999999999999999")]),
        Err(BadContentLength(_))
    ));
    assert!(matches!(
        framing(&[("Transfer-Encoding", "chunked, gzip")]),
        Err(UnsupportedTransferEncoding(_))
    ));
    assert!(matches!(
        framing(&[("Transfer-Encoding", "gzip")]),
        Err(UnsupportedTransferEncoding(_))
    ));
    assert_eq!(framing(&[("Host", "h")]), Ok(Framing::None));
    assert_eq!(framing(&[("Content-Length", "0")]), Ok(Framing::Length(0)));
    assert_eq!(
        framing(&[
            ("Transfer-Encoding", "gzip"),
            ("Transfer-Encoding", "chunked")
        ]),
        Ok(Framing::Chunked)
    );
}

#[test]
fn response_framing_follows_the_rfc() {
    let h = |pairs: &[(&str, &str)]| -> Headers {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        response_framing(200, &h(&[("Content-Length", "3")]), true),
        Ok(Framing::None)
    );
    assert_eq!(
        response_framing(204, &h(&[("Content-Length", "3")]), false),
        Ok(Framing::None)
    );
    assert_eq!(response_framing(304, &h(&[]), false), Ok(Framing::None));
    assert_eq!(
        response_framing(200, &h(&[]), false),
        Ok(Framing::UntilClose)
    );
    assert_eq!(
        response_framing(
            200,
            &h(&[("Transfer-Encoding", "chunked"), ("Content-Length", "3")]),
            false
        ),
        Ok(Framing::Chunked)
    );
    assert!(response_framing(
        200,
        &h(&[("Content-Length", "3"), ("Content-Length", "3")]),
        false
    )
    .is_err());
}

#[test]
fn heads_name_what_is_missing() {
    assert_eq!(
        read_request_head(&mut Cursor::new(Vec::new()), MAX_HEAD),
        Ok(None)
    );
    assert_eq!(
        read_request_head(
            &mut Cursor::new(b"GET / HTTP/1.1\r\nHost: h".to_vec()),
            MAX_HEAD
        ),
        Err(WireError::TruncatedHead)
    );
    assert_eq!(
        read_response_head(&mut Cursor::new(Vec::new()), MAX_HEAD),
        Err(WireError::ConnectionClosed)
    );
    assert_eq!(
        read_head(
            &mut Cursor::new(b"GET / HTTP/1.1\r\nX: aaaaaaaaaaaaaaaa\r\n\r\n".to_vec()),
            16
        ),
        Err(WireError::HeadTooLarge { limit: 16 })
    );
    let mut endless = BufReader::new(std::io::repeat(b'a'));
    assert_eq!(
        read_head(&mut endless, 1024),
        Err(WireError::HeadTooLarge { limit: 1024 })
    );
    assert!(matches!(
        parse_request_head(b"/ HTTP/1.1\r\n\r\n"),
        Err(WireError::MalformedHead(_))
    ));
    let many: String = (0..200).map(|i| format!("X-{i}: v\r\n")).collect();
    assert_eq!(
        parse_request_head(format!("GET / HTTP/1.1\r\n{many}\r\n").as_bytes()),
        Err(WireError::TooManyHeaders)
    );
}

#[test]
fn heads_parse_with_trimmed_values() {
    let head = parse_request_head(b"POST /a?b=1 HTTP/1.1\r\nHost:  h \r\nX-Y: z\r\n\r\n").unwrap();
    assert_eq!(
        (head.method.as_str(), head.target.as_str(), head.minor),
        ("POST", "/a?b=1", 1)
    );
    assert_eq!(header(&head.headers, "host"), Some("h"));
    let head = parse_response_head(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
    assert_eq!((head.status, head.reason.as_str()), (404, "Not Found"));
}

#[test]
fn an_interim_response_is_skipped_but_101_is_final() {
    let mut reader = Cursor::new(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 102 Processing\r\n\r\nHTTP/1.1 200 OK\r\n\r\n"
            .to_vec(),
    );
    assert_eq!(
        read_response_head(&mut reader, MAX_HEAD).unwrap().status,
        200
    );
    let mut reader = Cursor::new(b"HTTP/1.1 101 Switching Protocols\r\n\r\n".to_vec());
    assert_eq!(
        read_response_head(&mut reader, MAX_HEAD).unwrap().status,
        101
    );
}

/// A reader that gives one byte at a time: framing must not depend on how
/// the transport splits the stream.
struct Drip<'a>(&'a [u8]);

impl Read for Drip<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match (self.0.split_first(), buf.first_mut()) {
            (Some((byte, rest)), Some(slot)) => {
                *slot = *byte;
                self.0 = rest;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}

#[test]
fn partial_reads_decode_the_same() {
    for (name, wire, body) in corpus::requests_valid() {
        let mut reader = BufReader::with_capacity(1, Drip(&wire));
        let head = read_request_head(&mut reader, MAX_HEAD).unwrap().unwrap();
        let framing = request_framing(&head.headers).unwrap();
        assert_eq!(
            read_body(&mut reader, framing, MAX_BODY).unwrap(),
            body,
            "{name}"
        );
    }
    for (name, wire) in corpus::requests_malformed() {
        let mut reader = BufReader::with_capacity(1, Drip(&wire));
        let outcome = read_request_head(&mut reader, MAX_HEAD).and_then(|head| {
            let head = head.ok_or(WireError::ConnectionClosed)?;
            let framing = request_framing(&head.headers)?;
            read_body(&mut reader, framing, MAX_BODY)
        });
        assert!(outcome.is_err(), "{name}");
    }
}

/// Every proper prefix of a valid chunked body is an error, never a
/// shorter body: truncation cannot pass for a clean end.
#[test]
fn a_truncated_chunked_body_never_decodes() {
    for (name, wire, _) in corpus::CHUNKED_VALID {
        for cut in 0..wire.len() {
            let got = chunked(&wire[..cut]);
            assert!(got.is_err(), "{name} cut at {cut} decoded to {got:?}");
        }
    }
}

/// Flip each byte of each valid message to a few hostile values: the
/// decoder may accept or refuse, but never panics, and never reads more
/// than the limit.
#[test]
fn mutated_messages_never_panic() {
    let hostile = [b'\r', b'\n', 0, b';', b':', b'f', b'-', 0xff, b' '];
    let seeds = corpus::requests_valid()
        .into_iter()
        .map(|(_, wire, _)| wire)
        .chain(
            corpus::responses_valid()
                .into_iter()
                .map(|(_, wire, _)| wire),
        );
    for wire in seeds {
        for i in 0..wire.len() {
            for byte in hostile {
                let mut mutated = wire.clone();
                mutated[i] = byte;
                if let Ok(Some((_, body))) = request(&mutated) {
                    assert!(body.len() <= MAX_BODY);
                }
                if let Ok((_, body)) = response(&mutated) {
                    assert!(body.len() <= MAX_BODY);
                }
            }
        }
    }
}

#[test]
fn a_short_content_length_body_is_an_error() {
    let mut body = Body::new(
        BufReader::new(Cursor::new(b"abc".to_vec())),
        Framing::Length(5),
    );
    let error = body.read_to_end(&mut Vec::new()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(matches!(
        wire_error(&error),
        Some(WireError::TruncatedBody(_))
    ));
}

#[test]
fn request_heads_are_validated_on_the_way_out() {
    let head = request_head("POST", "/a?b", [("Host", "h"), ("X-A", "b")], Some(3)).unwrap();
    assert_eq!(
        String::from_utf8(head).unwrap(),
        "POST /a?b HTTP/1.1\r\nHost: h\r\nX-A: b\r\nContent-Length: 3\r\n\r\n"
    );
    let bad_header = |name, value| request_head("GET", "/", [(name, value)], None);
    for (name, value) in [
        ("X-A", "b\r\nInjected: 1"),
        ("X-A", "b\nc"),
        ("X-A", "b\0"),
        ("X A", "b"),
        ("X:A", "b"),
        ("", "b"),
        ("Content-Length", "3"),
        ("transfer-encoding", "chunked"),
    ] {
        assert!(
            matches!(bad_header(name, value), Err(WireError::BadHeader(_))),
            "{name}: {value:?}"
        );
    }
    for (method, target) in [
        ("GE T", "/"),
        ("", "/"),
        ("GET", "/a b"),
        ("GET", ""),
        ("GET", "/\r\n"),
    ] {
        assert!(
            matches!(
                request_head(method, target, [], None),
                Err(WireError::BadRequestLine(_))
            ),
            "{method:?} {target:?}"
        );
    }
}

#[test]
fn a_written_request_reads_back() {
    let mut wire = request_head("PUT", "/x", [("Host", "h")], Some(2)).unwrap();
    wire.extend_from_slice(b"hi");
    let (head, body) = request(&wire).unwrap().unwrap();
    assert_eq!((head.method.as_str(), head.target.as_str()), ("PUT", "/x"));
    assert_eq!(body, b"hi");
    let mut out = Vec::new();
    write_chunk(&mut out, b"hello").unwrap();
    write_chunk(&mut out, b"").unwrap();
    out.extend_from_slice(LAST_CHUNK);
    assert_eq!(chunked(&out).unwrap(), b"hello");
}

#[test]
fn urls_split_and_refuse() {
    let u = HttpUrl::parse("https://by.example/api/v1?x=1").unwrap();
    assert_eq!(
        (
            u.tls,
            u.host.as_str(),
            u.port,
            u.path.as_str(),
            u.query.as_deref()
        ),
        (true, "by.example", 443, "/api/v1", Some("x=1"))
    );
    assert_eq!(u.target(), "/api/v1?x=1");
    assert_eq!(u.origin(), "https://by.example");
    assert_eq!(HttpUrl::parse("http://h").unwrap().target(), "/");
    assert_eq!(HttpUrl::parse("http://h:8080/p/").unwrap().prefix(), "/p");
    assert_eq!(
        HttpUrl::parse("http://h:8080/p/").unwrap().host_header(),
        "h:8080"
    );
    let v6 = HttpUrl::parse("http://[::1]:9").unwrap();
    assert_eq!(
        (v6.host.as_str(), v6.host_header()),
        ("::1", "[::1]:9".to_owned())
    );
    assert_eq!(
        HttpUrl::parse("http://[::1]/").unwrap().host_header(),
        "[::1]"
    );
    assert!(HttpUrl::parse("http://h/?q")
        .unwrap()
        .without_query()
        .is_err());
    for bad in [
        "ftp://x",
        "HTTP://x",
        "x",
        "http://",
        "http:///p",
        "http://u@h",
        "http://u:p@h/",
        "http://h:x",
        "http://h:",
        "http://h:99999",
        "http://h:-1",
        "http://h#frag",
        "http://[::1",
        "http://[::1]x",
        "http://[]/",
        "http://a:b:80/",
        "http://h /p",
        "http://h/\r\nX: 1",
        "http://h\\@evil/",
        "http://h/é",
    ] {
        assert!(HttpUrl::parse(bad).is_err(), "{bad}");
    }
}
