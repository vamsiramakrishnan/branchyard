//! One error type for every layer: a kind that says what the caller can do
//! about it, and a message for people.

use std::fmt;
use std::io;

/// What went wrong, as far as a caller acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The object is not there.
    NotFound,
    /// A conditional write or delete found another generation: someone
    /// else moved first.
    Precondition,
    /// Worth trying again: a network error, a 5xx, a 429, a timeout.
    Transient,
    /// What was read does not match its name or fails to decrypt. Never
    /// retried, never used.
    Corrupt,
    /// The remote said no for good: credentials, permissions, a malformed
    /// request.
    Refused,
    /// The configuration or a request is wrong.
    Config,
    /// Over the tenant's quota.
    Quota,
    /// A legal hold forbids it.
    Held,
    /// Another runner holds the lease.
    LeaseHeld,
    /// Something local: the outbox, git, the file system.
    Local,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::NotFound => "not_found",
            Kind::Precondition => "precondition_failed",
            Kind::Transient => "transient",
            Kind::Corrupt => "corrupt",
            Kind::Refused => "refused",
            Kind::Config => "config",
            Kind::Quota => "quota",
            Kind::Held => "held",
            Kind::LeaseHeld => "lease_held",
            Kind::Local => "local",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Error {
        Error::new(Kind::NotFound, message)
    }

    pub fn precondition(message: impl Into<String>) -> Error {
        Error::new(Kind::Precondition, message)
    }

    pub fn transient(message: impl Into<String>) -> Error {
        Error::new(Kind::Transient, message)
    }

    pub fn corrupt(message: impl Into<String>) -> Error {
        Error::new(Kind::Corrupt, message)
    }

    pub fn refused(message: impl Into<String>) -> Error {
        Error::new(Kind::Refused, message)
    }

    pub fn config(message: impl Into<String>) -> Error {
        Error::new(Kind::Config, message)
    }

    pub fn local(message: impl Into<String>) -> Error {
        Error::new(Kind::Local, message)
    }

    pub fn is(&self, kind: Kind) -> bool {
        self.kind == kind
    }

    /// Whether trying the same request again may succeed.
    pub fn retryable(&self) -> bool {
        self.kind == Kind::Transient
    }

    /// The same error with `context` in front of its message.
    pub fn context(mut self, context: impl fmt::Display) -> Error {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        let kind = match e.kind() {
            io::ErrorKind::NotFound => Kind::NotFound,
            io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock => Kind::Transient,
            _ => Kind::Local,
        };
        Error::new(kind, e.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::local(format!("sync outbox: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::corrupt(format!("unreadable JSON: {e}"))
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        let kind = match e.kind {
            Kind::NotFound => io::ErrorKind::NotFound,
            Kind::Precondition | Kind::LeaseHeld => io::ErrorKind::AlreadyExists,
            Kind::Corrupt => io::ErrorKind::InvalidData,
            Kind::Config => io::ErrorKind::InvalidInput,
            Kind::Refused | Kind::Held | Kind::Quota => io::ErrorKind::PermissionDenied,
            Kind::Transient => io::ErrorKind::TimedOut,
            Kind::Local => io::ErrorKind::Other,
        };
        io::Error::new(kind, e.message)
    }
}
