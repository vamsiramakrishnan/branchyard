//! Binding, TLS, the accept loop and graceful shutdown.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use branchyard::Yard;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};
use tokio_rustls::TlsAcceptor;

use crate::api::{self, App, RepoState};
use crate::auth::Credentials;
use crate::config::{Config, TlsFiles};
use crate::feed::Feed;
use crate::ops::Registry;
use crate::store::{OperationStore, SqliteStore};
use crate::webhook;

/// How long open connections get to finish after shutdown begins.
const DRAIN: Duration = Duration::from_secs(10);
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);

/// A server that is accepting connections.
pub struct Running {
    addr: SocketAddr,
    tls: bool,
    shutdown: watch::Sender<bool>,
    force: Arc<Notify>,
    registry: Arc<Registry>,
    grace: Duration,
    accept: tokio::task::JoinHandle<()>,
    pollers: Vec<tokio::task::JoinHandle<()>>,
    webhooks: Vec<tokio::task::JoinHandle<()>>,
    /// Held until the server has stopped: one server per data directory.
    _lock: branchyard::DirLock,
}

/// How a shutdown went.
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped {
    /// Operations recorded as interrupted because they outlasted the grace
    /// period.
    pub interrupted: usize,
}

impl Running {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://` or `https://` and the bound address.
    pub fn url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}", self.addr)
    }

    /// Begin shutting down: stop accepting connections and operations,
    /// end event streams, let requests in flight finish.
    pub fn shutdown(&self) {
        self.registry.stop_accepting();
        self.shutdown.send_replace(true);
    }

    /// A handle that can call [`Running::shutdown`] and [`Running::force`]
    /// from another task.
    pub fn handle(&self) -> Handle {
        Handle {
            shutdown: self.shutdown.clone(),
            force: self.force.clone(),
            registry: self.registry.clone(),
        }
    }

    /// Wait for shutdown to finish: connections drained, then running
    /// operations given the grace period. Operations still running after
    /// it are recorded as interrupted; their threads end with the process.
    pub async fn wait(self) -> Stopped {
        let _ = self.accept.await;
        for poller in self.pollers {
            poller.abort();
        }
        for webhook in self.webhooks {
            webhook.abort();
        }
        let registry = self.registry.clone();
        let grace = self.grace;
        let idle = tokio::task::spawn_blocking(move || registry.wait_idle(grace));
        tokio::select! {
            _ = idle => {}
            _ = self.force.notified() => {}
        }
        let registry = self.registry.clone();
        let interrupted = tokio::task::spawn_blocking(move || registry.close())
            .await
            .unwrap_or(0);
        Stopped { interrupted }
    }
}

/// Controls a [`Running`] server from elsewhere, such as a signal handler.
#[derive(Clone)]
pub struct Handle {
    shutdown: watch::Sender<bool>,
    force: Arc<Notify>,
    registry: Arc<Registry>,
}

impl Handle {
    pub fn shutdown(&self) {
        self.registry.stop_accepting();
        self.shutdown.send_replace(true);
    }

    /// Stop waiting for running operations; they are recorded as
    /// interrupted.
    pub fn force(&self) {
        self.shutdown();
        self.force.notify_one();
    }
}

fn tls_acceptor(files: &TlsFiles) -> Result<TlsAcceptor, String> {
    let certs = CertificateDer::pem_file_iter(&files.cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("TLS certificate {}: {e}", files.cert.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "TLS certificate {} has no certificates",
            files.cert.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_file(&files.key)
        .map_err(|e| format!("TLS key {}: {e}", files.key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Open every repository (recovering branches whose engine stopped), the
/// registry and the feeds, bind, and start
/// serving. Needs a multi-threaded Tokio runtime.
pub async fn start(config: Config) -> Result<Running, String> {
    config.validate()?;
    // Installed once per process, idempotently: our own TLS acceptor
    // already picks `ring` explicitly, but the webhook client's `reqwest`
    // (built with `rustls-no-provider`, so as never to also pull in
    // `aws-lc-rs` and leave two providers linked) needs a default
    // installed before it is built.
    let _ = rustls::crypto::ring::default_provider().install_default();
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|e| format!("data directory {}: {e}", config.data_dir.display()))?;
    let lock = branchyard::DirLock::acquire(&config.data_dir, "a Branchyard server")
        .map_err(|e| format!("data directory {}: {e}", config.data_dir.display()))?;
    let tls = match &config.tls {
        Some(files) => Some(tls_acceptor(files)?),
        None => None,
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let setup = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || open_state(&config))
    };
    let (repos, registry) = setup.await.map_err(|e| e.to_string())??;
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let pollers = repos
        .values()
        .map(|repo| {
            tokio::spawn(poll(
                repo.clone(),
                config.poll_interval,
                shutdown_rx.clone(),
            ))
        })
        .collect();
    let webhooks = start_webhooks(&config, &repos, shutdown_rx.clone())?;
    let grace = config.shutdown_grace;
    let app = Arc::new(App {
        repos,
        registry: registry.clone(),
        credentials: Credentials::new(config.all_credentials()),
        config,
        shutdown: shutdown_rx.clone(),
        storage_idem: crate::storage_routes::StorageIdem::default(),
    });
    let router = api::router(app);
    let accept = tokio::spawn(accept_loop(listener, tls.clone(), router, shutdown_rx));
    Ok(Running {
        addr,
        tls: tls.is_some(),
        shutdown: shutdown_tx,
        force: Arc::new(Notify::new()),
        registry,
        grace,
        accept,
        pollers,
        webhooks,
        _lock: lock,
    })
}

/// One delivery task per (repository, configured webhook), sharing a store
/// for their durable cursors and an HTTP client.
fn start_webhooks(
    config: &Config,
    repos: &BTreeMap<String, RepoState>,
    shutdown: watch::Receiver<bool>,
) -> Result<Vec<tokio::task::JoinHandle<()>>, String> {
    if config.webhooks.is_empty() {
        return Ok(Vec::new());
    }
    let (store, place) = operation_store(config)?;
    let store: Arc<dyn OperationStore> = Arc::from(store);
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("webhook client: {e}"))?;
    let mut tasks = Vec::new();
    for repo in repos.values() {
        for webhook in &config.webhooks {
            eprintln!(
                "branchyard-server: notifying {} of {}'s activity, cursor in {place}",
                webhook.url, repo.name
            );
            tasks.push(webhook::spawn(
                repo.clone(),
                webhook.clone(),
                store.clone(),
                client.clone(),
                shutdown.clone(),
            ));
        }
    }
    Ok(tasks)
}

type Opened = (BTreeMap<String, RepoState>, Arc<Registry>);

fn open_state(config: &Config) -> Result<Opened, String> {
    let mut repos = BTreeMap::new();
    for (name, path) in &config.repos {
        let yard = open_yard(config, name, path)
            .map_err(|e| format!("repository {name} at {}: {e}", path.display()))?;
        let feed = Feed::open(yard.clone())
            .map_err(|e| format!("reading the event feed of {name}: {e}"))?;
        repos.insert(
            name.clone(),
            RepoState {
                name: name.clone(),
                yard,
                feed: Arc::new(feed),
                wake: Arc::new(Notify::new()),
            },
        );
    }
    let (store, place) = operation_store(config)?;
    let registry = Registry::open(store, config.max_running)
        .map_err(|e| format!("operation registry {place}: {e}"))?;
    Ok((repos, registry))
}

/// A served repository, with its state in the configured database or in
/// its own `.branchyard/state.db`.
fn open_yard(config: &Config, name: &str, path: &std::path::Path) -> Result<Yard, String> {
    match &config.database {
        None => Yard::open(path).map_err(|e| e.to_string()),
        #[cfg(feature = "postgres")]
        Some(url) => Yard::open_postgres(path, url, name).map_err(|e| e.to_string()),
        #[cfg(not(feature = "postgres"))]
        Some(_) => {
            let _ = name;
            Err(NO_POSTGRES.into())
        }
    }
}

#[cfg(not(feature = "postgres"))]
const NO_POSTGRES: &str = "this build has no PostgreSQL support; build by or \
     branchyard-server with the postgres feature to use --database";

type Store = Box<dyn crate::store::OperationStore>;

/// The operation registry's store, and where it is for messages.
fn operation_store(config: &Config) -> Result<(Store, String), String> {
    #[cfg(feature = "postgres")]
    if let Some(url) = &config.database {
        let place = "in the database".to_owned();
        let store = crate::store::PostgresStore::open(url)
            .map_err(|e| format!("operation registry {place}: {e}"))?;
        return Ok((Box::new(store), place));
    }
    #[cfg(not(feature = "postgres"))]
    if config.database.is_some() {
        return Err(NO_POSTGRES.into());
    }
    let store_path = config.data_dir.join("state.db");
    let place = store_path.display().to_string();
    let legacy = config.data_dir.join("operations.jsonl");
    let store = SqliteStore::open(&store_path, Some(&legacy))
        .map_err(|e| format!("operation registry {place}: {e}"))?;
    Ok((Box::new(store), place))
}

/// How often the server looks for branches whose engine stopped, such as a
/// local `by run` that was killed.
const RECOVER_EVERY: Duration = Duration::from_secs(30);

/// Publish a repository's feed head when the engine reports activity, and
/// otherwise every `interval`, which picks up other processes' activity.
/// Every [`RECOVER_EVERY`], recover branches whose engine stopped.
async fn poll(repo: RepoState, interval: Duration, mut shutdown: watch::Receiver<bool>) {
    let mut recovered = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = repo.wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
        let recover = recovered.elapsed() >= RECOVER_EVERY;
        if recover {
            recovered = tokio::time::Instant::now();
        }
        let (feed, yard) = (repo.feed.clone(), repo.yard.clone());
        let polled = tokio::task::spawn_blocking(move || {
            let recovered = match recover {
                true => yard.recover().map(|r| r.len()),
                false => Ok(0),
            };
            (feed.sync(), recovered)
        })
        .await;
        match polled {
            Ok((synced, recovered)) => {
                if let Err(e) = synced {
                    eprintln!("branchyard-server: feed of {}: {e}", repo.name);
                }
                match recovered {
                    Ok(0) => {}
                    Ok(n) => eprintln!(
                        "branchyard-server: recovered {n} branch(es) of {} whose engine stopped",
                        repo.name
                    ),
                    Err(e) => eprintln!("branchyard-server: recovering {}: {e}", repo.name),
                }
            }
            Err(_) => return,
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    router: axum::Router,
    mut shutdown: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((tcp, _)) => {
                    let _ = tcp.set_nodelay(true);
                    let service = TowerToHyperService::new(router.clone());
                    let watcher = graceful.watcher();
                    match tls.clone() {
                        None => {
                            tokio::spawn(serve_connection(tcp, service, watcher));
                        }
                        Some(acceptor) => {
                            tokio::spawn(async move {
                                let handshake =
                                    tokio::time::timeout(TLS_HANDSHAKE, acceptor.accept(tcp)).await;
                                if let Ok(Ok(stream)) = handshake {
                                    serve_connection(stream, service, watcher).await;
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    // Such as too many open files: back off rather than spin.
                    eprintln!("branchyard-server: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(DRAIN) => {}
    }
}

async fn serve_connection<I>(io: I, service: TowerToHyperService<axum::Router>, watcher: Watcher)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    let connection = builder.serve_connection(TokioIo::new(io), service);
    let _ = watcher.watch(connection).await;
}
