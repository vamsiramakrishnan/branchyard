//! Just enough WebSocket (RFC 6455) for the bridge: the HTTP/1.1 upgrade
//! handshake and binary messages over a blocking stream, TCP or TLS
//! ([`Stream`]).
//!
//! WebSocket is used because routers that forward actor ingress forward
//! WebSocket upgrades; the bridge's own framing ([`crate::protocol`]) rides
//! inside binary messages. Text messages and extensions are refused, pings
//! are answered, and a close ends the stream. Client frames are masked as
//! the RFC requires; the server does not check that they are, since
//! masking protects intermediaries, not the endpoints.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::{Arc, Mutex};

use ring::rand::{SecureRandom, SystemRandom};

use crate::protocol::MAX_MESSAGE;
use crate::stream::Stream;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The largest HTTP request or response head either side reads.
const MAX_HEAD: usize = 16 * 1024;

const OP_CONTINUATION: u8 = 0x0;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Standard base64 with padding, as the handshake needs.
pub(crate) fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The `Sec-WebSocket-Accept` value for a key.
pub(crate) fn accept_key(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{GUID}").as_bytes(),
    );
    base64(digest.as_ref())
}

/// An HTTP/1.1 request or response head: the start line and headers, with
/// header names lowercased.
#[derive(Debug, Default)]
pub struct Head {
    pub start: String,
    pub headers: BTreeMap<String, String>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// Whether a comma-separated header lists `token`, ignoring case.
    pub fn lists(&self, name: &str, token: &str) -> bool {
        self.header(name).is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().eq_ignore_ascii_case(token))
        })
    }
}

/// Read a head byte by byte, so nothing after it is consumed.
pub fn read_head(stream: &mut impl Read) -> io::Result<Head> {
    let mut raw = Vec::new();
    let mut byte = [0u8];
    while !raw.ends_with(b"\r\n\r\n") {
        if raw.len() >= MAX_HEAD {
            return Err(invalid("HTTP head is too large"));
        }
        match stream.read(&mut byte)? {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before the HTTP head ended",
                ))
            }
            _ => raw.push(byte[0]),
        }
    }
    let text = String::from_utf8(raw).map_err(|_| invalid("HTTP head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let start = lines.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid(format!("malformed HTTP header {line:?}")))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    Ok(Head { start, headers })
}

/// Write a complete, non-upgrade HTTP response and close the connection.
pub fn respond(stream: &mut Stream, status: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Client,
    Server,
}

/// The sending half of a WebSocket.
pub struct WsWriter {
    stream: Stream,
    role: Role,
    random: SystemRandom,
}

impl WsWriter {
    fn frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut head = Vec::with_capacity(14);
        head.push(0x80 | opcode);
        let mask_bit = if self.role == Role::Client { 0x80 } else { 0 };
        match payload.len() {
            len if len < 126 => head.push(mask_bit | len as u8),
            len if len <= u16::MAX as usize => {
                head.push(mask_bit | 126);
                head.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                head.push(mask_bit | 127);
                head.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        if self.role == Role::Client {
            let mut mask = [0u8; 4];
            self.random
                .fill(&mut mask)
                .map_err(|_| io::Error::other("no randomness for the frame mask"))?;
            head.extend_from_slice(&mask);
            let mut masked = payload.to_vec();
            for (i, byte) in masked.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
            self.stream.write_all(&head)?;
            self.stream.write_all(&masked)?;
        } else {
            self.stream.write_all(&head)?;
            self.stream.write_all(payload)?;
        }
        self.stream.flush()
    }

    /// Send one binary message.
    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "message exceeds the protocol's limit",
            ));
        }
        self.frame(OP_BINARY, payload)
    }

    /// Send a close frame; the peer answers with its own and closes.
    pub fn close(&mut self) -> io::Result<()> {
        self.frame(OP_CLOSE, &1000u16.to_be_bytes())
    }

    /// Close the TCP connection in both directions without a close frame.
    pub fn abort(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// The receiving half of a WebSocket. Pings are answered through the
/// shared writer.
pub struct WsReader {
    stream: Stream,
    writer: Arc<Mutex<WsWriter>>,
    closed: bool,
}

impl WsReader {
    /// The next binary message, or `None` once the peer closed the
    /// WebSocket or the connection.
    pub fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut message: Option<Vec<u8>> = None;
        loop {
            if self.closed {
                return Ok(None);
            }
            let mut head = [0u8; 2];
            if !read_exact_or_eof(&mut self.stream, &mut head)? {
                self.closed = true;
                return Ok(None);
            }
            let fin = head[0] & 0x80 != 0;
            if head[0] & 0x70 != 0 {
                return Err(invalid("WebSocket extensions are not supported"));
            }
            let opcode = head[0] & 0x0f;
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7f {
                126 => {
                    let mut ext = [0u8; 2];
                    self.stream.read_exact(&mut ext)?;
                    u64::from(u16::from_be_bytes(ext))
                }
                127 => {
                    let mut ext = [0u8; 8];
                    self.stream.read_exact(&mut ext)?;
                    u64::from_be_bytes(ext)
                }
                len => u64::from(len),
            };
            let so_far = message.as_ref().map_or(0, Vec::len) as u64;
            if len + so_far > MAX_MESSAGE as u64 {
                return Err(invalid("WebSocket message exceeds the protocol's limit"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.stream.read_exact(&mut payload)?;
            if masked {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            }
            match opcode {
                OP_BINARY if message.is_none() => {
                    if fin {
                        return Ok(Some(payload));
                    }
                    message = Some(payload);
                }
                OP_CONTINUATION if message.is_some() => {
                    let whole = message.as_mut().unwrap();
                    whole.extend_from_slice(&payload);
                    if fin {
                        return Ok(message);
                    }
                }
                OP_PING => {
                    let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = writer.frame(OP_PONG, &payload);
                }
                OP_PONG => {}
                OP_CLOSE => {
                    self.closed = true;
                    let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = writer.frame(OP_CLOSE, &payload[..payload.len().min(2)]);
                    let _ = writer.stream.shutdown(Shutdown::Write);
                    return Ok(None);
                }
                other => return Err(invalid(format!("unexpected WebSocket opcode {other}"))),
            }
        }
    }
}

/// Fill `buf`, or return false on end of file before its first byte.
fn read_exact_or_eof(stream: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                ))
            }
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // A reset after a close is the end of the connection.
            Err(error)
                if filled == 0
                    && matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
            {
                return Ok(false)
            }
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn split(stream: Stream, role: Role) -> io::Result<(WsReader, Arc<Mutex<WsWriter>>)> {
    let writer = Arc::new(Mutex::new(WsWriter {
        stream: stream.try_clone()?,
        role,
        random: SystemRandom::new(),
    }));
    let reader = WsReader {
        stream,
        writer: writer.clone(),
        closed: false,
    };
    Ok((reader, writer))
}

/// A refused upgrade, as the server answered it.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub body: String,
}

/// Upgrade `stream` as a client: `GET path` to `host`, asking for
/// `protocol`, with `headers` added. A non-101 answer is returned as
/// [`Refused`] inside the error, with kind `PermissionDenied` for 401 and
/// 403, `NotFound` for 404 and `Other` otherwise.
pub fn client(
    mut stream: Stream,
    host: &str,
    path: &str,
    protocol: &str,
    headers: &[(&str, &str)],
) -> io::Result<(WsReader, Arc<Mutex<WsWriter>>)> {
    let mut nonce = [0u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| io::Error::other("no randomness for the WebSocket key"))?;
    let key = base64(&nonce);
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {protocol}\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let head = read_head(&mut stream)?;
    let status: u16 = head
        .start
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid(format!("malformed HTTP status line {:?}", head.start)))?;
    if status != 101 {
        let length = head
            .header("content-length")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(0)
            .min(MAX_HEAD);
        let mut body = vec![0u8; length];
        let _ = stream.read_exact(&mut body);
        let body = String::from_utf8_lossy(&body).trim().to_owned();
        let kind = match status {
            401 | 403 => io::ErrorKind::PermissionDenied,
            404 => io::ErrorKind::NotFound,
            _ => io::ErrorKind::Other,
        };
        let message = match body.is_empty() {
            true => format!("the bridge refused the connection with HTTP {status}"),
            false => format!("the bridge refused the connection with HTTP {status}: {body}"),
        };
        return Err(io::Error::new(kind, message));
    }
    if !head.lists("upgrade", "websocket") {
        return Err(invalid("the upgrade answer is not a WebSocket"));
    }
    if head.header("sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(invalid(
            "the upgrade answer has the wrong Sec-WebSocket-Accept",
        ));
    }
    if head.header("sec-websocket-protocol") != Some(protocol) {
        return Err(invalid(format!(
            "the bridge does not speak {protocol}; it answered {:?}",
            head.header("sec-websocket-protocol")
        )));
    }
    split(stream, Role::Client)
}

/// Complete a server-side upgrade whose request head was already read and
/// accepted.
pub fn server_accept(
    mut stream: Stream,
    request: &Head,
    protocol: &str,
) -> io::Result<(WsReader, Arc<Mutex<WsWriter>>)> {
    let key = request
        .header("sec-websocket-key")
        .ok_or_else(|| invalid("missing Sec-WebSocket-Key"))?;
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n",
        accept_key(key)
    )?;
    stream.flush()?;
    split(stream, Role::Server)
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;

    #[test]
    fn base64_and_accept_key_match_the_rfc() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // RFC 6455 section 1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn messages_cross_both_ways_and_close_ends_the_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = Stream::from(stream);
            let head = read_head(&mut stream).unwrap();
            assert!(head.lists("connection", "upgrade"));
            assert_eq!(head.header("x-extra"), Some("yes"));
            let (mut reader, writer) = server_accept(stream, &head, "p.v1").unwrap();
            while let Some(message) = reader.recv().unwrap() {
                writer.lock().unwrap().send(&message).unwrap();
            }
        });
        let stream = TcpStream::connect(address).unwrap().into();
        let (mut reader, writer) =
            client(stream, "localhost", "/x", "p.v1", &[("X-Extra", "yes")]).unwrap();
        for size in [0, 1, 125, 126, 65535, 65536, 300_000] {
            let message: Vec<u8> = (0..size).map(|i| i as u8).collect();
            writer.lock().unwrap().send(&message).unwrap();
            assert_eq!(reader.recv().unwrap(), Some(message));
        }
        writer.lock().unwrap().close().unwrap();
        assert_eq!(reader.recv().unwrap(), None);
        server.join().unwrap();
    }

    #[test]
    fn a_refused_upgrade_reports_its_status_and_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = Stream::from(stream);
            read_head(&mut stream).unwrap();
            respond(&mut stream, "401 Unauthorized", "expired credential").unwrap();
        });
        let stream = TcpStream::connect(address).unwrap().into();
        let error = client(stream, "localhost", "/", "p.v1", &[])
            .err()
            .expect("refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("expired credential"), "{error}");
        server.join().unwrap();
    }
}
