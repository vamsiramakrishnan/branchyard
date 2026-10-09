//! The delegation broker: how `by`, the Python module and the MCP server,
//! running inside a harness, reach the engine that runs the harness's turn.
//!
//! Children must run on threads of that engine, so those tools only
//! translate. While any of its turns may delegate, the engine listens on a
//! Unix socket, normally `.branchyard/delegation/broker-<pid>-<n>.sock`,
//! and names it in each turn's token file. A Unix socket is a path, so it
//! stays reachable from a harness sandbox that has no network but can see
//! the repository, such as Codex's. Each request is one JSON line,
//! `{"token", "tool", "arguments"}`, answered by one line: `{"ok":
//! <result>}` or `{"error": {"kind", "message"}}`. The token alone names
//! the acting branch, and the engine checks it on every request.
//!
//! The socket is private to your user; the token files are readable by
//! your user. A process running as you can therefore act as any branch
//! with a running turn: in local mode, tokens stop mistakes, not a hostile
//! harness.

use branchyard_support::best_effort;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::{json, Value};

use crate::delegation;
use crate::projection::{self, TokenFile};
use crate::{Error, Yard};

/// Longest request line accepted.
const LINE_MAX: u64 = 1 << 20;
/// Socket paths longer than this fall back to the temporary directory;
/// `sun_path` holds 108 bytes on Linux and 104 on macOS.
const SOCKET_PATH_MAX: usize = 100;

static BROKERS: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Broker {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Broker {
    pub fn start(yard: Yard) -> io::Result<Broker> {
        let file = format!(
            "broker-{}-{}.sock",
            std::process::id(),
            BROKERS.fetch_add(1, Ordering::Relaxed)
        );
        let dir = yard.store().dir().join("delegation");
        std::fs::create_dir_all(&dir)?;
        let mut path = dir.join(&file);
        if path.as_os_str().len() > SOCKET_PATH_MAX {
            path = std::env::temp_dir().join(format!("by-{file}"));
        }
        branchyard_support::cleanup_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let accept = std::thread::Builder::new()
            .name("by-broker".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let yard = yard.clone();
                    best_effort(
                        "start the connection thread",
                        std::thread::Builder::new()
                            .name("by-broker-conn".into())
                            .spawn(move || serve(&yard, stream)),
                    );
                }
            });
        let accept = match accept {
            Ok(accept) => accept,
            Err(error) => {
                branchyard_support::cleanup_file(&path);
                return Err(error);
            }
        };
        Ok(Broker {
            path,
            stop,
            accept: Some(accept),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting connections and remove the socket. Connections
    /// already open keep being served, but their tokens have been revoked.
    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the accept loop so it sees the flag.
        let _ = UnixStream::connect(&self.path);
        if let Some(accept) = self.accept.take() {
            branchyard_support::join_reporting("broker accept", accept);
        }
        branchyard_support::cleanup_file(&self.path);
    }
}

/// Answer one connection's requests until it closes.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
fn serve(yard: &Yard, stream: UnixStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match (&mut reader).take(LINE_MAX).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if !line.ends_with('\n') => {
                let error = Error::State("request line too long".into());
                let _ = writeln!(writer, "{}", json!({ "error": describe(&error) }));
                return;
            }
            Ok(_) => {}
        }
        let response = match handle(yard, &line, &writer) {
            Ok(result) => json!({ "ok": result }),
            Err(error) => json!({ "error": describe(&error) }),
        };
        if writeln!(writer, "{response}").is_err() {
            return;
        }
    }
}

fn describe(error: &Error) -> Value {
    let mut described = json!({"kind": error.kind(), "message": error.to_string()});
    if let Some(detail) = error.detail() {
        described["detail"] = detail;
    }
    described
}

fn handle(yard: &Yard, line: &str, stream: &UnixStream) -> Result<Value, Error> {
    let request: Value =
        serde_json::from_str(line).map_err(|e| Error::State(format!("unreadable request: {e}")))?;
    let field = |name: &str| {
        request[name]
            .as_str()
            .ok_or_else(|| Error::State(format!("request without {name}")))
    };
    let mut local = delegation::local_by_token(yard, field("token")?)?;
    // A client that hung up (a `by check` its harness's tool timeout
    // killed) waits for nothing: a check it started is stopped.
    if let Ok(watched) = stream.try_clone() {
        local = local.abandoned_when(Arc::new(move || hung_up(&watched)));
    }
    delegation::dispatch(&local, field("tool")?, request["arguments"].clone())
}

/// Whether the client at the other end of `stream` closed it. A client
/// waits for each answer without writing, so a read that would find the
/// end of the stream, peeked without waiting, means it is gone.
fn hung_up(stream: &UnixStream) -> bool {
    use rustix::net::{recv, RecvFlags};
    let mut byte = [0u8; 1];
    matches!(
        recv(stream, &mut byte, RecvFlags::PEEK | RecvFlags::DONTWAIT),
        Ok((0, _)) | Err(rustix::io::Errno::CONNRESET)
    )
}

/// Why a delegation call could not reach the engine running `branch`'s
/// turn. Nothing listening at its socket means the engine stopped: say so,
/// and how the turn is recovered and continued, since the harness's own
/// tool calls fail too from then on (Claude Code reports `Tool permission
/// request failed: AbortError: Stream closed`).
fn engine_gone(branch: &str, socket: &Path, error: &io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => Error::State(format!(
            "the Branchyard engine running {branch}'s turn has stopped (nothing listens at {}): \
             this turn can no longer delegate, and its tool calls are no longer answered \
             (Claude Code reports them as `Tool permission request failed: AbortError: Stream \
             closed`). End the turn; its worktree is kept. Once the engine is recovered (any `by` \
             command run outside the harness, such as `by ls`, recovers it), `by send {branch} \
             \"<prompt>\"` continues this session, starting with a note of what happened and \
             where its children are",
            socket.display()
        )),
        _ => Error::State(format!(
            "could not reach the engine at {}: {error}",
            socket.display()
        )),
    }
}

/// A connection to the engine running a branch's turn, acting as that
/// branch.
pub(crate) struct Remote {
    /// The branch the token was issued to, for messages.
    branch: String,
    socket: PathBuf,
    token: String,
    connection: Option<(BufReader<UnixStream>, UnixStream)>,
}

impl Remote {
    /// The branch `token` was issued to under `root`, and a way to reach
    /// its engine. Nothing is opened until the first call.
    pub fn find(root: &Path, token: &str) -> Result<(String, Remote), Error> {
        let Some(file) = projection::find_token(root, token) else {
            return Err(Error::Denied(format!(
                "no running turn under {} holds this delegation token; tokens are issued \
                 when a delegating turn starts and revoked when it ends",
                root.display()
            )));
        };
        let TokenFile { branch, broker, .. } = file;
        let remote = Remote {
            branch: branch.clone(),
            socket: broker,
            token: token.to_owned(),
            connection: None,
        };
        Ok((branch, remote))
    }

    /// Call one delegation tool and return its JSON result.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, Error> {
        let result = self.exchange(tool, arguments);
        if result.is_err() {
            // Reconnect on the next call rather than reuse a broken stream.
            self.connection = None;
        }
        result?
    }

    /// Outer error: transport; inner: the engine's answer.
    fn exchange(&mut self, tool: &str, arguments: Value) -> Result<Result<Value, Error>, Error> {
        if self.connection.is_none() {
            let stream = UnixStream::connect(&self.socket)
                .map_err(|e| engine_gone(&self.branch, &self.socket, &e))?;
            let writer = stream
                .try_clone()
                .map_err(|e| Error::State(format!("connection to the engine: {e}")))?;
            self.connection = Some((BufReader::new(stream), writer));
        }
        let Some((reader, writer)) = self.connection.as_mut() else {
            return Err(Error::State("the engine connection is not open".into()));
        };
        let request = json!({"token": self.token, "tool": tool, "arguments": arguments});
        let lost = |e: io::Error| Error::State(format!("lost the connection to the engine: {e}"));
        writeln!(writer, "{request}").map_err(lost)?;
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(lost)? == 0 {
            return Err(Error::State(
                "the engine closed the connection; the turn may have ended".into(),
            ));
        }
        let response: Value = serde_json::from_str(&line)
            .map_err(|e| Error::State(format!("unreadable answer from the engine: {e}")))?;
        Ok(match (response.get("ok"), response.get("error")) {
            (_, Some(error)) => Err(Error::Remote {
                kind: error["kind"].as_str().unwrap_or("error").to_owned(),
                message: error["message"].as_str().unwrap_or("error").to_owned(),
                detail: error.get("detail").cloned().map(Box::new),
            }),
            (Some(ok), None) => Ok(ok.clone()),
            (None, None) => Err(Error::State("the engine's answer has no result".into())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The battery's crash scenario: after the engine was killed, every
    /// in-harness `by` failed with no word of why.
    #[test]
    fn a_stopped_engine_is_named_with_how_to_go_on() {
        let mut remote = Remote {
            branch: "meta".into(),
            socket: PathBuf::from("/nonexistent/branchyard/broker.sock"),
            token: "t".into(),
            connection: None,
        };
        let error = remote.call("children", json!({})).unwrap_err().to_string();
        for needed in [
            "engine running meta's turn has stopped",
            "AbortError: Stream closed",
            "by send meta",
        ] {
            assert!(error.contains(needed), "{needed:?} missing from {error}");
        }
    }
}
