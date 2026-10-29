//! Authenticated workspace runtime access with injected console authorization.
//!
//! The embedding server supplies current workspace membership, credential
//! revocation and managed-yard resolution. This module never guesses a host,
//! executable, repository path or durable operation on behalf of a browser.

mod routes;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use branchyard::workspace_runtime::{RuntimeAction, RuntimeState, Terminal, TerminalOptions};
use branchyard::{Error, Yard};
use branchyard_client::api::Operation;
use branchyard_support::{new_ulid, time::now_ms, LockExt as _};
use serde::{Deserialize, Serialize};

use crate::api::Caller;
use crate::config::sha256_hex;
use crate::error::{self, ApiError};

pub use routes::router;

/// The authority that the resolver must verify against current console state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeAccess {
    /// Read scope and workspace membership.
    Inspect,
    /// Run scope, workspace editor membership and operator shell permission.
    Terminal,
    /// Run scope and workspace editor membership for this resource action.
    Control(RuntimeAction),
}

/// A workspace session resolved by the console after its current ACL checks.
#[derive(Clone)]
pub struct AuthorizedWorkspace {
    /// The already-authorized managed yard, never a browser-supplied path.
    pub yard: Yard,
    /// The existing execution branch bound to the session.
    pub branch: String,
}

impl AuthorizedWorkspace {
    fn same(&self, other: &Self) -> bool {
        self.branch == other.branch && self.yard.root() == other.yard.root()
    }
}

/// Re-read credentials, workspace membership and session binding on every call.
/// Terminal authorization must also recheck the operator's shell setting.
pub type Resolver = dyn Fn(&Caller, &str, &str, RuntimeAccess) -> Result<AuthorizedWorkspace, ApiError>
    + Send
    + Sync;

/// Admit lifecycle work to the existing durable operation queue atomically.
/// The last argument is the validated request idempotency key. The queued
/// worker calls `RuntimeGateway::execute_lifecycle` with its operation ID.
pub type LifecycleQueue =
    dyn Fn(&Caller, &str, &str, RuntimeAction, &str) -> Result<Operation, ApiError> + Send + Sync;

/// Operator policy and bounds for process-local terminal handles.
#[derive(Clone, Debug)]
pub struct RuntimeGatewayOptions {
    /// Explicit operator shell permission. Disabled by default.
    pub allow_client_commands: bool,
    /// Fixed operator-approved Python and shell; HTTP bodies cannot set them.
    pub terminal: TerminalOptions,
    /// Global bound on starting, attached and retained closed terminal records.
    pub max_terminals: usize,
    /// Close an idle terminal, including when its client disappeared.
    pub terminal_idle_timeout: Duration,
}

impl Default for RuntimeGatewayOptions {
    fn default() -> Self {
        Self {
            allow_client_commands: false,
            terminal: TerminalOptions::default(),
            max_terminals: 32,
            terminal_idle_timeout: Duration::from_secs(15 * 60),
        }
    }
}

/// Runtime observation with terminal readiness and explicit data retention.
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    /// Actual provider state and lifecycle capabilities.
    #[serde(flatten)]
    pub runtime: RuntimeState,
    /// Whether this server can offer an idle workspace terminal.
    pub terminal_available: bool,
    /// Actionable reason when a terminal cannot be opened here.
    pub terminal_reason: Option<String>,
    /// Source originals remain independent of runtime resource actions.
    pub preserves_sources: bool,
    /// Accepted deliverables remain independent of runtime resource actions.
    pub preserves_deliverables: bool,
    /// Terminal handles and replay buffers do not survive a server restart.
    pub terminal_persistent: bool,
}

/// Terminal geometry. Executable, host, path and environment fields are refused.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTerminal {
    /// Initial rows, or the operator default.
    pub rows: Option<u16>,
    /// Initial columns, or the operator default.
    pub columns: Option<u16>,
}

/// A process-local terminal handle; `opening` can be retried with the same key.
#[derive(Clone, Debug, Serialize)]
pub struct TerminalHandle {
    /// Opaque server-generated terminal identity.
    pub id: String,
    /// `opening`, `open`, or `closed`.
    pub state: String,
    /// Always false. A lost handle must be opened again explicitly.
    pub persistent: bool,
    /// Inactivity bound, measured from the last authorized HTTP action.
    pub idle_timeout_seconds: u64,
}

struct Metadata {
    workspace: String,
    session: String,
    owner: String,
    key: String,
    fingerprint: String,
}

struct Slot {
    meta: Metadata,
    entry: Option<Arc<Entry>>,
}

struct InputSequence {
    last: Option<(u64, String)>,
    uncertain: bool,
}

struct Entry {
    terminal: Terminal,
    target: AuthorizedWorkspace,
    last_activity: Arc<AtomicU64>,
    input: Mutex<InputSequence>,
}

/// A bounded terminal registry and runtime adapter, independent of console SQL.
///
/// Mount `router` behind authentication. The resolver must recheck revocable
/// identity and editor membership, including inside a running PTY callback.
/// Native turns retain their own leases; manual writers must wait until idle.
pub struct RuntimeGateway {
    options: RuntimeGatewayOptions,
    resolve: Arc<Resolver>,
    lifecycle: Option<Arc<LifecycleQueue>>,
    slots: Mutex<BTreeMap<String, Slot>>,
    stopping: AtomicBool,
}

impl RuntimeGateway {
    /// Whether a starting or live terminal owns a workspace's manual writer lease.
    pub(crate) fn has_manual_writer(&self, workspace: &str, session: Option<&str>) -> bool {
        self.slots
            .lock_recovering("workspace terminals")
            .values()
            .any(|slot| {
                slot.meta.workspace == workspace
                    && session.is_none_or(|session| slot.meta.session == session)
                    && slot
                        .entry
                        .as_ref()
                        .is_none_or(|entry| !entry.terminal.is_closed())
            })
    }

    /// Construct the gateway without starting any process or provider resource.
    pub fn new(
        options: RuntimeGatewayOptions,
        resolve: Arc<Resolver>,
        lifecycle: Option<Arc<LifecycleQueue>>,
    ) -> Result<Self, ApiError> {
        if !(1..=256).contains(&options.max_terminals)
            || options.terminal_idle_timeout < Duration::from_secs(1)
            || options.terminal_idle_timeout > Duration::from_secs(86400)
        {
            return Err(ApiError::bad_request(
                "terminal capacity must be 1..256 and idle timeout 1 second..1 day",
            ));
        }
        Ok(Self {
            options,
            resolve,
            lifecycle,
            slots: Mutex::new(BTreeMap::new()),
            stopping: AtomicBool::new(false),
        })
    }

    fn authorize(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        access: RuntimeAccess,
    ) -> Result<AuthorizedWorkspace, ApiError> {
        let scope = if access == RuntimeAccess::Inspect {
            "read"
        } else {
            "run"
        };
        if !caller.0.allows(scope) {
            return Err(crate::api::scope_required(scope));
        }
        if access == RuntimeAccess::Terminal && !self.options.allow_client_commands {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "terminal_disabled",
                "terminal access requires the operator's allow_client_commands setting",
            ));
        }
        (self.resolve)(caller, workspace, session, access)
    }

    /// Reauthorize queued lifecycle work and apply its actual provider action.
    /// The operation ID comes from the durable queue, never a request body.
    pub fn execute_lifecycle(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        action: RuntimeAction,
        operation_id: &str,
    ) -> Result<RuntimeState, ApiError> {
        let target = self.authorize(caller, workspace, session, RuntimeAccess::Control(action))?;
        target
            .yard
            .workspace_runtime_action(&target.branch, action, operation_id)
            .map_err(|error| error::sdk(&error))
    }

    /// Close every terminal on graceful shutdown and refuse new terminal opens.
    /// Restart recovery uses the core process registry; handles are not resumed.
    pub fn close_all(&self) {
        self.stopping.store(true, Ordering::Release);
        let slots = std::mem::take(&mut *self.slots.lock_recovering("workspace terminals"));
        for slot in slots.into_values() {
            if let Some(entry) = slot.entry {
                entry.terminal.close();
            }
        }
    }

    fn handle(&self, id: &str, slot: &Slot) -> TerminalHandle {
        TerminalHandle {
            id: id.into(),
            state: slot
                .entry
                .as_ref()
                .map_or("opening", |entry| {
                    if entry.terminal.is_closed() {
                        "closed"
                    } else {
                        "open"
                    }
                })
                .into(),
            persistent: false,
            idle_timeout_seconds: self.options.terminal_idle_timeout.as_secs(),
        }
    }

    fn open(
        &self,
        caller: Caller,
        workspace: String,
        session: String,
        request: OpenTerminal,
        key: String,
    ) -> Result<TerminalHandle, ApiError> {
        let target = self.authorize(&caller, &workspace, &session, RuntimeAccess::Terminal)?;
        if request.rows.is_some_and(|value| value == 0 || value > 1000)
            || request
                .columns
                .is_some_and(|value| value == 0 || value > 1000)
        {
            return Err(ApiError::bad_request(
                "terminal dimensions must be between 1 and 1000",
            ));
        }
        if self.stopping.load(Ordering::Acquire) {
            return Err(ApiError::shutting_down());
        }
        let fingerprint =
            serde_json::to_vec(&request).map_err(|error| ApiError::internal(error.to_string()))?;
        let fingerprint = sha256_hex(&fingerprint);
        let owner = caller.idempotency_scope();
        let id = format!(
            "terminal_{}",
            new_ulid().map_err(|error| ApiError::internal(format!(
                "cannot allocate terminal identity: {error}"
            )))?
        );
        {
            let mut slots = self.slots.lock_recovering("workspace terminals");
            if let Some((id, slot)) = slots.iter().find(|(_, slot)| {
                slot.meta.owner == owner
                    && slot.meta.workspace == workspace
                    && slot.meta.session == session
                    && slot.meta.key == key
            }) {
                if slot.meta.fingerprint != fingerprint {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "idempotency_key_reused",
                        "terminal key was already used with different geometry",
                    ));
                }
                return Ok(self.handle(id, slot));
            }
            if slots.len() >= self.options.max_terminals {
                slots.retain(|_, slot| {
                    slot.entry
                        .as_ref()
                        .is_none_or(|entry| !entry.terminal.is_closed())
                });
            }
            if slots.len() >= self.options.max_terminals {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "terminal_capacity",
                    "terminal capacity is full; close a terminal before opening another",
                ));
            }
            slots.insert(
                id.clone(),
                Slot {
                    meta: Metadata {
                        workspace: workspace.clone(),
                        session: session.clone(),
                        owner,
                        key,
                        fingerprint,
                    },
                    entry: None,
                },
            );
        }
        let mut options = self.options.terminal.clone();
        options.rows = request.rows.unwrap_or(options.rows);
        options.columns = request.columns.unwrap_or(options.columns);
        let last_activity = Arc::new(AtomicU64::new(now_ms()));
        let activity = last_activity.clone();
        let resolver = self.resolve.clone();
        let bound = target.clone();
        let timeout_ms = self.options.terminal_idle_timeout.as_millis();
        let terminal = (|| {
            let runtime = target
                .yard
                .workspace_runtime(&target.branch)
                .map_err(|error| error::sdk(&error))?;
            Terminal::start(
                runtime,
                options,
                Arc::new(move || {
                    if u128::from(now_ms().saturating_sub(activity.load(Ordering::Acquire)))
                        >= timeout_ms
                    {
                        return Err(Error::Denied("terminal idle timeout expired".into()));
                    }
                    let current = resolver(&caller, &workspace, &session, RuntimeAccess::Terminal)
                        .map_err(|_| {
                            Error::Denied("workspace terminal authority was revoked".into())
                        })?;
                    if !bound.same(&current) {
                        return Err(Error::Denied("workspace session binding changed".into()));
                    }
                    Ok(())
                }),
            )
            .map_err(|error| error::sdk(&error))
        })();
        let mut slots = self.slots.lock_recovering("workspace terminals");
        let terminal = match terminal {
            Ok(terminal) => terminal,
            Err(error) => {
                slots.remove(&id);
                return Err(error);
            }
        };
        if self.stopping.load(Ordering::Acquire) {
            slots.remove(&id);
            drop(slots);
            terminal.close();
            return Err(ApiError::shutting_down());
        }
        let slot = slots.get_mut(&id).ok_or_else(lost)?;
        slot.entry = Some(Arc::new(Entry {
            terminal,
            target,
            last_activity,
            input: Mutex::new(InputSequence {
                last: None,
                uncertain: false,
            }),
        }));
        Ok(self.handle(&id, slot))
    }

    fn entry(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        id: &str,
    ) -> Result<Arc<Entry>, ApiError> {
        let target = self.authorize(caller, workspace, session, RuntimeAccess::Terminal)?;
        let slots = self.slots.lock_recovering("workspace terminals");
        let slot = slots
            .get(id)
            .filter(|slot| {
                slot.meta.workspace == workspace
                    && slot.meta.session == session
                    && slot.meta.owner == caller.idempotency_scope()
            })
            .ok_or_else(lost)?;
        let entry = slot
            .entry
            .as_ref()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::CONFLICT,
                    "terminal_starting",
                    "terminal startup is still in progress",
                )
            })?
            .clone();
        if !entry.target.same(&target) {
            drop(slots);
            entry.terminal.close();
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "session_changed",
                "workspace session binding changed",
            ));
        }
        entry.last_activity.store(now_ms(), Ordering::Release);
        Ok(entry)
    }
}

impl Drop for RuntimeGateway {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn lost() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "terminal_lost",
        "terminal is unavailable on this server; after a restart, open a new terminal explicitly",
    )
    .detail(serde_json::json!({"persistent":false}))
}
