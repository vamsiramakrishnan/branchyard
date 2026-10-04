//! Binding, TLS, the accept loop and graceful shutdown.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
use crate::ops::{Options, Registry};
use crate::store::{OperationStore, SqliteStore};
use crate::webhook;

/// How long open connections get to finish after shutdown begins, at
/// most; [`Running::wait`] also bounds it by the grace period.
const DRAIN: Duration = Duration::from_secs(10);
/// The least time connections get to drain, even with no grace period.
const MIN_DRAIN: Duration = Duration::from_secs(1);
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);

/// A server that is accepting connections.
pub struct Running {
    addr: SocketAddr,
    /// `--listen-unix`'s socket, removed when the server stops.
    unix: Option<PathBuf>,
    tls: bool,
    shutdown: watch::Sender<bool>,
    force: Arc<Notify>,
    registry: Arc<Registry>,
    grace: Duration,
    accept: tokio::task::JoinHandle<()>,
    pollers: Vec<tokio::task::JoinHandle<()>>,
    webhooks: Vec<tokio::task::JoinHandle<()>>,
    /// Held until the server has stopped: one server per data directory,
    /// unless its operations are in PostgreSQL, which several servers may
    /// share.
    _lock: Option<branchyard::DirLock>,
    worker: bool,
    /// The connector gateway run beside the server, stopped with it.
    _gateway: Option<branchyard::connectors::gateway::Supervisor>,
    /// `--metrics-addr`'s listener: its address and its accept loop.
    metrics: Option<(SocketAddr, tokio::task::JoinHandle<()>)>,
    /// One keeper per repository whose workspace has a warm pool this
    /// process's labels keep; stopped when shutdown begins.
    pools: Vec<branchyard::PoolKeeper>,
    /// This server's records in the fleet's registry and in each served
    /// repository's own (docs/registry.md); deregistered when shutdown
    /// begins.
    services: Vec<branchyard::services::Registration>,
    /// The replicators, stopped and drained once more after the last
    /// operation (docs/sync.md).
    sync: Option<Arc<crate::sync::ServerSync>>,
}

/// How a shutdown went.
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped {
    /// Operations recorded as interrupted because they outlasted the grace
    /// period.
    pub interrupted: usize,
}

impl Running {
    /// The bound address; for a worker, which binds none, the configured
    /// one.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Whether this only runs operations, with no HTTP listener.
    pub fn is_worker(&self) -> bool {
        self.worker
    }

    /// `http://` and the metrics listener's bound address, when
    /// `--metrics-addr` gave it one.
    pub fn metrics_url(&self) -> Option<String> {
        self.metrics
            .as_ref()
            .map(|(addr, _)| format!("http://{addr}/metrics"))
    }

    /// `http://` or `https://` and the bound address, or `unix:` and the
    /// socket's path.
    pub fn url(&self) -> String {
        if let Some(path) = &self.unix {
            return format!("unix:{}", path.display());
        }
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}", self.addr)
    }

    /// Begin shutting down: stop accepting connections and operations,
    /// end event streams, let requests in flight finish.
    pub fn shutdown(&self) {
        self.registry.stop_accepting();
        self.shutdown.send_replace(true);
    }

    /// A handle that can call [`Running::shutdown`] and `Running::force`
    /// from another task.
    pub fn handle(&self) -> Handle {
        Handle {
            shutdown: self.shutdown.clone(),
            force: self.force.clone(),
            registry: self.registry.clone(),
        }
    }

    /// Wait for shutdown to begin and finish. From the moment it begins,
    /// open connections drain and running operations get the grace period
    /// at the same time, so the whole takes at most the grace period (and
    /// at least `MIN_DRAIN` for requests in flight), which is what a
    /// supervisor's stop timeout, such as `docker stop`'s, must exceed.
    /// Operations still running after it are recorded as interrupted;
    /// their threads end with the process.
    pub async fn wait(self) -> Stopped {
        let mut begun = self.shutdown.subscribe();
        let _ = begun.wait_for(|stop| *stop).await;
        drop(self.pools);
        let services = self.services;
        let _ = tokio::task::spawn_blocking(move || drop(services)).await;
        for poller in self.pollers {
            poller.abort();
        }
        for webhook in self.webhooks {
            webhook.abort();
        }
        if let Some((_, metrics)) = self.metrics {
            metrics.abort();
        }
        let registry = self.registry.clone();
        let grace = self.grace;
        let idle = tokio::task::spawn_blocking(move || registry.wait_running(grace));
        // A connection that has not finished its request by then (a
        // client that sent half its headers) is dropped with the process.
        let drained = tokio::time::timeout(grace.max(MIN_DRAIN), self.accept);
        tokio::select! {
            _ = async { tokio::join!(idle, drained) } => {}
            _ = self.force.notified() => {}
        }
        let registry = self.registry.clone();
        let interrupted = tokio::task::spawn_blocking(move || registry.close())
            .await
            .unwrap_or(0);
        if let Some(sync) = self.sync {
            let _ = tokio::task::spawn_blocking(move || sync.finish()).await;
        }
        if let Some(path) = &self.unix {
            let _ = std::fs::remove_file(path);
        }
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
    // With PostgreSQL, operations, their queue and branch locks are in the
    // database, which several servers may share; otherwise they are in the
    // data directory's SQLite, which one server owns.
    let lock = match config.database {
        Some(_) => None,
        None => Some(
            branchyard::DirLock::acquire(&config.data_dir, "a Branchyard server")
                .map_err(|e| format!("data directory {}: {e}", config.data_dir.display()))?,
        ),
    };
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
    let gateway = match worker_or_not(&config) {
        true => None,
        false => crate::connectors::start(&config)?,
    };
    let worker = config.worker_only;
    let listener = match (worker, &config.listen_unix) {
        (true, _) => None,
        (false, Some(path)) => Some(Listener::Unix(bind_unix(path)?)),
        (false, None) => Some(Listener::Tcp(
            TcpListener::bind(config.listen)
                .await
                .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?,
        )),
    };
    let addr = match &listener {
        Some(Listener::Tcp(listener)) => listener.local_addr().map_err(|e| e.to_string())?,
        _ => config.listen,
    };
    let unix = match listener {
        Some(Listener::Unix(_)) => config.listen_unix.clone(),
        _ => None,
    };
    // A worker may serve metrics too: it has no other listener.
    let metrics_listener = match config.metrics.as_ref().and_then(|m| m.listen) {
        Some(at) => Some(
            TcpListener::bind(at)
                .await
                .map_err(|e| format!("cannot listen for metrics on {at}: {e}"))?,
        ),
        None => None,
    };
    let mut pollers: Vec<tokio::task::JoinHandle<()>> = repos
        .values()
        .map(|repo| {
            tokio::spawn(poll(
                repo.clone(),
                config.poll_interval,
                config.recover_interval,
                shutdown_rx.clone(),
            ))
        })
        .collect();
    let webhooks = match worker {
        true => Vec::new(),
        false => start_webhooks(
            &config,
            &repos,
            registry.observability(),
            shutdown_rx.clone(),
        )?,
    };
    let grace = config.shutdown_grace;
    let triggers = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            let store = crate::triggers::store::open(&config)?;
            // A data directory's store is this server's alone: any claim on
            // a pending run was its predecessor's.
            if config.database.is_none() {
                store
                    .release_claims()
                    .map_err(|e| format!("triggers: {e}"))?;
            }
            Ok::<_, String>(store)
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let base_url = format!("{}://{addr}", if tls.is_some() { "https" } else { "http" });
    let hub = Arc::new(crate::triggers::dispatch::Hub::new(
        triggers,
        config.triggers.clone(),
        base_url,
    ));
    let companion = match config.app.enabled && !config.worker_only {
        true => {
            let config = config.clone();
            Some(Arc::new(
                tokio::task::spawn_blocking(move || crate::companion::Companion::open(&config))
                    .await
                    .map_err(|e| e.to_string())??,
            ))
        }
        false => None,
    };
    let sync = {
        let (config, repos) = (config.clone(), repos.clone());
        tokio::task::spawn_blocking(move || crate::sync::ServerSync::open(&config, &repos))
            .await
            .map_err(|e| e.to_string())??
    };
    if let Some(sync) = &sync {
        sync.start()?;
    }
    let app = Arc::new(App {
        sync: sync.clone(),
        companion,
        repos,
        registry: registry.clone(),
        credentials: Credentials::new(config.all_credentials()),
        config,
        shutdown: shutdown_rx.clone(),
        storage_idem: crate::storage_routes::StorageIdem::default(),
        triggers: hub,
    });
    registry
        .start(Arc::new(crate::work::AppExecutor(app.clone())))
        .map_err(|e| format!("could not start the operation dispatcher: {e}"))?;
    let metrics = match metrics_listener {
        Some(listener) => {
            let at = listener.local_addr().map_err(|e| e.to_string())?;
            let router = api::metrics_router(app.clone());
            Some((
                at,
                tokio::spawn(accept_loop(
                    Listener::Tcp(listener),
                    None,
                    router,
                    shutdown_rx.clone(),
                )),
            ))
        }
        None => None,
    };
    // The companion's push notifications follow every repository's feed.
    if app.companion.as_ref().is_some_and(|c| c.vapid.is_some()) {
        let (store, _) = operation_store(&app.config)?;
        pollers.extend(crate::companion::push::spawn(
            app.clone(),
            Arc::from(store),
            shutdown_rx.clone(),
        ));
    }
    // Triggers fire wherever a dispatcher runs: this server or worker.
    if app.config.triggers.dispatch {
        pollers.push(crate::triggers::dispatch::spawn(
            app.clone(),
            shutdown_rx.clone(),
        ));
    }
    let pools = keep_pools(&app);
    let services = match worker {
        true => Vec::new(),
        false => {
            let app = app.clone();
            let url = match &unix {
                Some(path) => format!("unix:{}", path.display()),
                None => format!("{}://{addr}", if tls.is_some() { "https" } else { "http" }),
            };
            tokio::task::spawn_blocking(move || announce(&app, &url))
                .await
                .map_err(|e| e.to_string())?
        }
    };
    pollers.push(tokio::spawn(reap_services(
        app.clone(),
        app.config.recover_interval,
        shutdown_rx.clone(),
    )));
    let accept = match listener {
        Some(listener) => {
            let router = api::router(app);
            tokio::spawn(accept_loop(listener, tls.clone(), router, shutdown_rx))
        }
        None => {
            let mut shutdown = shutdown_rx;
            tokio::spawn(async move {
                let _app = app;
                let _ = shutdown.wait_for(|stop| *stop).await;
            })
        }
    };
    Ok(Running {
        addr,
        unix,
        tls: tls.is_some(),
        shutdown: shutdown_tx,
        force: Arc::new(Notify::new()),
        registry,
        grace,
        accept,
        pollers,
        webhooks,
        _lock: lock,
        worker,
        _gateway: gateway,
        metrics,
        pools,
        services,
        sync,
    })
}

/// A store that is the fleet registry of `Registry`'s operation store, for
/// a [`branchyard::services::Registration`] to renew in.
struct Fleet(Arc<Registry>);

impl branchyard::services::ServiceStore for Fleet {
    fn transact(
        &self,
        f: &mut (dyn FnMut(&mut dyn branchyard::services::Rows) -> std::io::Result<()> + Send),
    ) -> std::io::Result<()> {
        self.0.services().transact(f)
    }
}

/// Register this server in the fleet's registry and in each served
/// repository's own, so that a `by` on this machine finds it there
/// (`by services --kind server`). A record that cannot be written is
/// logged, never fatal.
fn announce(app: &Arc<App>, url: &str) -> Vec<branchyard::services::Registration> {
    use branchyard::services::{
        Clock, Endpoint, Registration, Service, ServiceOwner, DEFAULT_TTL, KIND_SERVER,
    };
    let endpoint = match url.strip_prefix("unix:") {
        Some(path) => Endpoint::Unix { path: path.into() },
        None => Endpoint::url(url),
    };
    let service = |owner: ServiceOwner| {
        Service::new(KIND_SERVER, owner)
            .with("version", env!("CARGO_PKG_VERSION"))
            .with("repos", app.repos.keys().cloned().collect::<Vec<_>>())
            .with("labels", app.config.labels.clone())
            .with("connectors", app.config.connectors.is_some())
            .with("well_known", "/.well-known/branchyard")
            .with_endpoint(endpoint.clone())
    };
    let mut held = Vec::new();
    match Registration::start(
        Arc::new(Fleet(app.registry.clone())),
        service(ServiceOwner::this_process()),
        DEFAULT_TTL,
        Clock::system(),
    ) {
        Ok(registration) => held.push(registration),
        Err(e) => tracing::warn!(error = %e, "registering this server in the fleet's registry"),
    }
    for repo in app.repos.values() {
        match repo.yard.register_service(
            service(ServiceOwner::this_process()).with("repo", repo.name.clone()),
            DEFAULT_TTL,
        ) {
            Ok(registration) => held.push(registration),
            Err(e) => tracing::warn!(
                repo = %repo.name,
                error = %e,
                "registering this server in the repository's registry"
            ),
        }
    }
    held
}

/// Every `every`, expire and reclaim the fleet registry's records whose
/// lease ran out (each served repository's own are reaped by its
/// recovery, in [`poll`]).
async fn reap_services(app: Arc<App>, every: Duration, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = shutdown.changed() => return,
        }
        let shared = app.clone();
        match tokio::task::spawn_blocking(move || crate::services_routes::reap(&shared)).await {
            Ok(Ok(reaped)) => {
                for r in reaped {
                    tracing::info!(service = %r.service.id, kind = %r.service.kind, outcome = ?r.outcome, "reaped a service");
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "reaping the fleet's services"),
            Err(e) => tracing::warn!(error = %e, "reaping the fleet's services"),
        }
    }
}

/// Keep the warm pool of every served repository whose workspace has one
/// and whose scripts may run here, when this process carries the pool's
/// labels: refilled after each claim here, and every
/// [`branchyard::POOL_KEEP_EVERY`]. The workspace is read again each time,
/// so a changed `[workspace.pool]` takes effect without a restart. See
/// docs/pools.md.
fn keep_pools(app: &Arc<App>) -> Vec<branchyard::PoolKeeper> {
    app.repos
        .values()
        .filter(|repo| app.config.allow_workspace_scripts.allows(&repo.name))
        .map(|repo| {
            let spec = {
                let app = app.clone();
                let name = repo.name.clone();
                move || {
                    let repo = app.repos.get(&name)?;
                    app.workspace(repo).ok().flatten().filter(|spec| {
                        spec.pool
                            .as_ref()
                            .is_some_and(|pool| pool.kept_by(&app.config.labels))
                    })
                }
            };
            let on_fill = {
                let metrics = app.registry.observability().metrics.clone();
                let name = repo.name.clone();
                move |filled: &branchyard::PoolFill| {
                    crate::metrics::record_fill(&metrics, &name, filled);
                    if let Some(error) = &filled.error {
                        tracing::warn!(repo = %name, error = %error, "filling the warm pool");
                    }
                }
            };
            repo.yard
                .keep_pool(spec, branchyard::POOL_KEEP_EVERY, on_fill)
        })
        .collect()
}

/// A worker runs operations only; the gateway runs beside a server.
fn worker_or_not(config: &Config) -> bool {
    config.worker_only
}

/// One delivery task per (repository, configured webhook), sharing a store
/// for their durable cursors and an HTTP client.
fn start_webhooks(
    config: &Config,
    repos: &BTreeMap<String, RepoState>,
    observability: &crate::observe::Observability,
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
            tracing::info!(
                webhook = %webhook.url,
                repo = %repo.name,
                cursor = %place,
                "notifying webhook of repo activity"
            );
            tasks.push(webhook::spawn_observed(
                repo.clone(),
                webhook.clone(),
                store.clone(),
                client.clone(),
                observability.clone(),
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
        // Only an operator's configuration lets a repository's scripts run;
        // see docs/workspace.md.
        if !config.allow_workspace_scripts.allows(name) {
            yard.deny_workspace_scripts();
        }
        if let Some(gateway) = crate::connectors::gateway_for(config, name) {
            yard.use_connectors(gateway);
        }
        // The model gateway its branches' turns may use, and each
        // principal's ceiling (docs/model-gateway.md).
        if let Some(gateway) = crate::models::gateway_for(config, name)
            .map_err(|e| format!("repository {name}: {e}"))?
        {
            yard.use_models(gateway);
        }
        yard.use_ceilings(config.ceilings.clone());
        // The administrator's locked approvals and the people's
        // (docs/effects.md#approvals).
        yard.use_approvals(config.approvals.clone());
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
    let options = Options {
        max_running: config.max_running,
        lease: config.operation_lease,
        poll: config.poll_interval,
        repos: config.repos.iter().map(|(name, _)| name.clone()).collect(),
        exclusive: config.database.is_none(),
        labels: config.labels.clone(),
        inventory: config.inventory.then(|| {
            config
                .inventory_source
                .clone()
                .unwrap_or_else(crate::ops::inventory_source)
        }),
        unclaimable_after: config.unclaimable_after,
        scheduling: config.scheduling(),
        observability: config
            .observability
            .clone()
            .unwrap_or_else(crate::observe::Observability::from_env),
    };
    let registry =
        Registry::open(store, options).map_err(|e| format!("operation registry {place}: {e}"))?;
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

/// How often, by default, the server looks for branches whose engine
/// stopped, such as a local `by run` that was killed, and for waiting
/// dependents whose prerequisites settled; [`Config::recover_interval`].
pub const RECOVER_EVERY: Duration = Duration::from_secs(30);

/// Publish a repository's feed head when the engine reports activity, and
/// otherwise every `interval`, which picks up other processes' activity.
/// Every `recover_every`, recover branches whose engine stopped and start
/// waiting dependents whose prerequisites settled.
async fn poll(
    repo: RepoState,
    interval: Duration,
    recover_every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut recovered = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = repo.wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
        let recover = recovered.elapsed() >= recover_every;
        if recover {
            recovered = tokio::time::Instant::now();
        }
        let (feed, yard) = (repo.feed.clone(), repo.yard.clone());
        let wake = repo.wake.clone();
        let polled = tokio::task::spawn_blocking(move || {
            let recovered = match recover {
                true => yard.recover().map(|r| r.len()),
                false => Ok(0),
            };
            // A dependent whose prerequisite settled while no engine here
            // could start it: with the options of the proposal that made
            // it, when this server applied that, else the default policy,
            // deny. See docs/graph.md.
            if recover {
                let options = branchyard::TaskOptions {
                    observer: Some(crate::api::observer(&wake)),
                    ..branchyard::TaskOptions::default()
                };
                match yard.resume_graph(&options) {
                    Ok(started) if started.is_empty() => {}
                    Ok(started) => tracing::info!(
                        started = %started.join(", "),
                        "started branch(es) whose prerequisites had settled"
                    ),
                    Err(e) => tracing::error!(error = %e, "resuming graphs"),
                }
            }
            // The gateway's newest calls, as connector_call events.
            if let Err(e) = yard.ingest_connector_audit() {
                tracing::warn!(error = %e, "reading the connector gateway's audit log");
            }
            // Effects whose outcome is unknown, looked up through the
            // gateway on the recovery interval (docs/effects.md).
            if recover {
                match yard.reconcile_effects() {
                    Ok(report) if !report.settled.is_empty() => tracing::info!(
                        settled = report.settled.len(),
                        still_unknown = report.unknown.len(),
                        "reconciled the effect ledger"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "reconciling the effect ledger"),
                }
            }
            (feed.sync(), recovered)
        })
        .await;
        match polled {
            Ok((synced, recovered)) => {
                if let Err(e) = synced {
                    tracing::error!(repo = %repo.name, error = %e, "syncing the feed");
                }
                match recovered {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(
                        repo = %repo.name,
                        recovered = n,
                        "recovered branch(es) whose engine stopped"
                    ),
                    Err(e) => tracing::error!(repo = %repo.name, error = %e, "recovering"),
                }
            }
            Err(_) => return,
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// What the server accepts connections on.
enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(not(unix))]
    #[allow(dead_code)]
    Unix(std::convert::Infallible),
}

/// Bind `--listen-unix`'s socket: its directory must already exist and be
/// private to its owner (no group or other access), a stale socket left
/// by a server that is gone is replaced, a live one is refused, and the
/// socket itself is made mode 0600.
#[cfg(unix)]
fn bind_unix(path: &Path) -> Result<tokio::net::UnixListener, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let shown = path.display();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| format!("--listen-unix {shown}: no directory"))?;
    let meta = std::fs::metadata(dir).map_err(|e| format!("--listen-unix {shown}: {e}"))?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "--listen-unix {shown}: {} must be a directory only its owner may enter              (chmod 700)",
            dir.display()
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(existing) if existing.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(format!(
                    "--listen-unix {shown}: a server already listens there"
                ));
            }
            std::fs::remove_file(path).map_err(|e| format!("--listen-unix {shown}: {e}"))?;
        }
        Ok(_) => return Err(format!("--listen-unix {shown}: exists and is not a socket")),
        Err(_) => {}
    }
    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|e| format!("cannot listen on {shown}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("--listen-unix {shown}: {e}"))?;
    Ok(listener)
}

#[cfg(not(unix))]
fn bind_unix(path: &Path) -> Result<std::convert::Infallible, String> {
    Err(format!(
        "--listen-unix {}: this platform has no Unix domain sockets",
        path.display()
    ))
}

/// A connection, over TCP or a Unix domain socket.
enum Accepted {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl Listener {
    async fn accept(&self) -> std::io::Result<Accepted> {
        match self {
            Listener::Tcp(listener) => listener.accept().await.map(|(tcp, _)| {
                let _ = tcp.set_nodelay(true);
                Accepted::Tcp(tcp)
            }),
            #[cfg(unix)]
            Listener::Unix(listener) => listener
                .accept()
                .await
                .map(|(unix, _)| Accepted::Unix(unix)),
            #[cfg(not(unix))]
            Listener::Unix(never) => match *never {},
        }
    }
}

async fn accept_loop(
    listener: Listener,
    tls: Option<TlsAcceptor>,
    router: axum::Router,
    mut shutdown: watch::Receiver<bool>,
) {
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                #[cfg(unix)]
                Ok(Accepted::Unix(unix)) => {
                    // Configuration refuses TLS with --listen-unix.
                    let service = TowerToHyperService::new(router.clone());
                    tokio::spawn(serve_connection(unix, service, graceful.watcher()));
                }
                Ok(Accepted::Tcp(tcp)) => {
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
                    tracing::error!(error = %e, "accept");
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn unix_sockets_need_a_private_directory_and_replace_only_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let refused = bind_unix(&open.join("by.sock")).err().unwrap();
        assert!(refused.contains("chmod 700"), "{refused}");

        let private = dir.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = private.join("by.sock");
        let listener = bind_unix(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let live = bind_unix(&path).err().unwrap();
        assert!(live.contains("already listens"), "{live}");
        drop(listener);
        // The file a stopped server left is replaced.
        assert!(bind_unix(&path).is_ok());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "not a socket").unwrap();
        assert!(bind_unix(&path).err().unwrap().contains("not a socket"));
    }
}
