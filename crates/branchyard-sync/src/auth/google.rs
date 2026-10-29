//! Google access tokens through the official Application Default Credentials
//! provider. Fixed tokens and anonymous emulator access remain explicit choices.
//! A bounded worker owns the asynchronous provider and its runtime, so blocking
//! storage callers also work inside an existing Tokio runtime.

use std::sync::mpsc::{self, SyncSender};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use google_cloud_auth::credentials::{self, AccessTokenCredentials};
use serde_json::Value;

use crate::error::{Error, Result};

/// OAuth scope for Cloud Storage and Cloud KMS. IAM still authorizes each call.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE_CAPACITY: usize = 16;

#[derive(Clone)]
enum Source {
    Adc,
    Json(Value),
    Metadata(String),
    Fixed(String),
    Anonymous,
}

/// Blocking access to Google's official credential discovery and token cache.
/// Credential material is never included in its debug output or errors.
pub struct GoogleAuth {
    source: Source,
    worker: OnceLock<Result<Worker>>,
}

impl std::fmt::Debug for GoogleAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GoogleAuth([credentials redacted])")
    }
}

impl GoogleAuth {
    fn new(source: Source) -> Self {
        Self {
            source,
            worker: OnceLock::new(),
        }
    }

    /// Use a fixed access-token override, otherwise official ADC discovery.
    /// ADC files and metadata are consulted lazily on the first authenticated call.
    pub fn from_env() -> Result<Self> {
        Ok(match std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN") {
            Ok(token) if !token.trim().is_empty() => Self::fixed(&token),
            _ => Self::new(Source::Adc),
        })
    }

    /// Explicit ADC JSON, without consulting environment credential files.
    /// Supports external accounts, user accounts, impersonation and service accounts.
    pub fn from_json(value: Value) -> Self {
        Self::new(Source::Json(value))
    }

    /// Explicit metadata endpoint, including a hermetic emulator's `host:port`.
    pub fn from_metadata(host: &str) -> Self {
        Self::new(Source::Metadata(format!("http://{host}")))
    }

    /// No authentication, for explicitly configured emulators.
    pub fn anonymous() -> Self {
        Self::new(Source::Anonymous)
    }

    /// A pre-minted token. The caller owns its expiry and rotation.
    pub fn fixed(token: &str) -> Self {
        Self::new(Source::Fixed(token.to_owned()))
    }

    /// The bearer header, or `None` for anonymous access. The official provider
    /// owns the real-time expiry/cache policy; `now_ms` is retained for callers.
    pub fn header(&self, _now_ms: u64) -> Result<Option<String>> {
        match &self.source {
            Source::Anonymous => Ok(None),
            Source::Fixed(token) => Ok(Some(format!("Bearer {token}"))),
            _ => {
                let worker = self
                    .worker
                    .get_or_init(|| Worker::start(self.source.clone()))
                    .as_ref()
                    .map_err(Clone::clone)?;
                worker.token().map(|token| Some(format!("Bearer {token}")))
            }
        }
    }
}

struct Request {
    deadline: Instant,
    reply: SyncSender<Result<String>>,
}

struct Worker {
    sender: Option<tokio::sync::mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(source: Source) -> Result<Self> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Request>(QUEUE_CAPACITY);
        let thread = std::thread::Builder::new()
            .name("google-credentials".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => {
                        runtime.block_on(async move {
                        let credentials = build_credentials(source);
                        while let Some(request) = receiver.recv().await {
                            let result = match &credentials {
                                Ok(credentials) => {
                                    let remaining = request.deadline.saturating_duration_since(Instant::now());
                                    match tokio::time::timeout(remaining, credentials.access_token()).await {
                                        Ok(Ok(token)) => Ok(token.token),
                                        Ok(Err(error)) if error.is_transient() => Err(Error::transient("Google credential acquisition failed temporarily")),
                                        Ok(Err(_)) => Err(Error::refused("Google credential acquisition failed; check ADC configuration and IAM permissions")),
                                        Err(_) => Err(Error::transient("Google credential request timed out")),
                                    }
                                }
                                Err(error) => Err(error.clone()),
                            };
                            branchyard_support::best_effort("return Google credential result", request.reply.send(result));
                        }
                        });
                        runtime.shutdown_timeout(Duration::from_secs(1));
                    }
                    Err(_) => {
                        while let Some(request) = receiver.blocking_recv() {
                            branchyard_support::best_effort("return Google runtime error", request.reply.send(Err(Error::local("could not start the Google credential runtime"))));
                        }
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    fn token(&self) -> Result<String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| Error::local("Google credential worker stopped"))?
            .try_send(Request {
                deadline: Instant::now() + TIMEOUT,
                reply,
            })
            .map_err(|_| Error::transient("Google credential worker is busy or stopped"))?;
        result
            .recv_timeout(TIMEOUT)
            .map_err(|_| Error::transient("Google credential request timed out"))?
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("Google credential worker", thread);
        }
    }
}

fn build_credentials(source: Source) -> Result<AccessTokenCredentials> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        branchyard_support::best_effort(
            "initialize Google credential TLS",
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "TLS provider was initialized concurrently"),
        );
    }
    let built = match source {
        Source::Adc => credentials::Builder::default()
            .with_scopes([SCOPE])
            .build_access_token_credentials(),
        Source::Metadata(endpoint) => credentials::mds::Builder::default()
            .with_endpoint(endpoint)
            .with_scopes([SCOPE])
            .build_access_token_credentials(),
        Source::Json(value) => match value.get("type").and_then(Value::as_str) {
            Some("external_account") => credentials::external_account::Builder::new(value)
                .with_scopes([SCOPE])
                .build_access_token_credentials(),
            Some("authorized_user") => credentials::user_account::Builder::new(value)
                .with_scopes([SCOPE])
                .build_access_token_credentials(),
            Some("impersonated_service_account") => credentials::impersonated::Builder::new(value)
                .with_scopes([SCOPE])
                .build_access_token_credentials(),
            Some("service_account") => credentials::service_account::Builder::new(value)
                .with_access_specifier(credentials::service_account::AccessSpecifier::from_scopes(
                    [SCOPE],
                ))
                .build_access_token_credentials(),
            _ => return Err(Error::config("unsupported Google ADC credential type")),
        },
        Source::Anonymous | Source::Fixed(_) => {
            return Err(Error::config("this credential source needs no provider"))
        }
    };
    built.map_err(|_| Error::config("could not initialize Google Application Default Credentials"))
}
