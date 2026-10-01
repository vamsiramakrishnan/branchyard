//! A typed, blocking Rust client for the Branchyard server: the remote SDK.
//!
//! ```no_run
//! use branchyard_client::{api::{PolicySpec, TaskRequest}, new_key, Client};
//!
//! let client = Client::from_token_file("http://127.0.0.1:8421", "token")?;
//! let repo = client.repo("app");
//! let op = repo.submit_task(
//!     &TaskRequest {
//!         prompt: "Make the flaky parser test deterministic".into(),
//!         policy: PolicySpec::allow_all(),
//!         ..TaskRequest::default()
//!     },
//!     &new_key(),
//! )?;
//! for entry in repo.stream(Some(op.cursor)) {
//!     let entry = entry?;
//!     println!("{} {:?}", entry.branch, entry.activity);
//! #   break;
//! }
//! let done = client.wait(&op.id, std::time::Duration::from_millis(500))?;
//! println!("{:?}", done.state);
//! # Ok::<(), branchyard_client::Error>(())
//! ```
//!
//! What it guarantees:
//!
//! - A `POST` carries the caller's idempotency key and is retried with the
//!   same key after a connection failure, so a retry returns the original
//!   operation instead of starting another.
//! - [`EventStream`] resumes from the last sequence number it delivered after
//!   a disconnect, so it yields each feed entry at most once and in order.
//! - Dropping anything here stops observation only. Accepted work keeps
//!   running on the server.
//!
//! What it does not guarantee:
//!
//! - Delivery across a feed reset: a server whose data directory was wiped
//!   restarts its feed at sequence 1, and a stream resuming at a higher
//!   cursor waits for entries that are not the ones it saw.
//! - Asynchronous use. Every call blocks its thread; run a stream on its own
//!   thread to observe while doing other work.
//! - Certificate pinning. `https` trusts the Mozilla roots plus an optional
//!   CA file.

pub mod api;
pub mod companion;
pub mod http;
pub mod knowledge_api;
#[cfg(feature = "schema")]
pub mod schema;
pub mod sse;
pub mod storage_api;
pub mod triggers;

use std::fmt;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use backon::{BackoffBuilder, BlockingRetryable, ExponentialBackoff, ExponentialBuilder};

use branchyard::{
    ArtifactRef, Asked, BranchInfo, Children, EventPage, Graph, GraphApplied, HarnessInfo, Inbox,
    Inspection, Message, ScratchArea, ScratchLock, Steer,
};
use rustls::ClientConfig;
use serde::de::DeserializeOwned;
use serde::Serialize;

use api::{
    AnswerRequest, AskRequest, BranchEvents, BranchList, CancelRequest, CancelResult, Diff,
    ErrorBody, ErrorResponse, FeedEntry, ForkRequest, GraphRequest, HarnessList, IntegrateRequest,
    InventoryReport, MergeRequest, Operation, ReincarnateRequest, Removed, RepoEntry, RepoList,
    SendRequest, SpawnRequest, SteerRequest, TaskRequest, TextRequest,
};
use http::{encode, Endpoint, Response};
use sse::SseReader;
use storage_api::{
    Ack, ArtifactList, CreateScratchRequest, Empty, LockState, ScratchList, ShareRequest,
    DIGEST_HEADER,
};

/// Largest response body read into memory.
const MAX_BODY: usize = 256 * 1024 * 1024;
/// A downloaded artifact's response headers and raw bytes.
type BinaryResponse = (Vec<(String, String)>, Vec<u8>);
/// Attempts for one idempotent `POST`.
const POST_ATTEMPTS: u32 = 3;

/// The waits between a `POST`'s attempts after a lost connection: 250 ms,
/// then 1 s (each four times the last), for [`POST_ATTEMPTS`] in all.
fn post_backoff() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(250))
        .with_factor(4.0)
        .with_max_delay(Duration::from_secs(4))
        .with_max_times(POST_ATTEMPTS as usize - 1)
}

fn reconnect_builder() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(250))
        .with_factor(2.0)
        .with_max_delay(Duration::from_secs(5))
        .without_max_times()
}

/// The waits before each reconnect to an event stream after consecutive
/// failures: 250 ms, doubling, at most 5 s, without end. [`EventStream`]
/// uses them, and so can a caller that reconnects itself.
pub fn reconnect_backoff() -> impl Iterator<Item = Duration> + Send + 'static {
    reconnect_builder().build()
}

#[derive(Debug)]
pub enum Error {
    /// The server answered with a structured error.
    Api { status: u16, error: Box<ErrorBody> },
    /// The request did not complete: connection, TLS or timeout.
    Transport { endpoint: String, message: String },
    /// A response this client does not understand.
    Protocol(String),
    /// Bad client configuration, such as an unusable URL or token file.
    Config(String),
}

impl Error {
    /// The server's stable error code, for API errors.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { error, .. } => Some(&error.code),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api { error, .. } => f.write_str(&error.message),
            Error::Transport { endpoint, message } => {
                write!(f, "cannot reach {endpoint}: {message}")
            }
            Error::Protocol(message) => write!(f, "unexpected response from the server: {message}"),
            Error::Config(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// A fresh random idempotency key: a version 4 UUID as 32 lowercase hex
/// digits, without hyphens.
pub fn new_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A connection to one server. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    token: Arc<str>,
    tls: Option<Arc<ClientConfig>>,
    timeout: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint.to_string())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Client {
    /// A client for `url` (`http://`, `https://` or `unix:/path/to/socket`)
    /// presenting `token`.
    pub fn new(url: &str, token: impl Into<String>) -> Result<Client, Error> {
        let endpoint = Endpoint::parse(url).map_err(Error::Config)?;
        let token = token.into();
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err(Error::Config("the token is empty or spans lines".into()));
        }
        let tls = match endpoint.tls {
            true => Some(http::tls_config(None).map_err(Error::Config)?),
            false => None,
        };
        Ok(Client {
            endpoint,
            token: token.into(),
            tls,
            timeout: Duration::from_secs(60),
        })
    }

    /// Read the token from the first line of `path`.
    pub fn from_token_file(url: &str, path: impl AsRef<Path>) -> Result<Client, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("token file {}: {e}", path.display())))?;
        let token = text.lines().next().unwrap_or("").trim();
        if token.is_empty() {
            return Err(Error::Config(format!(
                "token file {} is empty",
                path.display()
            )));
        }
        Client::new(url, token)
    }

    /// Also trust the certificates in `path`, such as a private CA's.
    pub fn with_ca_file(mut self, path: impl AsRef<Path>) -> Result<Client, Error> {
        self.tls = Some(http::tls_config(Some(path.as_ref())).map_err(Error::Config)?);
        Ok(self)
    }

    /// How long to wait for a response to start. Streams use their own.
    pub fn with_timeout(mut self, timeout: Duration) -> Client {
        self.timeout = timeout;
        self
    }

    /// The server's URL, without credentials.
    pub fn endpoint(&self) -> String {
        self.endpoint.to_string()
    }

    pub fn repo(&self, name: &str) -> Repo {
        Repo {
            client: self.clone(),
            name: name.to_owned(),
        }
    }

    pub fn repos(&self) -> Result<Vec<RepoEntry>, Error> {
        Ok(self.get::<RepoList>("/v1/repos")?.repos)
    }

    /// Harness profiles, as found on the server.
    pub fn harnesses(&self) -> Result<Vec<HarnessInfo>, Error> {
        let list: HarnessList = self.get("/v1/harnesses")?;
        Ok(list.harnesses)
    }

    /// The live workers serving this caller's repositories and the
    /// harnesses each one's machine has: `GET /v1/inventory`.
    pub fn inventory(&self) -> Result<InventoryReport, Error> {
        self.get("/v1/inventory")
    }

    pub fn operation(&self, id: &str) -> Result<Operation, Error> {
        self.get(&format!("/v1/operations/{}", encode(id)))
    }

    /// The operation this caller's request with idempotency `key` created,
    /// on any server sharing the operation store: how a caller that lost
    /// the response to a `POST` finds what it started, besides retrying the
    /// `POST` with the same key.
    pub fn operation_by_key(&self, key: &str) -> Result<Operation, Error> {
        self.get(&format!("/v1/operations?idempotency_key={}", encode(key)))
    }

    /// Poll an operation until it finishes.
    pub fn wait(&self, id: &str, poll: Duration) -> Result<Operation, Error> {
        loop {
            let op = self.operation(id)?;
            if op.state.is_terminal() {
                return Ok(op);
            }
            std::thread::sleep(poll);
        }
    }

    fn transport(&self, error: impl fmt::Display) -> Error {
        Error::Transport {
            endpoint: self.endpoint.to_string(),
            message: error.to_string(),
        }
    }

    /// One request; the response head is read, the body is not.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        extra: &[(&str, String)],
        timeout: Duration,
    ) -> Result<Response, Error> {
        let content_type = body.is_some().then_some("application/json");
        self.call_raw(method, path, body, content_type, extra, timeout)
    }

    /// Like [`Client::call`], but with `content_type` instead of always
    /// `application/json`: artifact bytes are not JSON. `None` sends the
    /// body (if any) with no `Content-Type` at all.
    fn call_raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        extra: &[(&str, String)],
        timeout: Duration,
    ) -> Result<Response, Error> {
        let mut headers = vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Accept", "application/json".to_owned()),
        ];
        if let Some(content_type) = content_type {
            headers.push(("Content-Type", content_type.to_owned()));
        }
        headers.extend(extra.iter().cloned());
        let target = format!("{}{path}", self.endpoint.prefix);
        let stream = http::connect(&self.endpoint, self.tls.as_ref(), timeout)
            .map_err(|e| self.transport(e))?;
        http::send(
            stream,
            &self.endpoint,
            &http::Request {
                method,
                target: &target,
                headers: &headers,
                body,
            },
        )
        .map_err(|e| self.transport(e))
    }

    /// `GET path`, returning its response headers and raw body (not JSON):
    /// an artifact's downloaded bytes, up to `limit`.
    fn get_binary(&self, path: &str, limit: usize) -> Result<BinaryResponse, Error> {
        let response = self.call("GET", path, None, &[], self.timeout)?;
        let status = response.status;
        let headers = response.headers.clone();
        let body = response.read_body(limit).map_err(|e| self.transport(e))?;
        match (200..300).contains(&status) {
            true => Ok((headers, body)),
            false => Err(api_error(status, &body)),
        }
    }

    /// `POST path` with a raw (non-JSON) body, retried with the same
    /// idempotency key when the connection fails, like [`Client::post`].
    fn post_raw<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &[u8],
        content_type: &str,
        key: &str,
    ) -> Result<T, Error> {
        let headers = [("Idempotency-Key", key.to_owned())];
        let attempt = || {
            self.call_raw(
                "POST",
                path,
                Some(body),
                Some(content_type),
                &headers,
                self.timeout,
            )
            .and_then(|response| self.decode(response))
        };
        attempt
            .retry(post_backoff())
            .sleep(std::thread::sleep)
            .when(|e| matches!(e, Error::Transport { .. }))
            .call()
    }

    /// Read a JSON body, or the structured error.
    fn decode<T: DeserializeOwned>(&self, response: Response) -> Result<T, Error> {
        let status = response.status;
        let body = response
            .read_body(MAX_BODY)
            .map_err(|e| self.transport(e))?;
        if (200..300).contains(&status) {
            return serde_json::from_slice(&body)
                .map_err(|e| Error::Protocol(format!("HTTP {status}: {e}")));
        }
        Err(api_error(status, &body))
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let response = self.call("GET", path, None, &[], self.timeout)?;
        self.decode(response)
    }

    fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let response = self.call("DELETE", path, None, &[], self.timeout)?;
        self.decode(response)
    }

    /// `POST` with an idempotency key, retried with the same key when the
    /// connection fails, so the server runs it at most once.
    fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        key: &str,
    ) -> Result<T, Error> {
        let body = serde_json::to_vec(body).map_err(|e| Error::Config(e.to_string()))?;
        let headers = [("Idempotency-Key", key.to_owned())];
        let attempt = || {
            self.call("POST", path, Some(&body), &headers, self.timeout)
                .and_then(|response| self.decode(response))
        };
        attempt
            .retry(post_backoff())
            .sleep(std::thread::sleep)
            .when(|e| matches!(e, Error::Transport { .. }))
            .call()
    }
}

fn api_error(status: u16, body: &[u8]) -> Error {
    match serde_json::from_slice::<ErrorResponse>(body) {
        Ok(response) => Error::Api {
            status,
            error: Box::new(response.error),
        },
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let text: String = text.trim().chars().take(200).collect();
            Error::Api {
                status,
                error: Box::new(ErrorBody {
                    code: format!("http_{status}"),
                    message: match text.is_empty() {
                        true => format!("the server answered HTTP {status}"),
                        false => format!("the server answered HTTP {status}: {text}"),
                    },
                    detail: None,
                }),
            }
        }
    }
}

/// One repository served by a [`Client`]'s server.
#[derive(Clone, Debug)]
pub struct Repo {
    client: Client,
    name: String,
}

impl Repo {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn path(&self, rest: &str) -> String {
        format!("/v1/repos/{}{rest}", encode(&self.name))
    }

    fn branch_path(&self, branch: &str, rest: &str) -> String {
        self.path(&format!("/branches/{}{rest}", encode(branch)))
    }

    /// Start a task. Returns once the server has durably accepted it.
    pub fn submit_task(&self, request: &TaskRequest, key: &str) -> Result<Operation, Error> {
        self.client.post(&self.path("/tasks"), request, key)
    }

    pub fn send(&self, branch: &str, request: &SendRequest, key: &str) -> Result<Operation, Error> {
        self.client
            .post(&self.branch_path(branch, "/send"), request, key)
    }

    pub fn fork(&self, branch: &str, request: &ForkRequest, key: &str) -> Result<Operation, Error> {
        self.client
            .post(&self.branch_path(branch, "/fork"), request, key)
    }

    /// A new branch from `branch`'s latest candidate, always with a fresh
    /// session and a generated handoff brief; like `by reincarnate`.
    pub fn reincarnate(
        &self,
        branch: &str,
        request: &ReincarnateRequest,
        key: &str,
    ) -> Result<Operation, Error> {
        self.client
            .post(&self.branch_path(branch, "/reincarnate"), request, key)
    }

    pub fn merge(
        &self,
        branch: &str,
        request: &MergeRequest,
        key: &str,
    ) -> Result<Operation, Error> {
        self.client
            .post(&self.branch_path(branch, "/merge"), request, key)
    }

    /// Create a child of `parent` with the server's authority, bounded by
    /// the parent's envelope, and run its first turn; like
    /// `by spawn --parent`. The finished operation's result holds the
    /// child's inspection.
    pub fn spawn(
        &self,
        parent: &str,
        request: &SpawnRequest,
        key: &str,
    ) -> Result<Operation, Error> {
        self.client
            .post(&self.branch_path(parent, "/spawn"), request, key)
    }

    /// Merge a delegated child into the parent that delegated it, after
    /// its check passes; like `by integrate`. The finished operation's
    /// result holds the merge.
    pub fn integrate(&self, branch: &str, key: &str) -> Result<Operation, Error> {
        self.client.post(
            &self.branch_path(branch, "/integrate"),
            &IntegrateRequest::default(),
            key,
        )
    }

    /// A branch as a delegating parent sees it; like `by inspect`.
    pub fn inspect(&self, branch: &str) -> Result<Inspection, Error> {
        self.client.get(&self.branch_path(branch, "/inspection"))
    }

    /// Up to `limit` of a branch's recorded events after `cursor`, or the
    /// most recent without one; like `by events`.
    pub fn event_page(
        &self,
        branch: &str,
        cursor: Option<usize>,
        limit: usize,
    ) -> Result<EventPage, Error> {
        let mut query = format!("?limit={limit}");
        if let Some(cursor) = cursor {
            query.push_str(&format!("&cursor={cursor}"));
        }
        self.client
            .get(&self.branch_path(branch, &format!("/event-page{query}")))
    }

    /// Every branch `branch` delegated to, directly or below; like
    /// `by children`.
    pub fn children(&self, branch: &str) -> Result<Children, Error> {
        self.client.get(&self.branch_path(branch, "/children"))
    }

    /// `branch`'s graph: its children, the dependencies among them, and
    /// its graph revision; like `by graph show`.
    pub fn graph(&self, branch: &str) -> Result<Graph, Error> {
        self.client.get(&self.branch_path(branch, "/graph"))
    }

    /// Apply a graph proposal to `branch`'s children with the server's
    /// authority as a person, like `by graph apply --parent`. All or
    /// nothing; a stale `expected_revision` is `409 stale_revision`. A
    /// retry after a lost connection is refused the same way once the
    /// first attempt committed, so a proposal never applies twice.
    pub fn apply_graph(&self, branch: &str, request: &GraphRequest) -> Result<GraphApplied, Error> {
        self.client
            .post(&self.branch_path(branch, "/graph"), request, &new_key())
    }

    /// Every message addressed to `branch`, oldest first; like `by inbox`.
    pub fn inbox(&self, branch: &str) -> Result<Inbox, Error> {
        self.client.get(&self.branch_path(branch, "/inbox"))
    }

    /// Ask `branch`'s parent a question; like `by ask`. Without
    /// `wait_seconds`, returns once the question is sent; with it, blocks
    /// on the server for up to that long for an answer.
    pub fn ask(&self, branch: &str, text: &str, wait_seconds: Option<f64>) -> Result<Asked, Error> {
        self.client.post(
            &self.branch_path(branch, "/ask"),
            &AskRequest {
                text: text.to_owned(),
                wait_seconds,
            },
            &new_key(),
        )
    }

    /// Report to `branch`'s parent; like `by report`.
    pub fn report(&self, branch: &str, text: &str) -> Result<Message, Error> {
        self.client.post(
            &self.branch_path(branch, "/report"),
            &TextRequest {
                text: text.to_owned(),
            },
            &new_key(),
        )
    }

    /// Escalate to `branch`'s parent, or further up if its rig seat allows;
    /// like `by escalate`.
    pub fn escalate(&self, branch: &str, text: &str) -> Result<Message, Error> {
        self.client.post(
            &self.branch_path(branch, "/escalate"),
            &TextRequest {
                text: text.to_owned(),
            },
            &new_key(),
        )
    }

    /// Answer one of `branch`'s own descendants' messages; like `by
    /// answer`.
    pub fn answer(&self, branch: &str, message_id: u64, text: &str) -> Result<Message, Error> {
        self.client.post(
            &self.branch_path(branch, "/answer"),
            &AnswerRequest {
                message_id,
                text: text.to_owned(),
            },
            &new_key(),
        )
    }

    /// Ask the branch's running turn, and every running turn delegated
    /// below it, to stop. Returns the branches that were running; each
    /// ends `interrupted`. Safe to repeat.
    pub fn cancel(&self, branch: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .client
            .post::<CancelResult>(
                &self.branch_path(branch, "/cancel"),
                &CancelRequest::default(),
                &new_key(),
            )?
            .cancelled)
    }

    /// Add `text` to the branch's running turn without interrupting it, like
    /// `by send --steer`, in whichever process of the server runs the turn.
    /// Returns the steer once the engine has delivered or refused it, or
    /// still pending after the server's brief wait. Refused with
    /// `not_running` when no turn runs and `unsupported` when the harness
    /// cannot take input mid-turn.
    pub fn steer(&self, branch: &str, text: &str) -> Result<Steer, Error> {
        self.client.post(
            &self.branch_path(branch, "/steer"),
            &SteerRequest {
                text: text.to_owned(),
            },
            &new_key(),
        )
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>, Error> {
        Ok(self
            .client
            .get::<BranchList>(&self.path("/branches"))?
            .branches)
    }

    pub fn branch(&self, name: &str) -> Result<BranchInfo, Error> {
        self.client.get(&self.branch_path(name, ""))
    }

    /// The caller's queued and running operations of this repository, or
    /// only those working on `branch`; each says why it waits when no live
    /// worker can claim it (`Operation::waiting`).
    pub fn operations(&self, branch: Option<&str>) -> Result<Vec<Operation>, Error> {
        let path = match branch {
            Some(branch) => self.path(&format!("/operations?branch={}", encode(branch))),
            None => self.path("/operations"),
        };
        Ok(self.client.get::<api::OperationList>(&path)?.operations)
    }

    pub fn diff(&self, name: &str) -> Result<String, Error> {
        Ok(self
            .client
            .get::<Diff>(&self.branch_path(name, "/diff"))?
            .diff)
    }

    /// A branch's recorded events after the first `cursor`.
    pub fn events(&self, name: &str, cursor: u64) -> Result<BranchEvents, Error> {
        self.client
            .get(&self.branch_path(name, &format!("/events?cursor={cursor}")))
    }

    pub fn remove(&self, name: &str) -> Result<(), Error> {
        self.client
            .delete::<Removed>(&self.branch_path(name, ""))
            .map(|_| ())
    }

    /// Activity across every branch, from after `cursor`, or from now when
    /// `None`. Reconnects by cursor after a disconnect.
    pub fn stream(&self, cursor: Option<u64>) -> EventStream {
        EventStream {
            repo: self.clone(),
            cursor,
            reader: None,
            failures: 0,
            backoff: None,
            max_failures: 8,
            done: false,
        }
    }
}

/// Feed entries from `GET .../events/stream`, in order, each at most once.
/// Yields an error, then ends, after repeated failures to reconnect or when
/// the server refuses the stream.
pub struct EventStream {
    repo: Repo,
    cursor: Option<u64>,
    reader: Option<SseReader<BufReader<http::Body>>>,
    failures: u32,
    /// The waits between reconnects, from the first failure in a row.
    backoff: Option<ExponentialBackoff>,
    max_failures: u32,
    done: bool,
}

impl EventStream {
    /// The last sequence number delivered, or the server's starting point.
    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    /// Consecutive reconnect failures tolerated before giving up.
    pub fn max_failures(mut self, failures: u32) -> Self {
        self.max_failures = failures;
        self
    }

    /// Connect now and return the feed position the stream starts after:
    /// the cursor it was given, or the feed's head when it was given none.
    /// Read a snapshot (such as [`Repo::branches`]) after this, then apply
    /// the entries that follow, and nothing recorded in between is missed.
    /// Call it before the first `next`; it does not retry.
    pub fn open(&mut self) -> Result<u64, Error> {
        if self.reader.is_none() {
            self.connect()?;
        }
        let reader = self.reader.as_mut().expect("connected above");
        match reader.next_event() {
            Ok(Some(event)) if event.event == "open" => {
                let id = event
                    .id
                    .and_then(|id| id.parse().ok())
                    .ok_or_else(|| Error::Protocol("open event without a cursor".into()))?;
                self.cursor = Some(id);
                Ok(id)
            }
            Ok(Some(event)) => Err(Error::Protocol(format!(
                "the stream began with {:?}, not open",
                event.event
            ))),
            Ok(None) => {
                self.reader = None;
                Err(self.repo.client.transport("the event stream ended at once"))
            }
            Err(error) => {
                self.reader = None;
                Err(self.repo.client.transport(error))
            }
        }
    }

    fn connect(&mut self) -> Result<(), Error> {
        let mut path = self.repo.path("/events/stream");
        if let Some(cursor) = self.cursor {
            path.push_str(&format!("?cursor={cursor}"));
        }
        let accept = [("Accept", "text/event-stream".to_owned())];
        // The server sends a comment every 15 seconds.
        let response =
            self.repo
                .client
                .call("GET", &path, None, &accept, Duration::from_secs(45))?;
        if response.status != 200 {
            let status = response.status;
            let body = response.read_body(64 * 1024).unwrap_or_default();
            return Err(api_error(status, &body));
        }
        self.reader = Some(SseReader::new(BufReader::new(response.body)));
        Ok(())
    }
}

impl Iterator for EventStream {
    type Item = Result<FeedEntry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.reader.is_none() {
                if self.failures > 0 {
                    let delays = self
                        .backoff
                        .get_or_insert_with(|| reconnect_builder().build());
                    if let Some(delay) = delays.next() {
                        std::thread::sleep(delay);
                    }
                }
                match self.connect() {
                    Ok(()) => {}
                    // Refused outright (unauthorized, unknown repository):
                    // retrying cannot help.
                    Err(error @ Error::Api { status, .. }) if status < 500 => {
                        self.done = true;
                        return Some(Err(error));
                    }
                    Err(error) => {
                        self.failures += 1;
                        if self.failures > self.max_failures {
                            self.done = true;
                            return Some(Err(error));
                        }
                        continue;
                    }
                }
            }
            let reader = self.reader.as_mut().expect("connected above");
            match reader.next_event() {
                Ok(Some(event)) => match event.event.as_str() {
                    "open" => {
                        if let Some(id) = event.id.and_then(|id| id.parse().ok()) {
                            self.cursor = Some(id);
                        }
                    }
                    "activity" => match serde_json::from_str::<FeedEntry>(&event.data) {
                        Ok(entry) => {
                            self.failures = 0;
                            self.backoff = None;
                            if self.cursor.is_some_and(|c| entry.seq <= c) {
                                continue;
                            }
                            self.cursor = Some(entry.seq);
                            return Some(Ok(entry));
                        }
                        Err(e) => {
                            self.done = true;
                            return Some(Err(Error::Protocol(format!("feed entry: {e}"))));
                        }
                    },
                    _ => {}
                },
                // End of stream or a broken connection: resume by cursor.
                Ok(None) | Err(_) => {
                    self.reader = None;
                    self.failures += 1;
                    if self.failures > self.max_failures {
                        self.done = true;
                        return Some(Err(self
                            .repo
                            .client
                            .transport("the event stream kept disconnecting")));
                    }
                }
            }
        }
    }
}

// =====================================================================
// Storage: artifacts and scratch areas over HTTP (see `docs/storage.md`).
// Kept in its own section, with its wire types in `storage_api`, so this
// feature's client methods are easy to merge alongside unrelated work
// elsewhere in this crate (inbox messages, branch lifecycle).
//
// A caller's `path`, for `publish_artifact` and `read_artifact`, is
// always the *caller's own local file*: unlike a `--substrate-key`-style
// path, which names a file on the server, the bytes here travel over the
// HTTP call itself. `read_artifact` writes to a local path the same way
// local mode does; `publish_artifact` reads its bytes from one.
// =====================================================================

/// A query parameter's value, percent-encoded like every path segment.
fn push_query(query: &mut String, key: &str, value: &str) {
    query.push(if query.is_empty() { '?' } else { '&' });
    query.push_str(key);
    query.push('=');
    query.push_str(&encode(value));
}

impl Repo {
    fn artifacts_path(&self, branch: &str, rest: &str) -> String {
        self.branch_path(branch, &format!("/artifacts{rest}"))
    }

    fn scratch_path(&self, branch: &str, rest: &str) -> String {
        self.branch_path(branch, &format!("/scratch{rest}"))
    }

    /// Publish `bytes` as a new immutable artifact of `branch`, acting
    /// with the server's authority as a person, like
    /// `by artifact publish --branch`. `name` defaults to the digest;
    /// `media_type` to `application/octet-stream`.
    pub fn publish_artifact(
        &self,
        branch: &str,
        bytes: &[u8],
        name: Option<&str>,
        media_type: Option<&str>,
        labels: &[(String, String)],
        key: &str,
    ) -> Result<ArtifactRef, Error> {
        let mut query = String::new();
        if let Some(name) = name {
            push_query(&mut query, "name", name);
        }
        if let Some(media_type) = media_type {
            push_query(&mut query, "media_type", media_type);
        }
        for (k, v) in labels {
            push_query(&mut query, "label", &format!("{k}={v}"));
        }
        let path = format!("{}{query}", self.artifacts_path(branch, ""));
        self.client.post_raw(
            &path,
            bytes,
            media_type.unwrap_or("application/octet-stream"),
            key,
        )
    }

    /// Every artifact `branch` may read; like `by artifact list --branch`.
    pub fn artifacts(&self, branch: &str) -> Result<Vec<ArtifactRef>, Error> {
        Ok(self
            .client
            .get::<ArtifactList>(&self.artifacts_path(branch, ""))?
            .artifacts)
    }

    /// Artifact `id`'s provenance and bytes, verified against its recorded
    /// digest and size, for `branch`; like `by artifact get --branch`. The
    /// caller writes the bytes wherever it likes (unlike local mode's
    /// `--out`, which names a path resolved against the acting branch's
    /// own worktree, `out` here is never sent: only the bytes cross the
    /// wire, and the caller's own filesystem is its own).
    pub fn read_artifact(&self, branch: &str, id: &str) -> Result<(ArtifactRef, Vec<u8>), Error> {
        let meta: ArtifactRef = self
            .client
            .get(&self.artifacts_path(branch, &format!("/{}", encode(id))))?;
        let (headers, bytes) = self.client.get_binary(
            &self.artifacts_path(branch, &format!("/{}/content", encode(id))),
            MAX_BODY,
        )?;
        let digest = blake3::hash(&bytes).to_hex().to_string();
        let header_digest = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(DIGEST_HEADER))
            .map(|(_, v)| v.as_str());
        if bytes.len() as u64 != meta.size
            || digest != meta.digest
            || header_digest.is_some_and(|h| h != meta.digest)
        {
            return Err(Error::Protocol(format!(
                "downloaded artifact {id} does not match its recorded size or digest"
            )));
        }
        Ok((meta, bytes))
    }

    /// Share artifact `id`, readable by `branch`, with `to`; like
    /// `by artifact share --branch`.
    pub fn share_artifact(&self, branch: &str, id: &str, to: &str, key: &str) -> Result<(), Error> {
        self.client
            .post::<Ack>(
                &self.artifacts_path(branch, &format!("/{}/share", encode(id))),
                &ShareRequest { to: to.to_owned() },
                key,
            )
            .map(|_| ())
    }

    /// Create scratch area `name`, owned by `branch`; like
    /// `by scratch create --branch`.
    pub fn create_scratch(
        &self,
        branch: &str,
        name: &str,
        key: &str,
    ) -> Result<ScratchArea, Error> {
        self.client.post(
            &self.scratch_path(branch, ""),
            &CreateScratchRequest {
                name: name.to_owned(),
            },
            key,
        )
    }

    /// Every scratch area `branch` may reach; like
    /// `by scratch list --branch`.
    pub fn scratch_areas(&self, branch: &str) -> Result<Vec<ScratchArea>, Error> {
        Ok(self
            .client
            .get::<ScratchList>(&self.scratch_path(branch, ""))?
            .areas)
    }

    /// Share scratch area `name`, reachable by `branch`, with `to`; like
    /// `by scratch share --branch`.
    pub fn share_scratch(
        &self,
        branch: &str,
        name: &str,
        to: &str,
        key: &str,
    ) -> Result<(), Error> {
        self.client
            .post::<Ack>(
                &self.scratch_path(branch, &format!("/{}/share", encode(name))),
                &ShareRequest { to: to.to_owned() },
                key,
            )
            .map(|_| ())
    }

    /// Acquire scratch area `name`'s writer lock for `branch`; like
    /// `by scratch lock --branch`. Refused with `running` while another
    /// branch's turn holds it.
    pub fn lock_scratch(&self, branch: &str, name: &str, key: &str) -> Result<ScratchLock, Error> {
        self.client.post(
            &self.scratch_path(branch, &format!("/{}/lock", encode(name))),
            &Empty::default(),
            key,
        )
    }

    /// Release scratch area `name`'s lock if `branch` holds it; like
    /// `by scratch unlock --branch`.
    pub fn unlock_scratch(&self, branch: &str, name: &str, key: &str) -> Result<(), Error> {
        self.client
            .post::<Ack>(
                &self.scratch_path(branch, &format!("/{}/unlock", encode(name))),
                &Empty::default(),
                key,
            )
            .map(|_| ())
    }

    /// Scratch area `name`'s writer lock, if one is held, whether or not
    /// its turn is still running. Not scoped to a branch: like
    /// [`branchyard::Yard::scratch_lock_state`], it is not access
    /// controlled.
    pub fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error> {
        Ok(self
            .client
            .get::<LockState>(&self.path(&format!("/scratch/{}/lock", encode(name))))?
            .lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The waits are the ones the hand-written formulas gave before backon:
    /// `250 * 4^(attempt-1)` ms between a POST's attempts, and
    /// `min(250 << min(failures-1, 5), 5000)` ms before each reconnect.
    #[test]
    fn backoff_keeps_the_old_timing() {
        let posts: Vec<Duration> = post_backoff().build().collect();
        let old: Vec<Duration> = (1..POST_ATTEMPTS)
            .map(|attempt| Duration::from_millis(250 * 4u64.pow(attempt - 1)))
            .collect();
        assert_eq!(posts, old);
        assert_eq!(
            posts,
            [Duration::from_millis(250), Duration::from_millis(1000)]
        );
        let reconnects: Vec<Duration> = reconnect_backoff().take(12).collect();
        let old: Vec<Duration> = (1..=12u32)
            .map(|failures| {
                let backoff = 250u64 << (failures - 1).min(5);
                Duration::from_millis(backoff.min(5_000))
            })
            .collect();
        assert_eq!(reconnects, old);
    }

    #[test]
    fn keys_are_unique_hex() {
        let a = new_key();
        let b = new_key();
        assert_eq!(a.len(), 32);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_ne!(a, b);
        // A version 4 UUID: version nibble 4, variant 10xx.
        assert_eq!(&a[12..13], "4");
        assert!("89ab".contains(&a[16..17]), "{a}");
    }

    #[test]
    fn api_errors_fall_back_to_the_status() {
        let error = api_error(502, b"bad gateway");
        assert_eq!(error.code(), Some("http_502"));
        assert_eq!(
            error.to_string(),
            "the server answered HTTP 502: bad gateway"
        );
        let error = api_error(
            404,
            br#"{"error":{"code":"unknown_branch","message":"no branch named x"}}"#,
        );
        assert_eq!(error.code(), Some("unknown_branch"));
        assert_eq!(error.to_string(), "no branch named x");
    }

    #[test]
    fn debug_never_shows_the_token() {
        let client = Client::new("http://127.0.0.1:1", "s3cret").unwrap();
        assert!(!format!("{client:?}").contains("s3cret"));
        assert!(Client::new("http://127.0.0.1:1", "a\nb").is_err());
    }
}
