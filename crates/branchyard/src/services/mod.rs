//! The service registry: what Branchyard starts or depends on (a connector
//! gateway, an egress proxy, a server, a worker, a sandbox, a recipe
//! machine, a pool keeper, an MCP server, a model gateway) announces itself
//! here with its capabilities, endpoints, owner, health and a lease, and is
//! found by what it can do rather than by a URL in a file. See
//! `docs/registry.md`.
//!
//! - **Records.** A [`Service`] is one row: its kind (a label such as
//!   `connector_gateway`), typed [`Capability`] values, [`Endpoint`]s, the
//!   [`ServiceOwner`] that registered it (a process, and the branch or
//!   operation it serves), its [`Health`], a weight, and how to reclaim what
//!   Branchyard started for it ([`Reclaim`]), if anything.
//! - **Leases.** A record is live until its lease runs out. Its owner renews
//!   it ([`Registration`] does this from a thread of its own) and
//!   deregisters it when done. A record whose lease ran out, or whose owner
//!   process is known to be gone from this host, is marked expired by
//!   [`expire`], and [`reap`] reclaims it through the owner-specific
//!   reaper: a leaked process is stopped, a sandbox or recipe machine is
//!   recovered and destroyed through the engine's own recovery, pool slots
//!   are reclaimed as recovery reclaims them. A record with no [`Reclaim`]
//!   (something Branchyard adopted, or that lives inside its owner) is only
//!   marked; nothing is ever reaped that Branchyard did not start.
//! - **Resolution.** [`resolve`] picks, among the live records of a kind
//!   whose capabilities hold what the query requires, the healthiest, then
//!   the heaviest, then the first by ID, so every caller picks the same one.
//! - **Watching.** Every change takes the next value of one counter, kept
//!   in the record as `seq`; [`ServiceStore::since`] returns what changed
//!   after a value, and [`watch`] waits for it.
//! - **Stores.** [`LocalRegistry`] is one SQLite file per repository
//!   (`.branchyard/registry.db`, mode 0600) that every `by` process on the
//!   machine shares; `BRANCHYARD_REGISTRY` names another file, to share one
//!   registry between repositories. A server keeps its fleet's records in
//!   its operation store, in SQLite or PostgreSQL, through the same
//!   operations ([`sqlite`], [`pg`]).
//!
//! Times are milliseconds since the Unix epoch, from the caller's
//! [`Clock`], so a test can move time without waiting.

pub mod conformance;
#[cfg(feature = "postgres")]
pub mod pg;
pub(crate) mod reclaim;
pub mod sqlite;

use branchyard_support::{CondvarExt as _, LockExt as _};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use sqlite::LocalRegistry;

/// Anvil's connector gateway (`by gateway start`, or a server's).
pub const KIND_CONNECTOR_GATEWAY: &str = "connector_gateway";
/// A model gateway: a Branchyard-run reverse proxy for model APIs.
pub const KIND_MODEL_GATEWAY: &str = "model_gateway";
/// A turn's egress proxy.
pub const KIND_EGRESS_PROXY: &str = "egress_proxy";
/// A Branchyard server (`by serve`).
pub const KIND_SERVER: &str = "server";
/// A process that claims queued operations (`by worker`, or a server).
pub const KIND_WORKER: &str = "worker";
/// A sandbox a turn runs in (Microsandbox, Substrate).
pub const KIND_SANDBOX: &str = "sandbox";
/// A machine an environment recipe made for a turn.
pub const KIND_RECIPE_MACHINE: &str = "recipe_machine";
/// An MCP server Branchyard runs.
pub const KIND_MCP_SERVER: &str = "mcp_server";
/// A thread keeping a warm pool filled.
pub const KIND_POOL_KEEPER: &str = "pool_keeper";

/// How long a lease lasts without renewal, unless the registrant says.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);
/// The shortest lease a registrant may ask for.
pub const MIN_TTL: Duration = Duration::from_secs(1);
/// The longest lease a registrant may ask for.
pub const MAX_TTL: Duration = Duration::from_secs(24 * 3600);
/// How long a record that left or was reclaimed stays listed (for `by
/// services` and watchers) before [`prune`] removes it.
pub const KEEP_ENDED: Duration = Duration::from_secs(3600);
/// How long a reclaim may stay under way before another reaper takes it
/// over (its reaper stopped mid-way).
pub const RECLAIM_TIMEOUT: Duration = Duration::from_secs(600);
/// The variable naming a registry file to use instead of a repository's
/// own `.branchyard/registry.db`.
pub const ENV_REGISTRY: &str = "BRANCHYARD_REGISTRY";

/// One typed capability value.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Capability {
    Flag(bool),
    Number(i64),
    Text(String),
    List(Vec<String>),
}

impl Capability {
    /// Whether this value, as a record carries it, satisfies `wanted`, as a
    /// query asks for it: equal; or, for a list, holding the wanted text or
    /// every wanted item.
    pub fn satisfies(&self, wanted: &Capability) -> bool {
        match (self, wanted) {
            (Capability::List(have), Capability::Text(want)) => have.contains(want),
            (Capability::List(have), Capability::List(want)) => {
                want.iter().all(|w| have.contains(w))
            }
            (have, want) => have == want,
        }
    }

    /// The value as one line for people.
    pub fn describe(&self) -> String {
        match self {
            Capability::Flag(b) => b.to_string(),
            Capability::Number(n) => n.to_string(),
            Capability::Text(t) => t.clone(),
            Capability::List(items) => items.join(","),
        }
    }

    /// Parse `text` as a query writes it: `true`/`false`, an integer, or
    /// text.
    pub fn parse(text: &str) -> Capability {
        match text {
            "true" => Capability::Flag(true),
            "false" => Capability::Flag(false),
            _ => match text.parse::<i64>() {
                Ok(n) => Capability::Number(n),
                Err(_) => Capability::Text(text.to_owned()),
            },
        }
    }
}

impl From<&str> for Capability {
    fn from(text: &str) -> Capability {
        Capability::Text(text.to_owned())
    }
}

impl From<String> for Capability {
    fn from(text: String) -> Capability {
        Capability::Text(text)
    }
}

impl From<bool> for Capability {
    fn from(flag: bool) -> Capability {
        Capability::Flag(flag)
    }
}

impl From<i64> for Capability {
    fn from(n: i64) -> Capability {
        Capability::Number(n)
    }
}

impl From<Vec<String>> for Capability {
    fn from(items: Vec<String>) -> Capability {
        Capability::List(items)
    }
}

/// Where a service is reached.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Endpoint {
    /// An `http://` or `https://` URL.
    Url { url: String },
    /// A Unix domain socket.
    Unix { path: PathBuf },
    /// Inside its owner process: reached through it, not by address.
    InProcess { name: String },
}

impl Endpoint {
    pub fn url(url: impl Into<String>) -> Endpoint {
        Endpoint::Url { url: url.into() }
    }

    /// One line for people.
    pub fn describe(&self) -> String {
        match self {
            Endpoint::Url { url } => url.clone(),
            Endpoint::Unix { path } => format!("unix:{}", path.display()),
            Endpoint::InProcess { name } => format!("in-process:{name}"),
        }
    }
}

/// Who registered a service and keeps its lease: a process, and what it
/// serves.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceOwner {
    /// Unique per registrant: only it renews or deregisters the record.
    pub id: String,
    /// Host and boot, as the engine names a lease's holder; empty for a
    /// registrant on another machine (one that registered over HTTP).
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub pid: u32,
    /// The process's start time, so a reused pid is never mistaken for it.
    #[serde(default)]
    pub start: String,
    /// The branch it serves, when it serves one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The server operation it serves, when it serves one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// The server principal that registered it over HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl ServiceOwner {
    /// This process, under a fresh registrant ID.
    pub fn this_process() -> ServiceOwner {
        let (host, pid, start) = crate::process_identity();
        ServiceOwner {
            id: fresh_id("o"),
            host,
            pid,
            start,
            branch: None,
            operation: None,
            principal: None,
        }
    }

    /// A registrant elsewhere, known only by the principal it
    /// authenticated as: its lease is all that says it is alive.
    pub fn remote(id: impl Into<String>, principal: impl Into<String>) -> ServiceOwner {
        ServiceOwner {
            id: id.into(),
            host: String::new(),
            pid: 0,
            start: String::new(),
            branch: None,
            operation: None,
            principal: Some(principal.into()),
        }
    }

    pub fn for_branch(mut self, branch: &str) -> ServiceOwner {
        self.branch = Some(branch.to_owned());
        self
    }

    /// Whether its process is known to be gone: it ran on this host and
    /// boot, and is not running now.
    pub fn gone(&self) -> bool {
        self.pid != 0 && crate::process_gone(&self.host, self.pid, &self.start)
    }
}

/// How a service says it is doing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    #[default]
    Healthy,
    /// Working, but worse than it should: resolved after healthy ones.
    Degraded,
    /// Not ready yet: resolved only when nothing better is.
    Starting,
    /// Never resolved.
    Unhealthy,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Starting => "starting",
            Health::Unhealthy => "unhealthy",
        }
    }
}

/// Where a record is in its life.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    /// Registered; live while its lease lasts.
    Live,
    /// Deregistered by its owner: nothing to reclaim.
    Left,
    /// Its lease ran out, or its owner is gone; reclaim pending.
    Expired,
    /// A reaper is reclaiming it.
    Reclaiming,
    /// Reclaimed, or there was nothing to reclaim.
    Reclaimed,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Live => "live",
            ServiceState::Left => "left",
            ServiceState::Expired => "expired",
            ServiceState::Reclaiming => "reclaiming",
            ServiceState::Reclaimed => "reclaimed",
        }
    }

    pub fn parse(text: &str) -> io::Result<ServiceState> {
        Ok(match text {
            "live" => ServiceState::Live,
            "left" => ServiceState::Left,
            "expired" => ServiceState::Expired,
            "reclaiming" => ServiceState::Reclaiming,
            "reclaimed" => ServiceState::Reclaimed,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown service state {other:?}"),
                ))
            }
        })
    }

    /// Whether nothing more happens to the record but pruning.
    pub fn ended(self) -> bool {
        matches!(self, ServiceState::Left | ServiceState::Reclaimed)
    }
}

/// A process group: its ID, and its leader's start time.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessGroup {
    pub pgid: u32,
    pub leader_start: String,
}

/// What Branchyard started for a service, and so reclaims when its lease
/// expires. Only ever recorded by Branchyard itself for what it started:
/// a record registered over a server's API never carries one.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reclaim {
    /// A process this host started: stopped only while its pid still has
    /// this start time; with a group, what is left of the group too.
    Process {
        host: String,
        pid: u32,
        start: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<ProcessGroup>,
    },
    /// A sandbox or recipe machine a branch's turn made: recovered through
    /// the engine's recovery (which brings its work back), then destroyed
    /// through its provider unless kept for the branch's next turn.
    Sandbox {
        root: PathBuf,
        branch: String,
        provider: Box<crate::Provider>,
        sandbox: String,
    },
    /// The warm pool slots of the repository at `root`: reclaimed as
    /// recovery reclaims them (a slot whose filler is gone is removed).
    PoolSlots { root: PathBuf },
}

impl Reclaim {
    /// The running process `pid` on this host, as it is now (its start
    /// time read, so a later reuse of the pid is never mistaken for it);
    /// with `group`, the process group it leads too. `None` when no such
    /// process runs.
    pub fn process(pid: u32, group: bool) -> Option<Reclaim> {
        let start = crate::proc::start_time(pid)?;
        Some(Reclaim::Process {
            host: crate::proc::host().to_owned(),
            pid,
            group: group.then(|| ProcessGroup {
                pgid: pid,
                leader_start: start.clone(),
            }),
            start,
        })
    }

    /// The process group this process leads, if it leads one (a
    /// supervisor started in a group of its own): what a reaper stops what
    /// is left of once this process is gone.
    pub fn own_group() -> Option<ProcessGroup> {
        let pid = std::process::id();
        (crate::proc::group(pid)? == pid).then(|| ProcessGroup {
            pgid: pid,
            leader_start: crate::proc::own_start().to_owned(),
        })
    }

    /// One line for people.
    pub fn describe(&self) -> String {
        match self {
            Reclaim::Process { pid, group, .. } => match group {
                Some(group) => format!("process {pid} (group {})", group.pgid),
                None => format!("process {pid}"),
            },
            Reclaim::Sandbox {
                branch, sandbox, ..
            } => format!("sandbox {sandbox} of branch {branch}"),
            Reclaim::PoolSlots { root } => format!("pool slots of {}", root.display()),
        }
    }
}

/// One registered service.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub capabilities: BTreeMap<String, Capability>,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    pub owner: ServiceOwner,
    #[serde(default)]
    pub health: Health,
    /// Among equally healthy records, heavier ones are resolved first.
    #[serde(default = "one")]
    pub weight: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaim: Option<Reclaim>,
    pub state: ServiceState,
    #[serde(default)]
    pub registered_ms: u64,
    #[serde(default)]
    pub renewed_ms: u64,
    #[serde(default)]
    pub lease_until_ms: u64,
    /// When its state last changed.
    #[serde(default)]
    pub changed_ms: u64,
    /// The registry's change counter at its last change.
    #[serde(default)]
    pub seq: u64,
    /// Why it expired, or what reclaiming it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn one() -> u32 {
    1
}

/// A fresh identifier: `prefix` and 20 random hex characters.
pub fn fresh_id(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let random = crate::connectors::keys::random_id().unwrap_or_else(|_| {
        format!(
            "{:x}{:x}{:x}",
            std::process::id(),
            system_ms(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    });
    let hex: String = blake3::hash(random.as_bytes()).to_hex()[..20].to_owned();
    format!("{prefix}_{hex}")
}

impl Service {
    /// A live record of `kind` owned by `owner`, under a fresh ID, healthy,
    /// with weight 1 and nothing to reclaim. Its lease is set when it is
    /// registered.
    pub fn new(kind: &str, owner: ServiceOwner) -> Service {
        Service {
            id: fresh_id(kind),
            kind: kind.to_owned(),
            capabilities: BTreeMap::new(),
            endpoints: Vec::new(),
            owner,
            health: Health::Healthy,
            weight: 1,
            reclaim: None,
            state: ServiceState::Live,
            registered_ms: 0,
            renewed_ms: 0,
            lease_until_ms: 0,
            changed_ms: 0,
            seq: 0,
            note: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Service {
        self.id = id.into();
        self
    }

    pub fn with(mut self, key: &str, value: impl Into<Capability>) -> Service {
        self.capabilities.insert(key.to_owned(), value.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: Endpoint) -> Service {
        self.endpoints.push(endpoint);
        self
    }

    pub fn with_reclaim(mut self, reclaim: Reclaim) -> Service {
        self.reclaim = Some(reclaim);
        self
    }

    pub fn with_health(mut self, health: Health) -> Service {
        self.health = health;
        self
    }

    pub fn with_weight(mut self, weight: u32) -> Service {
        self.weight = weight;
        self
    }

    pub fn capability(&self, key: &str) -> Option<&Capability> {
        self.capabilities.get(key)
    }

    /// A text capability's value.
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.capabilities.get(key) {
            Some(Capability::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// Its first URL endpoint.
    pub fn url(&self) -> Option<&str> {
        self.endpoints.iter().find_map(|e| match e {
            Endpoint::Url { url } => Some(url.as_str()),
            _ => None,
        })
    }

    /// Whether it is live at `now`: registered, and its lease not run out.
    pub fn live_at(&self, now_ms: u64) -> bool {
        self.state == ServiceState::Live && self.lease_until_ms > now_ms
    }

    /// Whether its kind and capabilities answer `query` (whatever its
    /// state).
    pub fn answers(&self, query: &Query) -> bool {
        query.kind.as_deref().is_none_or(|k| k == self.kind)
            && query.require.iter().all(|(key, wanted)| {
                self.capabilities
                    .get(key)
                    .is_some_and(|have| have.satisfies(wanted))
            })
    }

    /// Refuse a record no registry should hold.
    pub fn check(&self) -> Result<(), String> {
        check_kind(&self.kind)?;
        if self.id.is_empty()
            || self.id.len() > 200
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        {
            return Err(format!(
                "service id {:?} must be 1 to 200 of a-z, A-Z, 0-9, '_', '-', '.' and ':'",
                self.id
            ));
        }
        if self.owner.id.is_empty() {
            return Err("a service needs its owner's id".into());
        }
        for key in self.capabilities.keys() {
            check_kind(key).map_err(|_| {
                format!("capability {key:?} must be 1 to 63 of a-z, 0-9, '.', '_', '-' and ':'")
            })?;
        }
        Ok(())
    }
}

/// Whether `kind` may name a service kind (or a capability key): 1 to 63
/// of lowercase letters, digits, `.`, `_`, `-` and `:`, starting with a
/// letter or digit.
pub fn check_kind(kind: &str) -> Result<(), String> {
    let ok = !kind.is_empty()
        && kind.len() <= 63
        && kind.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && kind.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | ':')
        });
    match ok {
        true => Ok(()),
        false => Err(format!(
            "service kind {kind:?} must be 1 to 63 of a-z, 0-9, '.', '_', '-' and ':'"
        )),
    }
}

/// What a caller looks for: a kind, and capabilities a record must hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub kind: Option<String>,
    pub require: Vec<(String, Capability)>,
}

impl Query {
    pub fn kind(kind: &str) -> Query {
        Query {
            kind: Some(kind.to_owned()),
            require: Vec::new(),
        }
    }

    pub fn require(mut self, key: &str, value: impl Into<Capability>) -> Query {
        self.require.push((key.to_owned(), value.into()));
        self
    }
}

/// Time for leases, in milliseconds since the Unix epoch: the system's, or
/// one a test moves by hand.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Clock({})", self.now())
    }
}

impl Default for Clock {
    fn default() -> Clock {
        Clock::system()
    }
}

fn system_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Clock {
    pub fn system() -> Clock {
        Clock(Arc::new(system_ms))
    }

    /// A clock at `start` that moves only when the returned cell does.
    pub fn manual(start: u64) -> (Clock, Arc<AtomicU64>) {
        let cell = Arc::new(AtomicU64::new(start));
        let read = cell.clone();
        (Clock(Arc::new(move || read.load(Ordering::SeqCst))), cell)
    }

    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

/// The rows of one registry, inside one of its transactions. A backend
/// implements these plainly; what they mean is decided here, once.
pub trait Rows {
    /// Take the next value of the registry's change counter.
    fn next_seq(&mut self) -> io::Result<u64>;
    /// The counter's current value.
    fn head(&mut self) -> io::Result<u64>;
    fn get(&mut self, id: &str) -> io::Result<Option<Service>>;
    /// Insert or replace the row for `service.id`.
    fn put(&mut self, service: &Service) -> io::Result<()>;
    /// Every row, by ID.
    fn all(&mut self) -> io::Result<Vec<Service>>;
    /// Rows changed after `seq`, oldest change first.
    fn since(&mut self, seq: u64) -> io::Result<Vec<Service>>;
    /// Delete ended rows that ended before `before_ms`; how many.
    fn prune(&mut self, before_ms: u64) -> io::Result<usize>;
}

/// The body a backend stores for `service` (columns hold its kind, owner,
/// state, lease, change time and `seq` beside it).
pub fn encode(service: &Service) -> io::Result<String> {
    serde_json::to_string(service).map_err(io::Error::other)
}

/// A stored row, its columns overriding the body's copies.
pub fn decode(
    body: &str,
    state: &str,
    lease_until_ms: u64,
    changed_ms: u64,
    seq: u64,
) -> io::Result<Service> {
    let mut service: Service = serde_json::from_str(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("service record: {e}")))?;
    service.state = ServiceState::parse(state)?;
    service.lease_until_ms = lease_until_ms;
    service.changed_ms = changed_ms;
    service.seq = seq;
    Ok(service)
}

/// A registry: [`LocalRegistry`], or a server's operation store.
pub trait ServiceStore: Send + Sync {
    /// Run `f` in one transaction that writers take turns on, committed
    /// when it returns `Ok` and rolled back otherwise.
    fn transact(
        &self,
        f: &mut (dyn FnMut(&mut dyn Rows) -> io::Result<()> + Send),
    ) -> io::Result<()>;

    /// Register `service`, live until its `lease_until_ms`: a new record, or
    /// a replacement of one its owner holds, or of one no live owner
    /// holds. Refused (`AlreadyExists`) while another owner's lease on the
    /// ID lasts.
    fn register(&self, service: &Service, now_ms: u64) -> io::Result<Service> {
        service
            .check()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut out = None;
        self.transact(&mut |rows| {
            let existing = rows.get(&service.id)?;
            let mut next = service.clone();
            match existing {
                Some(held) if held.owner.id != service.owner.id && held.live_at(now_ms) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "service {} is held by another owner until its lease runs out",
                            service.id
                        ),
                    ))
                }
                Some(held)
                    if held.owner.id == service.owner.id && held.state == ServiceState::Live =>
                {
                    next.registered_ms = held.registered_ms;
                }
                _ => next.registered_ms = now_ms,
            }
            next.state = ServiceState::Live;
            next.renewed_ms = now_ms;
            next.changed_ms = now_ms;
            next.note = None;
            next.seq = rows.next_seq()?;
            rows.put(&next)?;
            out = Some(next);
            Ok(())
        })?;
        Ok(out.expect("set by the transaction"))
    }

    /// Extend the lease of `id`, which `owner` holds, to `lease_until_ms`,
    /// with a new health when given. `None` when it is no longer live or
    /// another owner holds it: the owner must register again.
    fn renew(
        &self,
        id: &str,
        owner: &str,
        lease_until_ms: u64,
        health: Option<Health>,
        now_ms: u64,
    ) -> io::Result<Option<Service>> {
        let mut out = None;
        self.transact(&mut |rows| {
            out = None;
            let Some(mut service) = rows.get(id)? else {
                return Ok(());
            };
            if service.owner.id != owner || service.state != ServiceState::Live {
                return Ok(());
            }
            service.lease_until_ms = lease_until_ms;
            service.renewed_ms = now_ms;
            if let Some(health) = health {
                service.health = health;
            }
            service.seq = rows.next_seq()?;
            rows.put(&service)?;
            out = Some(service);
            Ok(())
        })?;
        Ok(out)
    }

    /// Mark `id`, which `owner` holds, as left: nothing to reclaim. False
    /// when it was not live or another owner holds it.
    fn deregister(&self, id: &str, owner: &str, now_ms: u64) -> io::Result<bool> {
        let mut out = false;
        self.transact(&mut |rows| {
            out = false;
            let Some(mut service) = rows.get(id)? else {
                return Ok(());
            };
            if service.owner.id != owner || service.state != ServiceState::Live {
                return Ok(());
            }
            service.state = ServiceState::Left;
            service.changed_ms = now_ms;
            service.seq = rows.next_seq()?;
            rows.put(&service)?;
            out = true;
            Ok(())
        })?;
        Ok(out)
    }

    /// Move `id` to `state` with `note`, if it is still as the caller read
    /// it (its `seq` is `expected_seq`); the record as written, or `None`
    /// when it changed meanwhile.
    fn transition(
        &self,
        id: &str,
        expected_seq: u64,
        state: ServiceState,
        note: Option<String>,
        now_ms: u64,
    ) -> io::Result<Option<Service>> {
        let mut out = None;
        self.transact(&mut |rows| {
            out = None;
            let Some(mut service) = rows.get(id)? else {
                return Ok(());
            };
            if service.seq != expected_seq {
                return Ok(());
            }
            service.state = state;
            service.note = note.clone();
            service.changed_ms = now_ms;
            service.seq = rows.next_seq()?;
            rows.put(&service)?;
            out = Some(service);
            Ok(())
        })?;
        Ok(out)
    }

    /// One record, whatever its state.
    fn get(&self, id: &str) -> io::Result<Option<Service>> {
        let mut out = None;
        self.transact(&mut |rows| {
            out = rows.get(id)?;
            Ok(())
        })?;
        Ok(out)
    }

    /// Every record, whatever its state, by ID.
    fn all(&self) -> io::Result<Vec<Service>> {
        let mut out = Vec::new();
        self.transact(&mut |rows| {
            out = rows.all()?;
            Ok(())
        })?;
        Ok(out)
    }

    /// Records changed after `seq`, in the order they changed, with the
    /// counter's value they were read at.
    fn since(&self, seq: u64) -> io::Result<(u64, Vec<Service>)> {
        let mut out = (seq, Vec::new());
        self.transact(&mut |rows| {
            out = (rows.head()?, rows.since(seq)?);
            Ok(())
        })?;
        Ok(out)
    }

    /// Remove records that left or were reclaimed before `before_ms`.
    fn prune(&self, before_ms: u64) -> io::Result<usize> {
        let mut out = 0;
        self.transact(&mut |rows| {
            out = rows.prune(before_ms)?;
            Ok(())
        })?;
        Ok(out)
    }
}

/// The live records answering `query` at `now`, in the order [`resolve`]
/// prefers them: healthy first, then degraded, then starting (unhealthy
/// ones never), then the heaviest, then by ID.
pub fn candidates(
    store: &dyn ServiceStore,
    query: &Query,
    now_ms: u64,
) -> io::Result<Vec<Service>> {
    let mut found: Vec<Service> = store
        .all()?
        .into_iter()
        .filter(|s| s.live_at(now_ms) && s.health != Health::Unhealthy && s.answers(query))
        .collect();
    found.sort_by(|a, b| {
        a.health
            .cmp(&b.health)
            .then(b.weight.cmp(&a.weight))
            .then(a.id.cmp(&b.id))
    });
    Ok(found)
}

/// The record [`candidates`] puts first, if any.
pub fn resolve(
    store: &dyn ServiceStore,
    query: &Query,
    now_ms: u64,
) -> io::Result<Option<Service>> {
    Ok(candidates(store, query, now_ms)?.into_iter().next())
}

/// Mark expired every live record whose lease ran out by `now`, or whose
/// owner process is known to be gone from this host; each is marked once,
/// whichever caller gets there first. Returns those this call marked.
pub fn expire(store: &dyn ServiceStore, now_ms: u64) -> io::Result<Vec<Service>> {
    let mut marked = Vec::new();
    for service in store.all()? {
        if service.state != ServiceState::Live {
            continue;
        }
        let why = if service.lease_until_ms <= now_ms {
            "its lease ran out"
        } else if service.owner.gone() {
            "its owner process is gone"
        } else {
            continue;
        };
        if let Some(done) = store.transition(
            &service.id,
            service.seq,
            ServiceState::Expired,
            Some(why.to_owned()),
            now_ms,
        )? {
            marked.push(done);
        }
    }
    Ok(marked)
}

/// What a reaper did with one record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Reclaimed (or found nothing left); what it did.
    Done(String),
    /// Not this reaper's to reclaim (another host's process); why. The
    /// record stays expired for a reaper that can.
    Skipped(String),
    /// Reclaiming failed; why. The record stays expired, to try again.
    Failed(String),
}

/// One record a [`reap`] looked at, and what came of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reaped {
    pub service: Service,
    pub outcome: Outcome,
}

/// [`expire`], then reclaim every expired record (and every reclaim a
/// stopped reaper left under way for [`RECLAIM_TIMEOUT`]) through
/// `reclaimer`, claiming each first so that two reapers never reclaim one
/// record. A record with nothing to reclaim is marked reclaimed at once.
/// Ended records older than [`KEEP_ENDED`] are pruned.
pub fn reap(
    store: &dyn ServiceStore,
    now_ms: u64,
    reclaimer: &dyn Fn(&Service, &Reclaim) -> Outcome,
) -> io::Result<Vec<Reaped>> {
    expire(store, now_ms)?;
    let mut reaped = Vec::new();
    for service in store.all()? {
        let stalled = service.state == ServiceState::Reclaiming
            && service.changed_ms + RECLAIM_TIMEOUT.as_millis() as u64 <= now_ms;
        if service.state != ServiceState::Expired && !stalled {
            continue;
        }
        let Some(reclaim) = service.reclaim.clone() else {
            if let Some(done) = store.transition(
                &service.id,
                service.seq,
                ServiceState::Reclaimed,
                Some(format!(
                    "{}; nothing to reclaim",
                    service.note.as_deref().unwrap_or("expired")
                )),
                now_ms,
            )? {
                reaped.push(Reaped {
                    service: done,
                    outcome: Outcome::Done("nothing to reclaim".into()),
                });
            }
            continue;
        };
        let Some(claimed) = store.transition(
            &service.id,
            service.seq,
            ServiceState::Reclaiming,
            service.note.clone(),
            now_ms,
        )?
        else {
            continue;
        };
        let outcome = reclaimer(&claimed, &reclaim);
        let (state, note) = match &outcome {
            Outcome::Done(what) => (ServiceState::Reclaimed, what.clone()),
            Outcome::Skipped(why) => (ServiceState::Expired, why.clone()),
            Outcome::Failed(why) => (ServiceState::Expired, format!("reclaim failed: {why}")),
        };
        let written = store
            .transition(&claimed.id, claimed.seq, state, Some(note), now_ms)?
            .unwrap_or(claimed);
        reaped.push(Reaped {
            service: written,
            outcome,
        });
    }
    store.prune(now_ms.saturating_sub(KEEP_ENDED.as_millis() as u64))?;
    Ok(reaped)
}

/// Reclaim a [`Reclaim::Process`]: on its host, stop the process while its
/// pid still has its start time, and what is left of its group; elsewhere,
/// skip it. Other kinds are not a process reaper's.
pub fn reclaim_process(reclaim: &Reclaim) -> Outcome {
    let Reclaim::Process {
        host,
        pid,
        start,
        group,
    } = reclaim
    else {
        return Outcome::Skipped("not a process".into());
    };
    if host != crate::proc::host() {
        return Outcome::Skipped(format!("process {pid} runs on another host ({host})"));
    }
    let mut stopped = Vec::new();
    if crate::proc::kill(*pid, start) {
        stopped.push(*pid);
    }
    if let Some(group) = group {
        stopped.extend(crate::proc::kill_group(group.pgid, &group.leader_start));
    }
    stopped.sort_unstable();
    stopped.dedup();
    match stopped.is_empty() {
        true => Outcome::Done(format!("process {pid} had already exited")),
        false => Outcome::Done(format!(
            "stopped {}",
            stopped
                .iter()
                .map(|p| format!("pid {p}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What changed after `seq`, waiting up to `timeout` for a change: the
/// counter's value read, and the records. Looks again every 100 ms.
pub fn watch(
    store: &dyn ServiceStore,
    seq: u64,
    timeout: Duration,
) -> io::Result<(u64, Vec<Service>)> {
    let deadline = Instant::now() + timeout;
    loop {
        let (head, changed) = store.since(seq)?;
        if !changed.is_empty() || Instant::now() >= deadline {
            return Ok((head.max(seq), changed));
        }
        std::thread::sleep(Duration::from_millis(100).min(deadline - Instant::now()));
    }
}

/// The registry file of the repository at `root`: `BRANCHYARD_REGISTRY`
/// when set, else `.branchyard/registry.db`.
pub fn local_path(root: &Path) -> PathBuf {
    match std::env::var_os(ENV_REGISTRY).filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => crate::state::dir(root).join("registry.db"),
    }
}

struct Renewal {
    stop: Mutex<bool>,
    wake: Condvar,
}

/// A record its owner keeps live: registered when made, renewed from a
/// thread of its own every third of its lease, and deregistered when
/// dropped. A renewal that finds the record expired (the owner was
/// thought gone) registers it again.
pub struct Registration {
    store: Arc<dyn ServiceStore>,
    service: Arc<Mutex<Service>>,
    ttl: Duration,
    clock: Clock,
    renewal: Arc<Renewal>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Registration({})", self.id())
    }
}

impl Registration {
    /// Register `service` in `store` for `ttl` (within [`MIN_TTL`] and
    /// [`MAX_TTL`]) and keep it live.
    pub fn start(
        store: Arc<dyn ServiceStore>,
        mut service: Service,
        ttl: Duration,
        clock: Clock,
    ) -> io::Result<Registration> {
        let ttl = ttl.clamp(MIN_TTL, MAX_TTL);
        let now = clock.now();
        service.lease_until_ms = now + ttl.as_millis() as u64;
        let service = store.register(&service, now)?;
        let service = Arc::new(Mutex::new(service));
        let renewal = Arc::new(Renewal {
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let thread = {
            let (store, service, renewal, clock) = (
                store.clone(),
                service.clone(),
                renewal.clone(),
                clock.clone(),
            );
            std::thread::Builder::new()
                .name("by-registry".into())
                .spawn(move || renew_loop(&*store, &service, &renewal, ttl, &clock))?
        };
        Ok(Registration {
            store,
            service,
            ttl,
            clock,
            renewal,
            thread: Some(thread),
        })
    }

    pub fn id(&self) -> String {
        lock(&self.service).id.clone()
    }

    /// The record as last written.
    pub fn service(&self) -> Service {
        lock(&self.service).clone()
    }

    /// Renew now, with `health`.
    pub fn set_health(&self, health: Health) -> io::Result<()> {
        lock(&self.service).health = health;
        self.renew()
    }

    /// Change the record (its capabilities, endpoints, what to reclaim)
    /// and register it again at once.
    pub fn update(&self, change: impl FnOnce(&mut Service)) -> io::Result<()> {
        let mut service = lock(&self.service);
        let mut next = service.clone();
        change(&mut next);
        let now = self.clock.now();
        next.lease_until_ms = now + self.ttl.as_millis() as u64;
        *service = self.store.register(&next, now)?;
        Ok(())
    }

    /// Renew the lease now.
    pub fn renew(&self) -> io::Result<()> {
        renew_once(&*self.store, &self.service, self.ttl, &self.clock)
    }

    fn halt(&mut self) {
        *self.renewal.stop.lock_recovering("stop") = true;
        self.renewal.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("service heartbeat", thread);
        }
    }

    /// Stop renewing without deregistering: the lease runs out and the
    /// record is reaped as though its owner had stopped. For handing a
    /// resource over to whatever reaps it, and for tests.
    pub fn abandon(mut self) {
        self.halt();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.halt();
        let service = lock(&self.service).clone();
        let _ = self
            .store
            .deregister(&service.id, &service.owner.id, self.clock.now());
    }
}

fn lock(service: &Mutex<Service>) -> std::sync::MutexGuard<'_, Service> {
    service.lock_recovering("service")
}

fn renew_once(
    store: &dyn ServiceStore,
    service: &Mutex<Service>,
    ttl: Duration,
    clock: &Clock,
) -> io::Result<()> {
    let mut held = lock(service);
    let now = clock.now();
    let until = now + ttl.as_millis() as u64;
    match store.renew(&held.id, &held.owner.id, until, Some(held.health), now)? {
        Some(renewed) => *held = renewed,
        None => {
            // Expired while this owner lives (a lease that ran out under a
            // stalled clock): register it again.
            let mut again = held.clone();
            again.lease_until_ms = until;
            *held = store.register(&again, now)?;
        }
    }
    Ok(())
}

fn renew_loop(
    store: &dyn ServiceStore,
    service: &Mutex<Service>,
    renewal: &Renewal,
    ttl: Duration,
    clock: &Clock,
) {
    let every = ttl / 3;
    let mut stop = renewal.stop.lock_recovering("stop");
    loop {
        let (next, _) =
            renewal
                .wake
                .wait_timeout_while_recovering(stop, every, |stopped| !*stopped, "wake");
        stop = next;
        if *stop {
            return;
        }
        drop(stop);
        let _ = renew_once(store, service, ttl, clock);
        stop = renewal.stop.lock_recovering("stop");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_satisfy_queries_by_value_and_membership() {
        let list = Capability::List(vec!["github".into(), "slack".into()]);
        assert!(list.satisfies(&"github".into()));
        assert!(!list.satisfies(&"linear".into()));
        assert!(list.satisfies(&Capability::List(vec!["slack".into()])));
        assert!(Capability::Flag(true).satisfies(&Capability::Flag(true)));
        assert!(!Capability::Number(2).satisfies(&Capability::Number(3)));
        assert_eq!(Capability::parse("true"), Capability::Flag(true));
        assert_eq!(Capability::parse("12"), Capability::Number(12));
        assert_eq!(Capability::parse("mcp"), Capability::Text("mcp".into()));
    }

    #[test]
    fn records_round_trip_through_json_with_typed_capabilities() {
        let service = Service::new(KIND_CONNECTOR_GATEWAY, ServiceOwner::this_process())
            .with("connectors", vec!["github".to_owned()])
            .with("tls", false)
            .with("port", 8931_i64)
            .with("issuer", "branchyard:local:x")
            .with_endpoint(Endpoint::url("http://127.0.0.1:8931/mcp"))
            .with_reclaim(Reclaim::Process {
                host: "h".into(),
                pid: 7,
                start: "1".into(),
                group: None,
            });
        let text = encode(&service).unwrap();
        let back = decode(&text, "live", 5, 6, 7).unwrap();
        assert_eq!(back.capabilities, service.capabilities);
        assert_eq!(back.url(), Some("http://127.0.0.1:8931/mcp"));
        assert_eq!((back.lease_until_ms, back.changed_ms, back.seq), (5, 6, 7));
        assert!(service.check().is_ok());
        assert!(Service::new("Bad Kind", ServiceOwner::this_process())
            .check()
            .is_err());
    }

    #[test]
    fn this_process_is_never_gone_and_a_remote_owner_never_is() {
        assert!(!ServiceOwner::this_process().gone());
        assert!(!ServiceOwner::remote("o", "ops").gone());
    }
}
