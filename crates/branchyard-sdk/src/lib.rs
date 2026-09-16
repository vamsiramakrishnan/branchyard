//! Thin remote client. Dropping a client/future does not cancel accepted work.
//! No model loop, subprocess launcher, sandbox implementation or automatic retries.
pub use branchyard_protocol as protocol;
pub use protocol::*;
use reqwest::{header, Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(#[from] Invalid),
    #[error("{0}")]
    Configuration(&'static str),
    #[error("transport failed; response unavailable")]
    Transport,
    #[error("invalid or incompatible server response")]
    Protocol,
    #[error("server response exceeds 1 MiB")]
    ResponseTooLarge,
    #[error("server returned HTTP {status}")]
    Http { status: u16 },
    #[error("submission outcome unknown; reconcile operation {operation_id} with request {request_sha256}")]
    SubmissionUnknown {
        operation_id: OperationId,
        request_sha256: String,
    },
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
}

pub struct ClientOptions {
    pub timeout: Duration,
    /// Enables HTTP only for literal loopback IP addresses, for contract fixtures/tunnels.
    pub allow_loopback_http: bool,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            allow_loopback_http: false,
        }
    }
}
impl Client {
    pub fn new(endpoint: &str, token: &str) -> Result<Self, Error> {
        Self::with_options(endpoint, token, ClientOptions::default())
    }
    pub fn with_options(
        endpoint: &str,
        token: &str,
        options: ClientOptions,
    ) -> Result<Self, Error> {
        let mut base =
            Url::parse(endpoint).map_err(|_| Error::Configuration("invalid endpoint URL"))?;
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::Configuration(
                "endpoint must not contain credentials, query, or fragment",
            ));
        }
        let loopback = base
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if base.scheme() != "https"
            && !(base.scheme() == "http" && options.allow_loopback_http && loopback)
        {
            return Err(Error::Configuration(
                "HTTPS required; fixtures may explicitly enable literal-loopback HTTP",
            ));
        }
        if options.timeout.is_zero() || options.timeout > Duration::from_secs(300) {
            return Err(Error::Configuration("timeout must be in (0, 300] seconds"));
        }
        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);
        if token.is_empty() || token.len() > 8192 {
            return Err(Error::Configuration("a bounded bearer token is required"));
        }
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Configuration("invalid bearer token"))?;
        auth.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(options.timeout)
            .connect_timeout(options.timeout.min(Duration::from_secs(10)))
            .pool_max_idle_per_host(8)
            .user_agent(concat!("branchyard-sdk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Error::Configuration("cannot initialize HTTP client"))?;
        Ok(Self { http, base })
    }
    async fn request<T: DeserializeOwned>(
        &self,
        path: &str,
        command: Option<(&Command, Vec<u8>)>,
    ) -> Result<T, Error> {
        let url = self
            .base
            .join(path)
            .map_err(|_| Error::Configuration("invalid API path"))?;
        let mut request = self.http.request(
            if command.is_some() {
                Method::POST
            } else {
                Method::GET
            },
            url,
        );
        if let Some((command, bytes)) = command {
            request = request
                .header("Idempotency-Key", command.operation_id.to_string())
                .header("X-Branchyard-Request-Sha256", sha256(&bytes))
                .header(header::CONTENT_TYPE, "application/json")
                .body(bytes);
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http {
                status: status.as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Error::ResponseTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(Error::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }
    pub async fn info(&self) -> Result<ServerInfo, Error> {
        self.request("v1alpha1/info", None).await
    }
    /// Send once. Persist the command before calling. An uncertain result is never a reason to mint a new ID.
    pub async fn submit(&self, command: &Command) -> Result<Receipt, Error> {
        let bytes = command.canonical_bytes()?;
        let fingerprint = sha256(&bytes);
        let unknown = || Error::SubmissionUnknown {
            operation_id: command.operation_id,
            request_sha256: fingerprint.clone(),
        };
        let receipt: Receipt = match self
            .request("v1alpha1/commands", Some((command, bytes)))
            .await
        {
            Ok(receipt) => receipt,
            // These statuses are definitive rejections in this contract. Other failures can be post-commit.
            Err(
                e @ Error::Http {
                    status: 400 | 401 | 403 | 404 | 409 | 413 | 422 | 429,
                },
            ) => return Err(e),
            Err(_) => return Err(unknown()),
        };
        if receipt.operation_id != command.operation_id || receipt.request_sha256 != fingerprint {
            return Err(unknown());
        }
        Ok(receipt)
    }
    pub async fn operation(&self, id: OperationId) -> Result<Operation, Error> {
        let op: Operation = self
            .request(&format!("v1alpha1/operations/{id}"), None)
            .await?;
        if op.operation_id != id
            || op.request_sha256.len() != 64
            || !op
                .request_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::Protocol);
        }
        Ok(op)
    }
    /// Reconcile exactly the saved command, rejecting a reused ID with different input.
    pub async fn reconcile(&self, command: &Command) -> Result<Operation, Error> {
        let fingerprint = command.fingerprint()?;
        let op = self.operation(command.operation_id).await?;
        if op.request_sha256 != fingerprint {
            return Err(Error::Protocol);
        }
        Ok(op)
    }
    pub async fn task(&self, id: TaskId) -> Result<Task, Error> {
        let task: Task = self.request(&format!("v1alpha1/tasks/{id}"), None).await?;
        if task.task_id != id {
            return Err(Error::Protocol);
        }
        Ok(task)
    }
    pub async fn events(&self, id: TaskId, after: u64, limit: u16) -> Result<EventPage, Error> {
        if limit == 0 || limit > MAX_EVENT_PAGE {
            return Err(Invalid("event page limit must be 1..100").into());
        }
        let page: EventPage = self
            .request(
                &format!("v1alpha1/tasks/{id}/events?after={after}&limit={limit}"),
                None,
            )
            .await?;
        if page.task_id != id || page.events.len() > limit as usize {
            return Err(Error::Protocol);
        }
        let mut cursor = after;
        for event in &page.events {
            if event.sequence <= cursor
                || event.summary.len() > 1024
                || event.kind.len() > 128
                || event.artifacts.len() > 32
            {
                return Err(Error::Protocol);
            }
            cursor = event.sequence;
        }
        if page.next_after != cursor {
            return Err(Error::Protocol);
        }
        Ok(page)
    }
}

/// HTTP 404 on reconciliation means "not visible now", not proof that no submission can commit.
pub fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::Http { status } if *status == StatusCode::NOT_FOUND.as_u16())
}
