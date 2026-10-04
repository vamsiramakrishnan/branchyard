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
        let response = match handle(yard, &line) {
            Ok(result) => json!({ "ok": result }),
            Err(error) => json!({ "error": describe(&error) }),
        };
        if writeln!(writer, "{response}").is_err() {
            return;
        }
    }
}

fn describe(error: &Error) -> Value {
    json!({"kind": error.kind(), "message": error.to_string()})
}

fn handle(yard: &Yard, line: &str) -> Result<Value, Error> {
    let request: Value =
        serde_json::from_str(line).map_err(|e| Error::State(format!("unreadable request: {e}")))?;
    let field = |name: &str| {
        request[name]
            .as_str()
            .ok_or_else(|| Error::State(format!("request without {name}")))
    };
    let local = delegation::local_by_token(yard, field("token")?)?;
    delegation::dispatch(&local, field("tool")?, request["arguments"].clone())
}

/// A connection to the engine running a branch's turn, acting as that
/// branch.
pub(crate) struct Remote {
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
            let stream = UnixStream::connect(&self.socket).map_err(|e| {
                Error::State(format!(
                    "could not reach the engine at {}: {e}",
                    self.socket.display()
                ))
            })?;
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
            }),
            (Some(ok), None) => Ok(ok.clone()),
            (None, None) => Err(Error::State("the engine's answer has no result".into())),
        })
    }
}
