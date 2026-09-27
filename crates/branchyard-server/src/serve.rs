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
use crate::auth::Tokens;
use crate::config::{Config, TlsFiles};
use crate::feed::Feed;
use crate::ops::Registry;
use crate::store::FileStore;

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

/// Open every repository, the registry and the feeds, bind, and start
/// serving. Needs a multi-threaded Tokio runtime.
pub async fn start(config: Config) -> Result<Running, String> {
    config.validate()?;
    std::fs::create_dir_all(&config.data_dir)
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
    let grace = config.shutdown_grace;
    let app = Arc::new(App {
        repos,
        registry: registry.clone(),
        tokens: Tokens::new(config.tokens.clone()),
        config,
        shutdown: shutdown_rx.clone(),
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
    })
}

type Opened = (BTreeMap<String, RepoState>, Arc<Registry>);

fn open_state(config: &Config) -> Result<Opened, String> {
    let mut repos = BTreeMap::new();
    for (name, path) in &config.repos {
        let yard = Yard::open(path)
            .map_err(|e| format!("repository {name} at {}: {e}", path.display()))?;
        let feed_path = config.data_dir.join("feeds").join(format!("{name}.jsonl"));
        let feed = Feed::open(feed_path.clone(), yard.root())
            .map_err(|e| format!("feed {}: {e}", feed_path.display()))?;
        feed.sync()
            .map_err(|e| format!("reading the event logs of {name}: {e}"))?;
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
    let store_path = config.data_dir.join("operations.jsonl");
    let store = FileStore::open(&store_path)
        .map_err(|e| format!("operation registry {}: {e}", store_path.display()))?;
    let registry = Registry::open(Box::new(store), config.max_running)
        .map_err(|e| format!("operation registry {}: {e}", store_path.display()))?;
    Ok((repos, registry))
}

/// Ingest a repository's activity when the engine reports some, and
/// otherwise every `interval`, which picks up other processes' activity.
async fn poll(repo: RepoState, interval: Duration, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = repo.wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
        let feed = repo.feed.clone();
        match tokio::task::spawn_blocking(move || feed.sync()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("branchyard-server: feed of {}: {e}", repo.name),
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
