//! A sandboxed turn's delegation endpoint: Branchyard's delegation tools
//! over MCP's Streamable HTTP, served on this host for a harness in a
//! sandbox.
//!
//! A local harness starts Branchyard's MCP server itself (`by mcp`, over
//! stdio), and that server reaches the engine through the broker's socket
//! with the turn's delegation token. A sandboxed harness cannot: the
//! server, `by` and the socket are on this host. So when a sandboxed turn
//! may delegate and its sandbox can reach this host (a Microsandbox guest;
//! the fake provider in tests), the engine serves the same stdio server
//! over HTTP for that turn:
//!
//! - It binds [`SandboxHost::listen`] on a port of its own, and the harness
//!   is given `http://<SandboxHost::host>:<port>/mcp` as the `branchyard`
//!   MCP server (HTTP transport) with `Authorization: Bearer <token>`.
//! - The token is the turn's, in the connector gateway's form
//!   (`docs/connectors.md#keys-and-tokens`): signed by the yard's key,
//!   `aud` the endpoint's URL, `by_branch` and `by_turn` this turn, an
//!   empty grant and `by_delegation` its scope, expiring at the turn's
//!   deadline (or after [`UNBOUNDED_TTL`] when it has none). The endpoint
//!   takes that one token and no other, verifies it and its expiry on every
//!   request, and stops when the turn ends. Nothing records it.
//! - Each MCP session (an `initialize` without `Mcp-Session-Id`) is one
//!   process of the stdio server the local path runs, started on this host
//!   with the turn's delegation token; each POSTed request is written to it
//!   as one JSON-RPC line and answered with its response
//!   (`application/json`). A notification or a response is passed on and
//!   answered `202`. `DELETE` ends a session; `GET`, a stream of the
//!   server's own messages, is not offered (`405`).
//! - That process reaches the engine through the broker, which checks the
//!   delegation token on every call as it does a local harness's: the
//!   caller acts as its own branch, on its descendants only.
//!
//! A provider whose sandbox cannot reach this host (Substrate's actors, a
//! recipe's machine) gets no endpoint: the turn records a warning and runs
//! without the tools.

use branchyard_support::{best_effort, CondvarExt as _, LockExt as _};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use branchyard_harness::{McpServer, RemoteMcpServer, RemoteTransport};
use serde_json::{json, Value};

use crate::access::TokenScopes;
use crate::connectors::{keys, Actor};
use crate::models::gateway::{presented, read_request, same, Request};
use crate::projection::{Projection, SERVER_NAME};
use crate::state::Record;
use crate::Yard;
use branchyard_support::time::now_ms;

/// How long a sandboxed turn's delegation token lives when the turn has no
/// deadline. It is refused sooner, once the turn ends and its endpoint
/// stops.
pub(crate) const UNBOUNDED_TTL: Duration = Duration::from_secs(24 * 3600);
/// The one path the endpoint serves.
const PATH: &str = "/mcp";
/// The session header of MCP's Streamable HTTP.
const SESSION: &str = "Mcp-Session-Id";
/// Sessions kept at once; opening one more ends the oldest.
const MAX_SESSIONS: usize = 8;
/// How long a request may take to arrive.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a turn's end waits for answers still being written.
const DRAIN: Duration = Duration::from_secs(5);

/// Where a sandbox reaches this host: what a per-turn service for a
/// sandboxed harness binds, and the host that harness is told to dial.
/// Set with [`Yard::use_sandbox_host`]; without it, a yard uses its
/// connector gateway's effect-ledger proxy settings
/// ([`crate::connectors::EffectsProxy`]), when they name a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxHost {
    /// The address to bind: loopback, `0.0.0.0`, or this host's address on
    /// the sandboxes' network.
    pub listen: IpAddr,
    /// The host a sandbox dials to reach `listen`: an address of this host
    /// its guest routes to (such as `192.168.127.1`, the example in
    /// `docs/connectors.md`), or `127.0.0.1` for sandboxes that share this
    /// host's network, as the fake provider's do.
    pub host: String,
}

impl SandboxHost {
    /// Loopback both ways: for sandboxes that share this host's network,
    /// such as `branchyard_sandbox::fake::FakeProvider`'s.
    pub fn loopback() -> SandboxHost {
        SandboxHost {
            listen: Ipv4Addr::LOCALHOST.into(),
            host: "127.0.0.1".to_owned(),
        }
    }
}

/// How a turn's harness can reach the delegation tools.
pub(crate) enum Reach {
    /// It runs on this host: the stdio server, as always.
    Local,
    /// Its sandbox reaches this host here.
    Host(SandboxHost),
    /// Its sandbox could reach this host, but no address is configured.
    Unset(String),
    /// Its sandbox cannot reach this host at all.
    Never(String),
}

/// How `record`'s turn can reach the delegation tools.
pub(crate) fn reach(yard: &Yard, record: &Record) -> Reach {
    let kind = crate::providers::of(record.provider.as_ref());
    if !kind.sandboxed() {
        return Reach::Local;
    }
    if !kind.reaches_host() {
        return Reach::Never(format!(
            "a {} sandbox cannot reach this host, where the delegation tools are served",
            kind.name()
        ));
    }
    match yard.sandbox_host() {
        Some(host) => Reach::Host(host),
        None => Reach::Unset(
            "no address at which a sandbox reaches this host is configured \
             ([connectors] effects_sandbox_host, or Yard::use_sandbox_host); see \
             docs/delegation.md#sandboxed-harnesses"
                .to_owned(),
        ),
    }
}

/// The one token an endpoint takes, and what it must say.
struct Bearer {
    token: String,
    /// The yard's public keys, which the token must verify against.
    jwks: Value,
    audience: String,
    by_branch: String,
    turn: String,
}

impl Bearer {
    /// Whether `presented` is this turn's token, still valid; why not.
    fn check(&self, presented: Option<&str>) -> Result<(), String> {
        let token = presented.ok_or("no bearer token was presented")?;
        if !same(token, &self.token) {
            return Err("the token is not this turn's".into());
        }
        let claims = keys::verify(&self.jwks, token)?;
        let claims: keys::Claims =
            serde_json::from_value(claims).map_err(|e| format!("the token's claims: {e}"))?;
        if claims.exp <= now_ms() / 1000 {
            return Err("the token has expired".into());
        }
        if claims.by_purpose.is_some()
            || claims.aud != self.audience
            || claims.by_branch != self.by_branch
            || claims.by_turn != self.turn
            || claims.by_delegation.is_none()
        {
            return Err("the token was not issued for this endpoint".into());
        }
        Ok(())
    }
}

/// The requests waiting on a server's answers, by JSON-RPC id; `None`
/// once the server's output has ended, so no request waits on it again.
type Waiting = Arc<Mutex<Option<HashMap<String, mpsc::Sender<Value>>>>>;

/// One MCP session: a process of the stdio server, leading its own process
/// group, and the requests waiting on its answers.
struct Session {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    waiting: Waiting,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    fn start(server: &McpServer) -> Result<Session, String> {
        let mut command = Command::new(&server.command);
        // Only this turn's variables, never an outer turn's.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("BRANCHYARD_") {
                command.env_remove(&name);
            }
        }
        let mut child = command
            .args(&server.args)
            .envs(server.env.iter().map(|(k, v)| (k, v)))
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", server.command))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            best_effort("stop the delegation server", child.kill());
            best_effort("reap the delegation server", child.wait());
            return Err("the delegation server has no pipes".into());
        };
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        let routed = waiting.clone();
        let reader = std::thread::Builder::new()
            .name("by-delegation-session".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    // An answer; the server's own requests and
                    // notifications have no stream to go to.
                    if message.get("method").is_some() {
                        continue;
                    }
                    let Some(id) = message.get("id").map(Value::to_string) else {
                        continue;
                    };
                    let waiter = routed
                        .lock_recovering("waiting")
                        .as_mut()
                        .and_then(|waiting| waiting.remove(&id));
                    if let Some(waiter) = waiter {
                        // The request may have given up waiting.
                        best_effort("hand an answer to its request", waiter.send(message));
                    }
                }
                // Every request still waiting, or sent from now on, is
                // answered that the server is gone.
                routed.lock_recovering("waiting").take();
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                best_effort("stop the delegation server", child.kill());
                best_effort("reap the delegation server", child.wait());
                return Err(format!("could not read the delegation server: {error}"));
            }
        };
        Ok(Session {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            waiting,
            reader: Mutex::new(Some(reader)),
        })
    }

    /// Write one message to the server.
    fn send(&self, message: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock_recovering("stdin");
        let pipe = stdin.as_mut().ok_or("the session has ended")?;
        writeln!(pipe, "{message}")
            .and_then(|()| pipe.flush())
            .map_err(|e| format!("the delegation server stopped: {e}"))
    }

    /// Write a request and wait for its answer.
    fn call(&self, message: &Value) -> Result<Value, String> {
        let gone =
            || "the delegation server ended before it answered; the turn may have ended".to_owned();
        let id = message["id"].to_string();
        let (answer, answered) = mpsc::channel();
        self.waiting
            .lock_recovering("waiting")
            .as_mut()
            .ok_or_else(gone)?
            .insert(id.clone(), answer);
        if let Err(error) = self.send(message) {
            if let Some(waiting) = self.waiting.lock_recovering("waiting").as_mut() {
                waiting.remove(&id);
            }
            return Err(error);
        }
        answered.recv().map_err(|_| gone())
    }

    /// Stop the server and anything it started; requests still waiting are
    /// answered that it ended.
    fn end(&self) {
        self.stdin.lock_recovering("stdin").take();
        // Taken, so a second end signals nothing: once reaped, its id may
        // name another process.
        if let Some(mut child) = self.child.lock_recovering("child").take() {
            // Not reaped yet, so its id still names its group.
            branchyard_support::kill_group(child.id());
            best_effort("reap the delegation server", child.wait());
        }
        if let Some(reader) = self.reader.lock_recovering("reader").take() {
            branchyard_support::join_reporting("delegation session reader", reader);
        }
    }
}

/// What a turn's endpoint knows.
struct State {
    bearer: Bearer,
    /// The stdio server each session runs, on this host.
    server: McpServer,
    /// Sessions by id, with the order they were opened in.
    sessions: Mutex<HashMap<String, (u64, Arc<Session>)>>,
    opened: AtomicU64,
    stop: AtomicBool,
}

impl State {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock_recovering("sessions")
            .get(id)
            .map(|(_, session)| session.clone())
    }

    /// Open a session: its id and its server.
    fn open(&self) -> Result<(String, Arc<Session>), String> {
        let ended = || "the turn has ended".to_owned();
        if self.stop.load(Ordering::SeqCst) {
            return Err(ended());
        }
        let token = crate::projection::new_token().map_err(|e| e.to_string())?;
        let id = token[..32].to_owned();
        let session = Arc::new(Session::start(&self.server)?);
        let evicted = {
            let mut sessions = self.sessions.lock_recovering("sessions");
            if self.stop.load(Ordering::SeqCst) {
                drop(sessions);
                session.end();
                return Err(ended());
            }
            let evicted = match sessions.len() >= MAX_SESSIONS {
                true => sessions
                    .iter()
                    .min_by_key(|(_, (order, _))| *order)
                    .map(|(id, _)| id.clone())
                    .and_then(|oldest| sessions.remove(&oldest)),
                false => None,
            };
            let order = self.opened.fetch_add(1, Ordering::Relaxed);
            sessions.insert(id.clone(), (order, session.clone()));
            evicted
        };
        if let Some((_, oldest)) = evicted {
            oldest.end();
        }
        Ok((id, session))
    }

    fn close(&self, id: &str) -> bool {
        let removed = self.sessions.lock_recovering("sessions").remove(id);
        match removed {
            Some((_, session)) => {
                session.end();
                true
            }
            None => false,
        }
    }
}

/// A turn's endpoint, running; stopped, with its sessions, when dropped.
pub(crate) struct Endpoint {
    state: Arc<State>,
    address: SocketAddr,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Endpoint {
    /// Serve `projection`'s tools to `record`'s sandboxed turn at `host`,
    /// and give the harness the endpoint in place of the stdio server.
    /// `scopes` and `deadline_ms` are the turn's, for its token.
    pub(crate) fn start(
        yard: &Yard,
        record: &Record,
        host: &SandboxHost,
        projection: &mut Projection,
        scopes: &TokenScopes,
        deadline_ms: Option<u64>,
    ) -> Result<Endpoint, String> {
        let failed = |e: io::Error| format!("could not start the delegation endpoint: {e}");
        let listener = TcpListener::bind((host.listen, 0)).map_err(failed)?;
        let address = listener.local_addr().map_err(failed)?;
        let url = format!("http://{}:{}{PATH}", host.host, address.port());
        let (token, jwks, by_branch) = issue(yard, record, &url, scopes, deadline_ms)?;
        let state = Arc::new(State {
            bearer: Bearer {
                token: token.clone(),
                jwks,
                audience: url.clone(),
                by_branch,
                turn: (record.info.turns + 1).to_string(),
            },
            server: projection.server.clone(),
            sessions: Mutex::default(),
            opened: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let endpoint = serve(listener, state, address).map_err(failed)?;
        projection.serve_remotely(RemoteMcpServer {
            name: SERVER_NAME.to_owned(),
            transport: RemoteTransport::Http,
            url,
            headers: vec![("Authorization".to_owned(), format!("Bearer {token}"))],
        });
        Ok(endpoint)
    }

    fn shut(&mut self) {
        if self.state.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut wake = self.address;
        if wake.ip().is_unspecified() {
            wake.set_ip(Ipv4Addr::LOCALHOST.into());
        }
        best_effort(
            "connect to wake the delegation endpoint",
            TcpStream::connect_timeout(&wake, Duration::from_secs(1)),
        );
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("delegation endpoint accept", thread);
        }
        // Ending the sessions answers every request still waiting on one.
        let sessions: Vec<Arc<Session>> = self
            .state
            .sessions
            .lock_recovering("sessions")
            .drain()
            .map(|(_, (_, session))| session)
            .collect();
        for session in sessions {
            session.end();
        }
        let (count, idle) = &*self.in_flight;
        let deadline = std::time::Instant::now() + DRAIN;
        let mut active = count.lock_recovering("in-flight count");
        while *active > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            active = idle
                .wait_timeout_recovering(active, left, "in-flight count")
                .0;
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.shut();
    }
}

/// The turn's token for the endpoint at `url`, the public keys it verifies
/// against, and the branch it names.
fn issue(
    yard: &Yard,
    record: &Record,
    url: &str,
    scopes: &TokenScopes,
    deadline_ms: Option<u64>,
) -> Result<(String, Value, String), String> {
    let now = now_ms() / 1000;
    let exp = match deadline_ms {
        Some(deadline) => (deadline / 1000).max(now + 1),
        None => now + UNBOUNDED_TTL.as_secs(),
    };
    let name = &record.info.name;
    // The connector gateway's issuer and keys when the yard has one, so a
    // server's tokens all come from one signer; the local yard's otherwise.
    let (issuer, ring, actor, by_branch) = match yard.connectors() {
        Some(gateway) => (
            gateway.issuer.clone(),
            gateway.keys(),
            Actor {
                subject: gateway.subject.clone(),
                tenant: gateway.tenant.clone(),
            },
            gateway.by_branch(name),
        ),
        None => {
            let dir = crate::connectors::local_dir(yard.root());
            (
                crate::connectors::local_issuer(yard.root()).map_err(|e| e.to_string())?,
                keys::KeyRing::load_or_create(&dir.join("key"), Some(&dir.join("jwks.json"))),
                Actor {
                    subject: crate::connectors::local_subject(),
                    tenant: "local".to_owned(),
                },
                name.clone(),
            )
        }
    };
    let ring = ring.map_err(|e| format!("could not read the signing key: {e}"))?;
    let claims = crate::connectors::turn_claims(crate::connectors::TurnToken {
        issuer: &issuer,
        audience: url,
        actor: record.actor.clone().unwrap_or(actor),
        by_branch: by_branch.clone(),
        turn: record.info.turns + 1,
        window: (now, exp),
        grants: Vec::new(),
        scopes,
    })?;
    let token = ring
        .sign(&claims)
        .map_err(|e| format!("could not sign the delegation token: {e}"))?;
    let jwks = ring.jwks().map_err(|e| e.to_string())?;
    Ok((token, jwks, by_branch))
}

fn serve(listener: TcpListener, state: Arc<State>, address: SocketAddr) -> io::Result<Endpoint> {
    let in_flight = Arc::new((Mutex::new(0usize), Condvar::new()));
    let thread = {
        let (state, in_flight) = (state.clone(), in_flight.clone());
        std::thread::Builder::new()
            .name("by-delegation-http".into())
            .spawn(move || accept(listener, state, in_flight))?
    };
    Ok(Endpoint {
        state,
        address,
        in_flight,
        thread: Some(thread),
    })
}

fn accept(listener: TcpListener, state: Arc<State>, in_flight: Arc<(Mutex<usize>, Condvar)>) {
    static THREADS: AtomicUsize = AtomicUsize::new(0);
    for stream in listener.incoming() {
        if state.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let (state, in_flight) = (state.clone(), in_flight.clone());
        *in_flight.0.lock_recovering("in-flight count") += 1;
        let n = THREADS.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("by-delegation-http-{n}"))
            .spawn(move || {
                handle(stream, &state);
                let (count, idle) = &*in_flight;
                *count.lock_recovering("in-flight count") -= 1;
                idle.notify_all();
            });
        if spawned.is_err() {
            return;
        }
    }
}

/// What the endpoint answers.
#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

impl Reply {
    fn json(body: Value) -> Reply {
        Reply {
            status: 200,
            headers: Vec::new(),
            body: Some(body),
        }
    }

    /// A JSON-RPC error answering `id`, with an HTTP status.
    fn error(status: u16, id: Option<Value>, message: &str) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "error": {"code": -32000, "message": message},
            })),
        }
    }

    fn empty(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: None,
        }
    }

    fn with(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

fn handle(stream: TcpStream, state: &State) {
    best_effort(
        "bound the wait for a request",
        stream.set_read_timeout(Some(READ_TIMEOUT)),
    );
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let reply = match read_request(&mut reader, &mut out) {
        Ok(Some(request)) => route(state, &request),
        Ok(None) => return,
        Err(why) => Reply::error(400, None, &why),
    };
    best_effort("answer the harness", respond(&mut out, &reply));
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn respond(out: &mut TcpStream, reply: &Reply) -> io::Result<()> {
    let body = match &reply.body {
        Some(body) => serde_json::to_vec(body).map_err(io::Error::other)?,
        None => Vec::new(),
    };
    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    if reply.body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    out.write_all(head.as_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

/// Answer one request.
fn route(state: &State, request: &Request) -> Reply {
    let path = request.target.split('?').next().unwrap_or_default();
    if path != PATH {
        return Reply::error(404, None, "this endpoint serves /mcp only");
    }
    if let Err(why) = state.bearer.check(presented(request)) {
        return Reply::error(401, None, &why).with("WWW-Authenticate", "Bearer");
    }
    match request.method.as_str() {
        "POST" => post(state, request),
        "DELETE" => match request.header(SESSION) {
            Some(id) if state.close(id) => Reply::empty(200),
            Some(_) => Reply::error(404, None, "no such session"),
            None => Reply::error(400, None, "DELETE needs Mcp-Session-Id"),
        },
        _ => Reply::error(
            405,
            None,
            "only POST and DELETE: this endpoint sends no stream of its own",
        )
        .with("Allow", "POST, DELETE"),
    }
}

fn post(state: &State, request: &Request) -> Reply {
    let message: Value = match serde_json::from_slice(&request.body) {
        Ok(message) => message,
        Err(error) => return Reply::error(400, None, &format!("the body is not JSON: {error}")),
    };
    if !message.is_object() {
        return Reply::error(
            400,
            None,
            "send one JSON-RPC message per request; batches are not taken",
        );
    }
    let id = message.get("id").cloned();
    let (session, opened) = match request.header(SESSION) {
        Some(sid) => match state.session(sid) {
            Some(session) => (session, None),
            None => return Reply::error(404, id, "no such session; initialize again"),
        },
        None if message.get("method").and_then(Value::as_str) == Some("initialize") => {
            match state.open() {
                Ok((sid, session)) => (session, Some(sid)),
                Err(why) => return Reply::error(503, id, &why),
            }
        }
        None => return Reply::error(400, id, "Mcp-Session-Id is required after initialize"),
    };
    // A notification, or the client's answer to a request of the server's:
    // passed on, with nothing to answer.
    if message.get("method").is_none() || id.is_none() {
        return match session.send(&message) {
            Ok(()) => Reply::empty(202),
            Err(why) => Reply::error(502, None, &why),
        };
    }
    let reply = match session.call(&message) {
        Ok(answer) => Reply::json(answer),
        Err(why) => Reply::error(502, id, &why),
    };
    match opened {
        Some(sid) => reply.with(SESSION, &sid),
        None => reply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::DelegationScope;

    fn claims(branch: &str, turn: &str, audience: &str, exp: u64) -> keys::Claims {
        keys::Claims {
            iss: "branchyard:local:test".into(),
            aud: audience.into(),
            sub: "local:me".into(),
            iat: now_ms() / 1000,
            exp,
            jti: keys::random_id().unwrap(),
            by_tenant: "local".into(),
            by_branch: branch.into(),
            by_turn: turn.into(),
            by_grants: Vec::new(),
            by_purpose: None,
            by_models: None,
            by_network: None,
            by_delegation: Some(DelegationScope {
                depth: 0,
                max_depth: 1,
                max_children: 4,
                harnesses: Vec::new(),
            }),
        }
    }

    const URL: &str = "http://127.0.0.1:9/mcp";

    fn bearer(ring: &keys::KeyRing, token: &str) -> Bearer {
        Bearer {
            token: token.to_owned(),
            jwks: ring.jwks().unwrap(),
            audience: URL.into(),
            by_branch: "root".into(),
            turn: "1".into(),
        }
    }

    fn later() -> u64 {
        now_ms() / 1000 + 600
    }

    /// The endpoint takes its own turn's token, unexpired, and nothing else:
    /// no token, another one, another branch's or turn's (even signed by the
    /// yard's key), an expired one, or one signed by another key.
    #[test]
    fn only_this_turns_unexpired_token_is_taken() {
        let ring = keys::KeyRing::generate().unwrap();
        let token = ring.sign(&claims("root", "1", URL, later())).unwrap();
        let gate = bearer(&ring, &token);
        assert_eq!(gate.check(Some(&token)), Ok(()));
        assert_eq!(
            gate.check(None).unwrap_err(),
            "no bearer token was presented"
        );
        assert!(gate
            .check(Some("wrong"))
            .unwrap_err()
            .contains("not this turn's"));
        let kid = ring.sign(&claims("kid", "1", URL, later())).unwrap();
        assert!(gate
            .check(Some(&kid))
            .unwrap_err()
            .contains("not this turn's"));
        // A token that names another branch, held as this endpoint's own,
        // is still refused for what it says.
        for other in [
            claims("kid", "1", URL, later()),
            claims("root", "2", URL, later()),
            claims("root", "1", "http://127.0.0.1:10/mcp", later()),
        ] {
            let token = ring.sign(&other).unwrap();
            assert!(
                bearer(&ring, &token)
                    .check(Some(&token))
                    .unwrap_err()
                    .contains("not issued for this endpoint"),
                "{other:?}"
            );
        }
        let expired = ring
            .sign(&claims("root", "1", URL, now_ms() / 1000 - 1))
            .unwrap();
        assert_eq!(
            bearer(&ring, &expired).check(Some(&expired)).unwrap_err(),
            "the token has expired"
        );
        let stranger = keys::KeyRing::generate().unwrap();
        let forged = stranger.sign(&claims("root", "1", URL, later())).unwrap();
        let gate = Bearer {
            token: forged.clone(),
            ..bearer(&ring, &token)
        };
        assert!(gate.check(Some(&forged)).is_err());
    }

    /// A turn's state whose sessions run `command` with `args`.
    fn turn(command: &str, args: &[&str]) -> (State, String) {
        let ring = keys::KeyRing::generate().unwrap();
        let token = ring.sign(&claims("root", "1", URL, later())).unwrap();
        let state = State {
            bearer: bearer(&ring, &token),
            server: McpServer {
                name: SERVER_NAME.into(),
                command: command.into(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                env: Vec::new(),
            },
            sessions: Mutex::default(),
            opened: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        };
        (state, token)
    }

    /// A stand-in for the stdio server that answers each request with the
    /// request itself, `method` renamed `result` (so it reads as an answer,
    /// with the same id), and each notification with a line that answers
    /// nothing.
    fn echo() -> (State, String) {
        turn("sed", &["-u", "s/\"method\"/\"result\"/"])
    }

    fn answer_to(request: &Value) -> Value {
        let mut answer = request.as_object().unwrap().clone();
        let method = answer.remove("method").unwrap();
        answer.insert("result".into(), method);
        Value::Object(answer)
    }

    fn request(method: &str, token: Option<&str>, session: Option<&str>, body: Value) -> Request {
        let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        if let Some(session) = session {
            headers.push((SESSION.into(), session.into()));
        }
        Request {
            method: method.into(),
            target: PATH.into(),
            headers,
            body: match body {
                Value::Null => Vec::new(),
                body => serde_json::to_vec(&body).unwrap(),
            },
        }
    }

    fn header<'a>(reply: &'a Reply, name: &str) -> Option<&'a str> {
        reply
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn end_all(state: &State) {
        let sessions: Vec<_> = state
            .sessions
            .lock_recovering("sessions")
            .drain()
            .map(|(_, (_, session))| session)
            .collect();
        for session in sessions {
            session.end();
        }
    }

    /// Streamable HTTP as the endpoint speaks it, against a stand-in for
    /// the stdio server: `initialize` opens a session and names it, a
    /// request is answered with the server's line, a notification is
    /// accepted, a session is required and must exist, the token is
    /// checked, a batch and a `GET` are refused, `DELETE` ends a session.
    #[test]
    fn sessions_relay_json_rpc_to_the_stdio_server() {
        let (state, token) = echo();
        let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let reply = route(
            &state,
            &request("POST", Some(&token), None, initialize.clone()),
        );
        assert_eq!(reply.status, 200, "{reply:?}");
        assert_eq!(reply.body, Some(answer_to(&initialize)));
        let sid = header(&reply, SESSION).unwrap().to_owned();
        assert_eq!(sid.len(), 32);

        let call = json!({"jsonrpc": "2.0", "id": "two", "method": "tools/call", "params": {}});
        let reply = route(
            &state,
            &request("POST", Some(&token), Some(&sid), call.clone()),
        );
        assert_eq!(header(&reply, SESSION), None);
        assert_eq!((reply.status, reply.body), (200, Some(answer_to(&call))));
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let reply = route(&state, &request("POST", Some(&token), Some(&sid), note));
        assert_eq!((reply.status, reply.body.is_none()), (202, true));

        let ping = json!({"jsonrpc": "2.0", "id": 3, "method": "ping"});
        let reply = route(&state, &request("POST", Some(&token), None, ping.clone()));
        assert_eq!(reply.status, 400);
        let reply = route(
            &state,
            &request("POST", Some(&token), Some("nope"), ping.clone()),
        );
        assert_eq!(reply.status, 404);
        assert_eq!(reply.body.unwrap()["id"], 3);
        let reply = route(
            &state,
            &request("POST", Some("wrong"), Some(&sid), ping.clone()),
        );
        assert_eq!(reply.status, 401);
        assert_eq!(header(&reply, "WWW-Authenticate"), Some("Bearer"));
        let reply = route(&state, &request("POST", None, Some(&sid), ping.clone()));
        assert_eq!(reply.status, 401);
        let batch = json!([ping.clone()]);
        let reply = route(&state, &request("POST", Some(&token), Some(&sid), batch));
        assert_eq!(reply.status, 400);
        let reply = route(
            &state,
            &request("GET", Some(&token), Some(&sid), Value::Null),
        );
        assert_eq!(reply.status, 405);
        let mut elsewhere = request("POST", Some(&token), Some(&sid), ping.clone());
        elsewhere.target = "/other".into();
        assert_eq!(route(&state, &elsewhere).status, 404);

        let reply = route(
            &state,
            &request("DELETE", Some(&token), Some(&sid), Value::Null),
        );
        assert_eq!(reply.status, 200);
        let reply = route(&state, &request("POST", Some(&token), Some(&sid), ping));
        assert_eq!(reply.status, 404);
        assert!(state.sessions.lock_recovering("sessions").is_empty());
    }

    /// A server that cannot answer fails the request that needed it; a
    /// server that never answers holds its request until the session
    /// ends, which answers it that the server ended, its own children
    /// included; opening more sessions than the limit ends the oldest.
    #[test]
    fn a_session_that_ends_answers_its_waiting_requests() {
        // `false` exits at once, before it answers.
        let (state, token) = turn("false", &[]);
        let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let reply = route(&state, &request("POST", Some(&token), None, initialize));
        assert_eq!(reply.status, 502, "{reply:?}");
        end_all(&state);

        // The server reads the request and waits on a child of its own,
        // which holds its output open.
        let (state, token) = turn("sh", &["-c", "read line; sleep 30"]);
        let state = Arc::new(state);
        let waiting = {
            let (state, token) = (state.clone(), token.clone());
            std::thread::spawn(move || {
                let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
                route(&state, &request("POST", Some(&token), None, initialize))
            })
        };
        let session = loop {
            let opened = state
                .sessions
                .lock_recovering("sessions")
                .values()
                .next()
                .map(|(_, session)| session.clone());
            if let Some(session) = opened {
                break session;
            }
            std::thread::yield_now();
        };
        session.end();
        let reply = waiting.join().unwrap();
        assert_eq!(reply.status, 502, "{reply:?}");
        // Ending it again signals nothing.
        session.end();
        end_all(&state);

        let (state, token) = echo();
        let mut first = None;
        for n in 0..=MAX_SESSIONS {
            let initialize = json!({"jsonrpc": "2.0", "id": n, "method": "initialize"});
            let reply = route(&state, &request("POST", Some(&token), None, initialize));
            assert_eq!(reply.status, 200, "{reply:?}");
            first.get_or_insert(header(&reply, SESSION).unwrap().to_owned());
        }
        assert_eq!(
            state.sessions.lock_recovering("sessions").len(),
            MAX_SESSIONS
        );
        assert!(state.session(first.as_deref().unwrap()).is_none());
        end_all(&state);
    }
}
