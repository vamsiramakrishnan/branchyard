//! A byte stream the WebSocket layer runs over: a TCP connection in the
//! clear, or the application end of a TLS connection ([`crate::tls`]).
//!
//! Both kinds can be cloned into independent reading and writing handles
//! and shut down per direction, which is all [`crate::ws`] needs.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// A connected stream.
#[derive(Debug)]
pub enum Stream {
    /// Plain TCP.
    Tcp(TcpStream),
    /// The plaintext side of a TLS connection; a thread moves bytes between
    /// it and the TLS session ([`crate::tls`]).
    Tls(UnixStream),
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Tcp(stream) => Stream::Tcp(stream.try_clone()?),
            Stream::Tls(stream) => Stream::Tls(stream.try_clone()?),
        })
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.shutdown(how),
            Stream::Tls(stream) => stream.shutdown(how),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.set_read_timeout(timeout),
            Stream::Tls(stream) => stream.set_read_timeout(timeout),
        }
    }

    /// Whether the stream is encrypted.
    pub fn is_tls(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.read(buf),
            Stream::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.write(buf),
            Stream::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.flush(),
            Stream::Tls(stream) => stream.flush(),
        }
    }
}

impl From<TcpStream> for Stream {
    fn from(stream: TcpStream) -> Self {
        Stream::Tcp(stream)
    }
}
