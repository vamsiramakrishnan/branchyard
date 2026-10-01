//! Connectors: a branch's grant, the turn's token, the packages placed in
//! the harness's home, and the gateway's audit log as events. See
//! `docs/connectors.md`, the contract with Anvil.
//!
//! Branchyard is the access broker. A branch's grant
//! ([`Provisioning::connectors`](crate::Provisioning::connectors)) is
//! stored with it; a delegated child's is narrowed to its parent's. When a
//! turn of a branch with a grant starts, [`prepare`] checks every granted
//! connector is one the gateway serves, places each one's harness package
//! and the grant's `INDEX.md` under `~/.branchyard/connectors/` in the
//! branch's private home, signs a token for the turn with the yard's
//! Ed25519 key, writes it to a 0600 file there, and returns the two
//! variables the harness's Anvil CLIs and SDKs read. The gateway verifies
//! the token against the yard's public keys and enforces the grant;
//! [`ingest`] records each line of its audit log on the branch it names as
//! an [`Activity::ConnectorCall`](crate::Activity::ConnectorCall).

pub mod gateway;
pub mod keys;
pub mod packager;

use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use branchyard_provision::connectors::{
    check_connector, describe, fold_connector, intersect, narrow, Confirm, GrantEntry, GrantMode,
};
use branchyard_provision::EnvVar;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use keys::{Claims, KeyRing};
pub use packager::{AnvilPackager, Bundle, Packager};

use crate::state::Record;
use crate::{Activity, Error, RecordedEvent, Yard};

/// The variable naming the gateway's `/mcp` URL for Anvil's CLIs and SDKs.
pub const ENV_GATEWAY_URL: &str = "ANVIL_GATEWAY_URL";
/// The variable naming the 0600 file that holds the turn's token.
pub const ENV_GATEWAY_TOKEN_FILE: &str = "ANVIL_GATEWAY_TOKEN_FILE";
/// Where packages and the index go, relative to the harness's home.
pub const HOME_DIR: &str = ".branchyard/connectors";
/// The turn's token file, relative to the harness's home.
pub const TOKEN_FILE: &str = ".branchyard/gateway-token";
/// The longest a token lives, whatever the turn's deadline.
pub const MAX_TTL: Duration = Duration::from_secs(3600);
/// The longest a connect token lives: ten minutes, which the gateway
/// enforces too.
pub const CONNECT_TTL: Duration = Duration::from_secs(600);
/// A local yard's gateway files, under `.branchyard/`.
pub const GATEWAY_DIR: &str = "gateway";

/// A yard's connector gateway: where it is, who signs for the yard, and
/// where packages come from. Set with [`Yard::use_connectors`].
#[derive(Clone, Debug)]
pub struct Gateway {
    /// The gateway's canonical `/mcp` URL: every token's `aud`, and what a
    /// harness on this host is given.
    pub url: String,
    /// The same gateway as a sandboxed harness reaches it (a host address
    /// a Microsandbox guest can route to, or Substrate's routed ingress).
    /// Without it, a sandboxed branch with a grant fails its turn.
    pub sandbox_url: Option<String>,
    /// Every token's `iss`: the server's URL, or `branchyard:local:<id>`.
    pub issuer: String,
    /// The key file: a private JSON Web Key Set, signing key first.
    pub key_file: PathBuf,
    /// Where to keep the public set in step with the key file, for a
    /// gateway that reads it as a `file:` URL.
    pub jwks_file: Option<PathBuf>,
    /// The gateway's audit log (JSON lines), which [`ingest`] reads.
    pub audit_file: Option<PathBuf>,
    /// The subject of a branch that records none (`local:<user>` locally).
    pub subject: String,
    /// The tenant of a branch that records none (`local` locally).
    pub tenant: String,
    /// Prefixed to `by_branch` as `<scope>/<branch>`, when one gateway and
    /// its audit log serve several repositories (a server's repository
    /// name). Only lines with this prefix are ingested.
    pub branch_scope: Option<String>,
    /// The longest a token lives; at most [`MAX_TTL`].
    pub max_ttl: Duration,
    pub packager: Arc<dyn Packager>,
}

impl Gateway {
    /// A local yard's gateway at `url`: the key, public set and audit log
    /// under `.branchyard/gateway/`, issuer `branchyard:local:<yard id>`,
    /// subject `local:<os user>`.
    pub fn local(yard: &Yard, url: &str, packager: Arc<dyn Packager>) -> Result<Gateway, Error> {
        let dir = local_dir(yard.root());
        Ok(Gateway {
            url: url.to_owned(),
            sandbox_url: None,
            issuer: local_issuer(yard.root())?,
            key_file: dir.join("key"),
            jwks_file: Some(dir.join("jwks.json")),
            audit_file: Some(dir.join("audit.jsonl")),
            subject: local_subject(),
            tenant: "local".to_owned(),
            branch_scope: None,
            max_ttl: MAX_TTL,
            packager,
        })
    }

    /// The key ring, made on first use, with the public set kept in step.
    pub fn keys(&self) -> Result<KeyRing, Error> {
        KeyRing::load_or_create(&self.key_file, self.jwks_file.as_deref())
    }

    /// The public key set the gateway verifies against.
    pub fn jwks(&self) -> Result<Value, Error> {
        self.keys()?.jwks()
    }

    fn by_branch(&self, branch: &str) -> String {
        match &self.branch_scope {
            Some(scope) => format!("{scope}/{branch}"),
            None => branch.to_owned(),
        }
    }

    /// The person's connect token, for `anvil connect` (which asks the
    /// gateway for an authorization URL for `sub`, or stores a key): no
    /// branch, no turn, no grant, and `by_purpose: "connect"`. The gateway's
    /// connect routes take only this token, so a harness, which holds a
    /// turn's token, can never start a connection or replace the person's
    /// credential; and the gateway refuses it for tools. Lives `ttl`, at most
    /// [`CONNECT_TTL`]. Never give it to a harness.
    pub fn connect_token(&self, subject: Option<&str>, ttl: Duration) -> Result<String, Error> {
        self.keys()?.sign(&self.connect_claims(subject, ttl)?)
    }

    fn connect_claims(&self, subject: Option<&str>, ttl: Duration) -> Result<Claims, Error> {
        let now = now_secs();
        let ttl = ttl.min(self.max_ttl).min(CONNECT_TTL);
        Ok(Claims {
            iss: self.issuer.clone(),
            aud: self.url.clone(),
            sub: subject.unwrap_or(&self.subject).to_owned(),
            iat: now,
            exp: now + ttl.as_secs().max(1),
            jti: keys::random_id()?,
            by_tenant: self.tenant.clone(),
            by_branch: String::new(),
            by_turn: String::new(),
            by_grants: Vec::new(),
            by_purpose: Some(keys::CONNECT_PURPOSE.to_owned()),
        })
    }

    /// A token for the person themselves with `grants` and no branch, for a
    /// call `by` makes as them (such as fetching an issue for `--issue`
    /// through a tracker's connector). Lives `ttl`, at most [`MAX_TTL`].
    pub fn person_token_granted(
        &self,
        grants: Vec<GrantEntry>,
        ttl: Duration,
    ) -> Result<String, Error> {
        let now = now_secs();
        let claims = Claims {
            iss: self.issuer.clone(),
            aud: self.url.clone(),
            sub: self.subject.clone(),
            iat: now,
            exp: now + ttl.min(self.max_ttl).min(MAX_TTL).as_secs().max(1),
            jti: keys::random_id()?,
            by_tenant: self.tenant.clone(),
            by_branch: String::new(),
            by_turn: String::new(),
            by_grants: grants,
            by_purpose: None,
        };
        self.keys()?.sign(&claims)
    }
}

/// `.branchyard/gateway` of the repository at `root`.
pub fn local_dir(root: &Path) -> PathBuf {
    crate::state::dir(root).join(GATEWAY_DIR)
}

/// `branchyard:local:<yard id>`, the id made once and kept in
/// `.branchyard/gateway/yard-id`.
pub fn local_issuer(root: &Path) -> Result<String, Error> {
    let path = local_dir(root).join("yard-id");
    let id = match fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => id.trim().to_owned(),
        _ => {
            let id = keys::random_id()?.replace(['-', '_'], "x");
            keys::write_private(&path, id.as_bytes())?;
            id
        }
    };
    Ok(format!("branchyard:local:{id}"))
}

/// `local:<os user>`.
pub fn local_subject() -> String {
    let user = ["USER", "LOGNAME"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| format!("uid-{}", rustix::process::getuid().as_raw()));
    format!("local:{user}")
}

/// Who a branch acts for, as its token's `sub` and `by_tenant`: recorded
/// when the branch is created (a server's principal), inherited by its
/// forks and children.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub subject: String,
    pub tenant: String,
}

/// A gateway call, from one line of its audit log. Fields are Anvil's;
/// any it does not write are absent.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectorCall {
    pub connector: String,
    pub operation: String,
    /// `allowed`, `denied` or `confirmation_required`.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The token's `by_turn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// The token's `sub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Why it was refused, as the gateway's error code (such as
    /// `policy_denied`), when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The grant entry that allowed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// The grant rule that decided a refusal (such as
    /// `policy/grant_denied`), when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// A hash of the redacted input (`sha256:<hex>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<String>,
    /// The line's own time, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

impl ConnectorCall {
    /// One line for people: `github issues.list allowed (200, 41 ms)`.
    pub fn describe(&self) -> String {
        let mut out = format!("{} {} {}", self.connector, self.operation, self.decision);
        if let Some(account) = &self.account {
            out = format!(
                "{}@{account} {} {}",
                self.connector, self.operation, self.decision
            );
        }
        let mut detail = Vec::new();
        if let Some(reason) = &self.reason {
            detail.push(reason.clone());
        }
        if let Some(status) = self.upstream_status {
            detail.push(status.to_string());
        }
        if let Some(ms) = self.latency_ms {
            detail.push(format!("{ms} ms"));
        }
        if !detail.is_empty() {
            out.push_str(&format!(" ({})", detail.join(", ")));
        }
        out
    }
}

/// What a turn's connectors give its harness.
pub(crate) struct Prepared {
    pub env: Vec<EnvVar>,
    /// The gateway's URL as the harness is given it, which a restricted
    /// network policy allows (`crate::egress`).
    pub gateway_url: String,
    /// One line for the harness's instructions, pointing at `INDEX.md`.
    pub instruction: String,
    pub connectors: Vec<String>,
    /// Written in the home, relative to it.
    pub files: Vec<String>,
    /// The token file on this host, removed when the turn ends (held for
    /// its drop).
    pub _token: TokenFile,
}

/// The turn's token file, removed when dropped.
pub(crate) struct TokenFile(PathBuf);

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prepare the turn's connectors: `None` when the branch has no grant.
/// A failure is the turn's, by name: no gateway, no private home, a
/// connector the gateway does not serve, a sandbox the gateway is not
/// reachable from, or packaging.
pub(crate) fn prepare(
    yard: &Yard,
    record: &Record,
    deadline_ms: Option<u64>,
) -> Result<Option<Prepared>, String> {
    let grant = match &record.provision {
        Some(spec) if !spec.connectors.is_empty() => spec.connectors.clone(),
        _ => return Ok(None),
    };
    let names = branchyard_provision::connectors::connectors(&grant);
    let gateway = yard.connectors().ok_or_else(|| {
        format!(
            "it is granted connectors ({}) but this yard has no connector gateway; set \
             [connectors] gateway in branchyard.toml (docs/connectors.md)",
            names.join(", ")
        )
    })?;
    let home = record
        .home
        .as_deref()
        .ok_or("connectors are placed only in a home private to the branch")?;
    let sandboxed = crate::placement::sandboxed(record.provider.as_ref());
    let url = match (sandboxed, &gateway.sandbox_url) {
        (false, _) => gateway.url.clone(),
        (true, Some(url)) => url.clone(),
        (true, None) => {
            return Err(
                "it is granted connectors and runs in a sandbox, but no gateway address \
                 reachable from a sandbox is configured ([connectors] sandbox_gateway)"
                    .into(),
            )
        }
    };
    let served = gateway.packager.served()?;
    let mut bundles = Vec::new();
    for name in &names {
        match served.iter().find(|b| &b.id == name) {
            Some(bundle) => bundles.push(bundle.clone()),
            None => {
                let ids: Vec<&str> = served.iter().map(|b| b.id.as_str()).collect();
                return Err(format!(
                    "connector {name} is not served by the gateway (it serves: {})",
                    match ids.is_empty() {
                        true => "nothing".to_owned(),
                        false => ids.join(", "),
                    }
                ));
            }
        }
    }
    let store = yard.store();
    let cache = store.dir().join("connectors").join("cache");
    let staged = home.join(format!(
        ".branchyard/.connectors-{}",
        keys::random_id().map_err(|e| e.to_string())?
    ));
    let placed = (|| -> Result<(), String> {
        fs::create_dir_all(&staged).map_err(|e| format!("create {}: {e}", staged.display()))?;
        for bundle in &bundles {
            let package = cached_package(gateway.packager.as_ref(), &cache, bundle)?;
            packager::copy_tree(&package, &staged.join(&bundle.id))?;
        }
        let grants = staged.join(".grants.json");
        let text = serde_json::to_string_pretty(&grant).map_err(|e| e.to_string())?;
        fs::write(&grants, text).map_err(|e| format!("write {}: {e}", grants.display()))?;
        gateway
            .packager
            .index(&grants, &bundles, &staged.join("INDEX.md"))?;
        let _ = fs::remove_file(&grants);
        if !staged.join("INDEX.md").is_file() {
            return Err("the connector index was not written".into());
        }
        let target = home.join(HOME_DIR);
        if target.exists() {
            fs::remove_dir_all(&target)
                .map_err(|e| format!("replace {}: {e}", target.display()))?;
        }
        fs::rename(&staged, &target).map_err(|e| format!("place {}: {e}", target.display()))
    })();
    if placed.is_err() {
        let _ = fs::remove_dir_all(&staged);
    }
    placed.map_err(|e| format!("could not place connector packages: {e}"))?;
    // The token: this turn's, for at most an hour and not past the
    // turn's deadline.
    let now = now_secs();
    let mut exp = now + gateway.max_ttl.min(MAX_TTL).as_secs().max(1);
    if let Some(deadline) = deadline_ms {
        exp = exp.min((deadline / 1000).max(now + 1));
    }
    let actor = record.actor.clone().unwrap_or_else(|| Actor {
        subject: gateway.subject.clone(),
        tenant: gateway.tenant.clone(),
    });
    let claims = Claims {
        iss: gateway.issuer.clone(),
        aud: gateway.url.clone(),
        sub: actor.subject,
        iat: now,
        exp,
        jti: keys::random_id().map_err(|e| e.to_string())?,
        by_tenant: actor.tenant,
        by_branch: gateway.by_branch(&record.info.name),
        by_turn: (record.info.turns + 1).to_string(),
        by_grants: grant,
        by_purpose: None,
    };
    let token = gateway
        .keys()
        .and_then(|keys| keys.sign(&claims))
        .map_err(|e| format!("could not sign the gateway token: {e}"))?;
    let token_path = home.join(TOKEN_FILE);
    keys::write_private(&token_path, token.as_bytes())
        .map_err(|e| format!("could not write the gateway token: {e}"))?;
    let (_, guest_home) = crate::placement::guest_paths(record);
    let guest = |rel: &str| format!("{}/{rel}", guest_home.trim_end_matches('/'));
    Ok(Some(Prepared {
        env: vec![
            EnvVar::plain(ENV_GATEWAY_URL, url.clone()),
            EnvVar::plain(ENV_GATEWAY_TOKEN_FILE, guest(TOKEN_FILE)),
        ],
        instruction: format!(
            "Connectors ({}) are available through Branchyard's gateway: before using one, read {} \
             and follow it; never ask for or use upstream credentials.",
            names.join(", "),
            guest(&format!("{HOME_DIR}/INDEX.md"))
        ),
        connectors: names,
        gateway_url: url,
        files: vec![format!("{HOME_DIR}/"), TOKEN_FILE.to_owned()],
        _token: TokenFile(token_path),
    }))
}

/// A bundle's package from the cache, packaged first if needed; keyed by
/// the bundle's hash, so a changed bundle is packaged again.
fn cached_package(
    packager: &dyn Packager,
    cache: &Path,
    bundle: &Bundle,
) -> Result<PathBuf, String> {
    let dir = cache.join(&bundle.hash);
    if dir.is_dir() {
        return Ok(dir);
    }
    fs::create_dir_all(cache).map_err(|e| format!("create {}: {e}", cache.display()))?;
    let tmp = cache.join(format!(
        ".tmp-{}",
        keys::random_id().map_err(|e| e.to_string())?
    ));
    fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    if let Err(e) = packager.package(bundle, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("packaging {}: {e}", bundle.id));
    }
    // Another turn may have packaged it meanwhile; either copy will do.
    if fs::rename(&tmp, &dir).is_err() {
        let _ = fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(format!("could not cache the package of {}", bundle.id));
        }
    }
    Ok(dir)
}

/// Reads the gateway's audit log while a turn with connectors runs, so its
/// calls appear as they happen, and once more when dropped.
pub(crate) struct AuditTail {
    yard: Yard,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AuditTail {
    /// How often the log is read.
    const EVERY: Duration = Duration::from_millis(250);

    pub fn start(yard: &Yard) -> AuditTail {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (yard, stop) = (yard.clone(), stop.clone());
            std::thread::Builder::new()
                .name("by-audit".into())
                .spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = ingest(&yard);
                        std::thread::sleep(AuditTail::EVERY);
                    }
                })
                .ok()
        };
        AuditTail {
            yard: yard.clone(),
            stop,
            thread,
        }
    }
}

impl Drop for AuditTail {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = ingest(&self.yard);
    }
}

/// Where [`ingest`] has read the audit log to: the file's identity and
/// the offset after the last complete line recorded.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    file: String,
    dev: u64,
    ino: u64,
    offset: u64,
}

/// Record each new line of the gateway's audit log whose `by_branch` is a
/// branch of this yard as an [`Activity::ConnectorCall`] on it; lines for
/// other branches (another yard's, a removed branch's) are skipped. Reads
/// from where the last call stopped, kept in
/// `.branchyard/gateway/audit.cursor`; a log that was replaced or truncated
/// is read from its start. One process ingests at a time. Returns how many
/// calls were recorded.
pub fn ingest(yard: &Yard) -> Result<usize, Error> {
    let Some(gateway) = yard.connectors() else {
        return Ok(0);
    };
    let Some(audit) = &gateway.audit_file else {
        return Ok(0);
    };
    let meta = match fs::metadata(audit) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::State(format!("read {}: {e}", audit.display()))),
    };
    let dir = local_dir(yard.root());
    fs::create_dir_all(&dir).map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("audit.lock"))
        .map_err(|e| Error::State(format!("open the audit lock: {e}")))?;
    lock.lock()
        .map_err(|e| Error::State(format!("lock the audit cursor: {e}")))?;
    let cursor_path = dir.join("audit.cursor");
    let mut cursor: Cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let file_name = audit.display().to_string();
    if cursor.file != file_name
        || cursor.dev != meta.dev()
        || cursor.ino != meta.ino()
        || cursor.offset > meta.len()
    {
        cursor = Cursor {
            file: file_name,
            dev: meta.dev(),
            ino: meta.ino(),
            offset: 0,
        };
    }
    let mut file = fs::File::open(audit)
        .map_err(|e| Error::State(format!("open {}: {e}", audit.display())))?;
    file.seek(SeekFrom::Start(cursor.offset))
        .map_err(|e| Error::State(format!("seek {}: {e}", audit.display())))?;
    let mut reader = BufReader::new(file);
    let store = yard.store();
    let mut recorded = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::State(format!("read {}: {e}", audit.display())))?;
        // Only complete lines: the gateway may be writing the next one.
        if read == 0 || !line.ends_with('\n') {
            break;
        }
        if let Some((branch, at_ms, call)) = parse_line(&line, gateway.branch_scope.as_deref()) {
            if store.read(&branch).is_ok() {
                let event = RecordedEvent {
                    at_ms,
                    activity: Activity::ConnectorCall(Box::new(call)),
                };
                store.append(&branch, &event, None)?;
                recorded += 1;
            }
        }
        cursor.offset += read as u64;
        // Kept after every recorded line, so a crash repeats at most one.
        let text = serde_json::to_string(&cursor).map_err(|e| Error::State(e.to_string()))?;
        keys::write_atomic(&cursor_path, text.as_bytes(), 0o600)?;
    }
    Ok(recorded)
}

/// A line's branch, time and call; `None` for a line that is not a call
/// or is not this yard's.
fn parse_line(line: &str, scope: Option<&str>) -> Option<(String, u64, ConnectorCall)> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| value.get(*k).and_then(Value::as_str))
            .map(str::to_owned)
    };
    let number = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| value.get(*k).and_then(Value::as_i64))
    };
    let by_branch = text(&["by_branch"])?;
    let branch = match scope {
        Some(scope) => by_branch.strip_prefix(&format!("{scope}/"))?.to_owned(),
        None => by_branch,
    };
    if branch.is_empty() {
        return None;
    }
    let time = text(&["time", "ts", "timestamp"]);
    let at_ms = time
        .as_deref()
        .and_then(rfc3339_ms)
        .or_else(|| number(&["at_ms"]).map(|n| n.max(0) as u64))
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });
    let call = ConnectorCall {
        connector: text(&["connector", "bundle"]).unwrap_or_default(),
        operation: text(&["operation", "operation_id", "tool"]).unwrap_or_default(),
        decision: text(&["decision"]).unwrap_or_else(|| "unknown".into()),
        account: text(&["account"]),
        turn: text(&["by_turn"]).or_else(|| number(&["by_turn"]).map(|n| n.to_string())),
        subject: text(&["sub"]),
        reason: text(&["error_code", "reason", "code"]).or_else(|| {
            value
                .get("error")
                .and_then(|e| e.get("code").and_then(Value::as_str).or(e.as_str()))
                .map(str::to_owned)
        }),
        grant: value
            .get("grant")
            .or_else(|| value.get("grant_entry"))
            .filter(|g| !g.is_null())
            .cloned(),
        upstream_status: number(&["upstream_status", "status"]),
        latency_ms: number(&["latency_ms"]).map(|n| n.max(0) as u64),
        rule: text(&["rule"]),
        input_hash: text(&["input_sha256", "input_hash"]),
        time,
    };
    Some((branch, at_ms, call))
}

/// `2026-09-30T12:34:56.789Z` (or with a `+hh:mm` offset) as milliseconds
/// since the Unix epoch.
fn rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &text[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        let mut padded = digits.clone();
        padded.truncate(3);
        while padded.len() < 3 {
            padded.push('0');
        }
        ms = padded.parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + s - offset;
    u64::try_from(secs * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_lines_parse_with_or_without_a_scope() {
        let line = r#"{"time":"2026-09-30T12:00:01.5Z","sub":"local:me","by_tenant":"local","by_branch":"fix","by_turn":"2","connector":"github","account":"work","operation":"issues.create","decision":"denied","reason":"policy_denied","latency_ms":3}"#;
        let (branch, at, call) = parse_line(line, None).unwrap();
        assert_eq!(branch, "fix");
        assert_eq!(at, 1_790_769_601_500);
        assert_eq!(call.decision, "denied");
        assert_eq!(call.reason.as_deref(), Some("policy_denied"));
        assert_eq!(call.turn.as_deref(), Some("2"));
        assert_eq!(
            call.describe(),
            "github@work issues.create denied (policy_denied, 3 ms)"
        );
        assert!(parse_line(line, Some("app")).is_none());
        let scoped = line.replace("\"fix\"", "\"app/fix\"");
        assert_eq!(parse_line(&scoped, Some("app")).unwrap().0, "fix");
        assert!(parse_line("not json", None).is_none());
        assert!(parse_line(r#"{"connector":"github"}"#, None).is_none());
        let nested = r#"{"by_branch":"b","connector":"c","operation":"o","decision":"denied","error":{"code":"policy_denied"}}"#;
        assert_eq!(
            parse_line(nested, None).unwrap().2.reason.as_deref(),
            Some("policy_denied")
        );
    }

    #[test]
    fn times_parse_as_rfc3339() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            rfc3339_ms("2000-03-01T00:00:00.123456Z"),
            Some(951_868_800_123)
        );
        assert_eq!(rfc3339_ms("yesterday"), None);
    }
}
