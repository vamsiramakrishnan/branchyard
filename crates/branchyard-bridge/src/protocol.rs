//! The bridge wire protocol, version 1.
//!
//! A connection is an HTTP/1.1 upgrade to WebSocket ([`crate::ws`]) that
//! names [`SUBPROTOCOL`] and carries a per-attempt credential
//! ([`crate::credential`]) as `Authorization: Bearer`. Every WebSocket
//! binary message then holds exactly one [`Frame`].
//!
//! The client's first frame is the connection's one request: [`Frame::Exec`],
//! [`Frame::PutFile`], [`Frame::GetFile`], [`Frame::PutTree`],
//! [`Frame::GetTree`], [`Frame::EndAttempt`] or [`Frame::Shutdown`]. See
//! `docs/substrate.md` for each exchange.
//!
//! Encoding: a one-byte tag, then the frame's fields in order. Integers are
//! big-endian; `bytes` is a `u32` length and that many bytes; a list is a
//! `u32` count and that many items; an optional integer is a `0` or `1`
//! byte, then the integer when `1`. A frame with an unknown tag, a short
//! field or trailing bytes is an error: a peer never guesses at a newer
//! frame. A new frame or field is a new protocol version, negotiated by the
//! WebSocket subprotocol.

use std::io;

/// The protocol version this crate speaks.
pub const VERSION: u32 = 1;
/// The WebSocket subprotocol naming [`VERSION`].
pub const SUBPROTOCOL: &str = "branchyard-bridge.v1";
/// The largest WebSocket message either side accepts.
pub const MAX_MESSAGE: usize = 1 << 20;
/// How much stream or file data one [`Frame::Data`], [`Frame::Stdin`],
/// [`Frame::Stdout`] or [`Frame::Stderr`] carries at most.
pub const CHUNK: usize = 64 * 1024;

/// Why a request failed, mapped to and from [`io::ErrorKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Other,
    NotFound,
    PermissionDenied,
    InvalidInput,
    AlreadyExists,
    Unsupported,
}

impl FailureKind {
    fn code(self) -> u8 {
        match self {
            FailureKind::Other => 0,
            FailureKind::NotFound => 1,
            FailureKind::PermissionDenied => 2,
            FailureKind::InvalidInput => 3,
            FailureKind::AlreadyExists => 4,
            FailureKind::Unsupported => 5,
        }
    }

    fn from_code(code: u8) -> io::Result<FailureKind> {
        Ok(match code {
            0 => FailureKind::Other,
            1 => FailureKind::NotFound,
            2 => FailureKind::PermissionDenied,
            3 => FailureKind::InvalidInput,
            4 => FailureKind::AlreadyExists,
            5 => FailureKind::Unsupported,
            other => return Err(invalid(format!("unknown failure kind {other}"))),
        })
    }
}

impl From<io::ErrorKind> for FailureKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::NotFound => FailureKind::NotFound,
            io::ErrorKind::PermissionDenied => FailureKind::PermissionDenied,
            io::ErrorKind::InvalidInput => FailureKind::InvalidInput,
            io::ErrorKind::AlreadyExists => FailureKind::AlreadyExists,
            io::ErrorKind::Unsupported => FailureKind::Unsupported,
            _ => FailureKind::Other,
        }
    }
}

impl From<FailureKind> for io::ErrorKind {
    fn from(kind: FailureKind) -> Self {
        match kind {
            FailureKind::Other => io::ErrorKind::Other,
            FailureKind::NotFound => io::ErrorKind::NotFound,
            FailureKind::PermissionDenied => io::ErrorKind::PermissionDenied,
            FailureKind::InvalidInput => io::ErrorKind::InvalidInput,
            FailureKind::AlreadyExists => io::ErrorKind::AlreadyExists,
            FailureKind::Unsupported => io::ErrorKind::Unsupported,
        }
    }
}

/// What a tree entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    /// A regular file; its content follows as [`Frame::Data`] frames ending
    /// with [`Frame::End`].
    File,
    /// A symbolic link to [`Frame::Entry`]'s `target`, never followed.
    Symlink,
}

/// One protocol message. See the module documentation for the order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Start `argv` without a shell, in `cwd`, with `env` added to the
    /// bridge's base environment, in a new process group. Answered by
    /// [`Frame::Started`] or [`Frame::Failed`].
    Exec {
        argv: Vec<Vec<u8>>,
        cwd: Vec<u8>,
        env: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// Write the [`Frame::Data`] that follows, up to [`Frame::End`], to
    /// `path` with permission bits `mode`, creating parent directories.
    /// Answered by [`Frame::Done`] or [`Frame::Failed`].
    PutFile {
        path: Vec<u8>,
        mode: u32,
    },
    /// Send `path` as [`Frame::Data`] ending with [`Frame::End`], or
    /// [`Frame::Failed`].
    GetFile {
        path: Vec<u8>,
    },
    /// Write the tree that follows (entries, ended by [`Frame::End`]) under
    /// directory `path`, creating it. Answered by [`Frame::Done`] or
    /// [`Frame::Failed`].
    PutTree {
        path: Vec<u8>,
    },
    /// Send the tree under directory `path`, ended by [`Frame::End`], or
    /// [`Frame::Failed`].
    GetTree {
        path: Vec<u8>,
    },
    /// End the credential's attempt: it is refused from now on, and its
    /// processes are torn down. Answered by [`Frame::Survivors`].
    EndAttempt,
    /// Tear down every process the bridge started, of any attempt.
    /// Answered by [`Frame::Survivors`].
    Shutdown,

    /// Bytes for the exec's stdin.
    Stdin(Vec<u8>),
    /// End of file on the exec's stdin.
    CloseStdin,
    /// SIGKILL to the launched process only.
    Kill,
    /// Name the exec's live process-group members, then kill the group.
    /// Answered by [`Frame::Survivors`].
    Teardown,

    /// The exec started with this process ID.
    Started {
        pid: u32,
    },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    StdoutClosed,
    StderrClosed,
    /// The launched process ended. Sent when it is reaped, independently of
    /// its output pipes, which descendants may hold open.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// Command names of the processes that were found and killed.
    Survivors(Vec<String>),
    Failed {
        kind: FailureKind,
        message: String,
    },
    Done,

    /// One entry of a tree, its path relative to the tree's root.
    Entry {
        kind: EntryKind,
        path: Vec<u8>,
        mode: u32,
        target: Vec<u8>,
    },
    Data(Vec<u8>),
    End,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct Encoder(Vec<u8>);

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value);
    }

    fn optional(&mut self, value: Option<i32>) {
        match value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                self.i32(value);
            }
        }
    }
}

struct Decoder<'a>(&'a [u8]);

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(invalid("frame is truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| invalid("a text field is not UTF-8"))
    }

    fn count(&mut self) -> io::Result<usize> {
        let count = self.u32()? as usize;
        // Every item takes at least four bytes, so a count beyond that is a
        // lie that must not size an allocation.
        if count > self.0.len() / 4 {
            return Err(invalid("list count exceeds the frame"));
        }
        Ok(count)
    }

    fn optional(&mut self) -> io::Result<Option<i32>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.i32()?)),
            other => Err(invalid(format!("bad optional marker {other}"))),
        }
    }
}

impl Frame {
    /// The frame's encoding, one WebSocket message.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder(Vec::new());
        match self {
            Frame::Exec { argv, cwd, env } => {
                e.u8(0x01);
                e.u32(argv.len() as u32);
                for arg in argv {
                    e.bytes(arg);
                }
                e.bytes(cwd);
                e.u32(env.len() as u32);
                for (name, value) in env {
                    e.bytes(name);
                    e.bytes(value);
                }
            }
            Frame::PutFile { path, mode } => {
                e.u8(0x02);
                e.bytes(path);
                e.u32(*mode);
            }
            Frame::GetFile { path } => {
                e.u8(0x03);
                e.bytes(path);
            }
            Frame::PutTree { path } => {
                e.u8(0x04);
                e.bytes(path);
            }
            Frame::GetTree { path } => {
                e.u8(0x05);
                e.bytes(path);
            }
            Frame::EndAttempt => e.u8(0x06),
            Frame::Shutdown => e.u8(0x07),
            Frame::Stdin(data) => {
                e.u8(0x10);
                e.bytes(data);
            }
            Frame::CloseStdin => e.u8(0x11),
            Frame::Kill => e.u8(0x12),
            Frame::Teardown => e.u8(0x13),
            Frame::Started { pid } => {
                e.u8(0x20);
                e.u32(*pid);
            }
            Frame::Stdout(data) => {
                e.u8(0x21);
                e.bytes(data);
            }
            Frame::Stderr(data) => {
                e.u8(0x22);
                e.bytes(data);
            }
            Frame::StdoutClosed => e.u8(0x23),
            Frame::StderrClosed => e.u8(0x24),
            Frame::Exited { code, signal } => {
                e.u8(0x25);
                e.optional(*code);
                e.optional(*signal);
            }
            Frame::Survivors(names) => {
                e.u8(0x26);
                e.u32(names.len() as u32);
                for name in names {
                    e.bytes(name.as_bytes());
                }
            }
            Frame::Failed { kind, message } => {
                e.u8(0x27);
                e.u8(kind.code());
                e.bytes(message.as_bytes());
            }
            Frame::Done => e.u8(0x28),
            Frame::Entry {
                kind,
                path,
                mode,
                target,
            } => {
                e.u8(0x30);
                e.u8(match kind {
                    EntryKind::Dir => 0,
                    EntryKind::File => 1,
                    EntryKind::Symlink => 2,
                });
                e.bytes(path);
                e.u32(*mode);
                e.bytes(target);
            }
            Frame::Data(data) => {
                e.u8(0x31);
                e.bytes(data);
            }
            Frame::End => e.u8(0x32),
        }
        e.0
    }

    /// Decode one message. Unknown tags, short fields and trailing bytes
    /// are [`io::ErrorKind::InvalidData`].
    pub fn decode(message: &[u8]) -> io::Result<Frame> {
        let mut d = Decoder(message);
        let frame = match d.u8()? {
            0x01 => {
                let argv = (0..d.count()?)
                    .map(|_| d.bytes())
                    .collect::<io::Result<_>>()?;
                let cwd = d.bytes()?;
                let env = (0..d.count()?)
                    .map(|_| Ok((d.bytes()?, d.bytes()?)))
                    .collect::<io::Result<_>>()?;
                Frame::Exec { argv, cwd, env }
            }
            0x02 => Frame::PutFile {
                path: d.bytes()?,
                mode: d.u32()?,
            },
            0x03 => Frame::GetFile { path: d.bytes()? },
            0x04 => Frame::PutTree { path: d.bytes()? },
            0x05 => Frame::GetTree { path: d.bytes()? },
            0x06 => Frame::EndAttempt,
            0x07 => Frame::Shutdown,
            0x10 => Frame::Stdin(d.bytes()?),
            0x11 => Frame::CloseStdin,
            0x12 => Frame::Kill,
            0x13 => Frame::Teardown,
            0x20 => Frame::Started { pid: d.u32()? },
            0x21 => Frame::Stdout(d.bytes()?),
            0x22 => Frame::Stderr(d.bytes()?),
            0x23 => Frame::StdoutClosed,
            0x24 => Frame::StderrClosed,
            0x25 => Frame::Exited {
                code: d.optional()?,
                signal: d.optional()?,
            },
            0x26 => Frame::Survivors(
                (0..d.count()?)
                    .map(|_| d.string())
                    .collect::<io::Result<_>>()?,
            ),
            0x27 => Frame::Failed {
                kind: FailureKind::from_code(d.u8()?)?,
                message: d.string()?,
            },
            0x28 => Frame::Done,
            0x30 => Frame::Entry {
                kind: match d.u8()? {
                    0 => EntryKind::Dir,
                    1 => EntryKind::File,
                    2 => EntryKind::Symlink,
                    other => return Err(invalid(format!("unknown entry kind {other}"))),
                },
                path: d.bytes()?,
                mode: d.u32()?,
                target: d.bytes()?,
            },
            0x31 => Frame::Data(d.bytes()?),
            0x32 => Frame::End,
            tag => return Err(invalid(format!("unknown frame tag {tag:#04x}"))),
        };
        if !d.0.is_empty() {
            return Err(invalid("trailing bytes after the frame"));
        }
        Ok(frame)
    }

    /// A [`Frame::Failed`] describing `error`.
    pub fn failed(error: &io::Error) -> Frame {
        Frame::Failed {
            kind: error.kind().into(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_frame() -> Vec<Frame> {
        vec![
            Frame::Exec {
                argv: vec![b"sh".to_vec(), b"-c".to_vec(), vec![0xff, 0]],
                cwd: b"/workspace".to_vec(),
                env: vec![(b"A".to_vec(), b"1".to_vec()), (b"B".to_vec(), Vec::new())],
            },
            Frame::PutFile {
                path: b"/tmp/x".to_vec(),
                mode: 0o755,
            },
            Frame::GetFile {
                path: b"/tmp/x".to_vec(),
            },
            Frame::PutTree {
                path: b"/home".to_vec(),
            },
            Frame::GetTree {
                path: b"/home".to_vec(),
            },
            Frame::EndAttempt,
            Frame::Shutdown,
            Frame::Stdin(b"line\n".to_vec()),
            Frame::CloseStdin,
            Frame::Kill,
            Frame::Teardown,
            Frame::Started { pid: 4242 },
            Frame::Stdout(vec![1, 2, 3]),
            Frame::Stderr(Vec::new()),
            Frame::StdoutClosed,
            Frame::StderrClosed,
            Frame::Exited {
                code: Some(-1),
                signal: None,
            },
            Frame::Exited {
                code: None,
                signal: Some(9),
            },
            Frame::Survivors(vec!["sleep".into(), "sh".into()]),
            Frame::Failed {
                kind: FailureKind::NotFound,
                message: "no such file".into(),
            },
            Frame::Done,
            Frame::Entry {
                kind: EntryKind::Symlink,
                path: b"a/b".to_vec(),
                mode: 0o777,
                target: b"../c".to_vec(),
            },
            Frame::Data(vec![0; CHUNK]),
            Frame::End,
        ]
    }

    #[test]
    fn every_frame_round_trips() {
        for frame in every_frame() {
            assert_eq!(Frame::decode(&frame.encode()).unwrap(), frame);
        }
    }

    #[test]
    fn malformed_frames_are_refused() {
        for frame in every_frame() {
            let bytes = frame.encode();
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Frame::decode(&longer).is_err(), "trailing byte: {frame:?}");
            if bytes.len() > 1 {
                assert!(
                    Frame::decode(&bytes[..bytes.len() - 1]).is_err(),
                    "truncated: {frame:?}"
                );
            }
        }
        assert!(Frame::decode(&[]).is_err());
        assert!(Frame::decode(&[0x7f]).is_err(), "unknown tag");
        // A huge list count is refused before anything is allocated.
        assert!(Frame::decode(&[0x26, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(
            Frame::decode(&[0x27, 9, 0, 0, 0, 0]).is_err(),
            "unknown kind"
        );
        assert!(Frame::decode(&[0x25, 2]).is_err(), "bad optional");
        assert!(
            Frame::decode(&[0x27, 0, 0, 0, 0, 1, 0xff]).is_err(),
            "non-UTF-8 message"
        );
    }

    #[test]
    fn failure_kinds_map_to_io_kinds() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::Unsupported,
            io::ErrorKind::Other,
        ] {
            assert_eq!(io::ErrorKind::from(FailureKind::from(kind)), kind);
        }
        assert_eq!(
            FailureKind::from(io::ErrorKind::BrokenPipe),
            FailureKind::Other
        );
    }
}
