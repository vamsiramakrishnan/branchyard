//! One turn on one branch: start the harness in the branch's worktree,
//! submit the prompt, answer and record everything until the turn ends,
//! enforce the budget, snapshot the candidate and close the harness.
//!
//! A delegating branch's turn also gets Branchyard's MCP server and a token
//! that lives exactly as long as the turn. Its cost limit counts what its
//! children reserved, and a cancel request from an ancestor interrupts it
//! like a budget limit does.
//!
//! The turn runs under its branch's lease (see [`crate::state`]), renewed by
//! while it is held; every record write and event append is fenced by it.
//! Its steps are journaled, intent before effect and outcome after:
//! `start` (the harness process and its identity), `submit` (the prompt),
//! `turn_end` (how the turn ended) and `snapshot` (the candidate). Recovery
//! reads them to say truthfully what a turn whose engine stopped did; a
//! submitted prompt is never submitted again.
//!
//! Steered input ([`crate::Branch::steer`]) is polled from the store like a
//! cancel, bound to this turn, and written to the harness while the turn
//! runs; what became of each is recorded in the store and the event log.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::Profile;
use branchyard_harness::{Open, Rejected, SessionMode};
use branchyard_runtime::{RuntimeError, Session};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::delegation;
use crate::graph;
use crate::placement::{Placement, SandboxPlan};
use crate::projection::{ENV_BRANCH, ENV_ROOT};
use crate::record::Recorder;
use crate::state::{Begun, Fence, Lease, ProcessRow, Record, Store};
use crate::store_codec::deadline_capped;
use crate::{
    git, names, Activity, Branch, BranchStatus, Budget, CandidateInfo, DecisionSource,
    DeliveredVia, Error, Event, NativeSession, PermissionDecision, PermissionRequest, Policy,
    StallAction, SteerState, TaskOptions, TurnOutcome, Yard,
};
use branchyard_support::time::now_ms;

/// How long a harness may take to complete its handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a harness may take to exit once its stdin is closed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);
/// How long a harness may take to end a turn after an interrupt.
const INTERRUPT_GRACE: Duration = Duration::from_secs(30);
/// How often limits are checked while the harness is quiet.
const TICK: Duration = Duration::from_millis(100);
/// How long a turn with no duration limit may be held open after the
/// harness answered it, for its background tasks and the harness's answer
/// to their notification ([`branchyard_harness::Driver::held`]), before
/// the hold is cut, unless [`Budget::hold_cap`] says otherwise. The turn
/// then ends with the outcome it was held with
/// ([`branchyard_harness::Driver::held_outcome`]), as it does at its
/// duration limit.
const HOLD_CAP: Duration = Duration::from_secs(30 * 60);
/// How long a held turn waits, with no background task running, for the
/// harness to start the cycle on an ended task's notification
/// ([`branchyard_harness::Driver::awaiting_follow_up`]) before the hold
/// ends, unless [`Budget::follow_up_grace`] says otherwise. Claude Code
/// starts one within about 2 seconds of the result, or never (a task the
/// model stopped, a notification it already read).
const FOLLOW_UP_GRACE: Duration = Duration::from_secs(10);
/// Events drained without waiting once the turn has ended.
const DRAIN_MAX: usize = 10_000;
/// The variables that name a temporary directory: POSIX's, and the two
/// that Python's `tempfile` and Windows-minded tools read first.
pub(crate) const TMP_VARIABLES: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

/// `name`'s private temporary directory, created readable by its owner
/// only. It lives as long as the branch; [`crate::ops::remove`] removes it.
pub(crate) fn private_tmp(store: &Store, name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = store.tmp(name);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}

/// One turn to run on a branch whose record is already written.
pub(crate) struct Turn<'a> {
    pub yard: &'a Yard,
    pub record: Record,
    pub profile: &'static Profile,
    pub command: Vec<String>,
    pub mode: SessionMode,
    pub prompt: &'a str,
    pub options: &'a TaskOptions,
    /// For a forked session: the parent's worktree, where its session began.
    pub fork_source: Option<PathBuf>,
    /// Recorded as a warning when the turn starts, such as why it starts a
    /// fresh session.
    pub note: Option<String>,
    /// Where the turn's sandbox comes from, when the caller already knows
    /// (a fan's prepared branch); by default, as the store says.
    pub sandbox: SandboxPlan,
}

/// How a turn ended, before the snapshot. Journaled as the `turn_end`
/// step's outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub(crate) enum End {
    Outcome {
        outcome: TurnOutcome,
    },
    /// Stopped at this budget limit: by the engine, or by the harness
    /// itself at the spending limit it was given.
    Budget {
        limit: String,
        /// The spending limit the harness stopped itself at: what was left
        /// of the branch's `max_usd` when the turn started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness_usd: Option<f64>,
    },
    /// Stopped at the request of this ancestor.
    Cancelled {
        by: String,
    },
    Failed {
        reason: String,
    },
    /// Its engine stopped before the turn's outcome was recorded; recovery
    /// says what is known.
    Lost {
        reason: String,
    },
    /// Interrupted by the engine after a stall; see
    /// [`crate::StallAction::Interrupt`].
    Stalled,
}

/// The journaled steps of a turn.
pub(crate) const STEP_START: &str = "start";
pub(crate) const STEP_SUBMIT: &str = "submit";
pub(crate) const STEP_TURN_END: &str = "turn_end";
pub(crate) const STEP_SNAPSHOT: &str = "snapshot";

/// The limits and policy a turn actually runs under: the caller's, narrowed
/// by whatever a delegating parent imposed on the branch.
struct Bounds {
    budget: Budget,
    policy: Policy,
}

pub(crate) struct Driven {
    pub end: End,
    pub submitted: bool,
    pub session: Option<NativeSession>,
    /// The harness's latest cumulative cost estimate.
    pub cost: Option<f64>,
    /// What this turn's calls through the model gateway cost, metered;
    /// when set, the branch's cost is this added to what it had, and the
    /// harness's estimate is not used.
    pub metered: Option<f64>,
    /// The branch's spend as last estimated while the turn ran, from the
    /// harness's per-call usage; its cost when the harness never reported
    /// a cumulative one.
    pub live_cost: Option<f64>,
    /// The descendants still running when the turn's hold began, if the
    /// engine cut the hold: what the turn's answer cannot have seen
    /// settle, which its branch is woken for ([`crate::wake::park`]).
    pub held_on: Vec<String>,
}

/// A running turn's spend: the branch's spend when the turn started, or
/// as of the harness's latest cumulative report, plus what each model
/// call since cost (the harness's per-call figure, or the catalog's price
/// of its tokens). The next cumulative report replaces the estimate.
struct LiveCost {
    anchor: f64,
    since: f64,
}

impl LiveCost {
    fn new(spent: f64) -> LiveCost {
        LiveCost {
            anchor: spent,
            since: 0.0,
        }
    }

    fn reported(&mut self, spent: f64) {
        self.anchor = spent;
        self.since = 0.0;
    }

    fn add(&mut self, cost: f64) {
        if cost.is_finite() && cost > 0.0 {
            self.since += cost;
        }
    }

    fn total(&self) -> f64 {
        self.anchor + self.since
    }
}

/// What one model call's tokens cost by the catalog, when the usage names
/// a model it prices.
fn priced(usage: &branchyard_harness::Usage) -> Option<f64> {
    let model = usage.model.as_deref()?;
    let tokens = crate::models::Tokens {
        input: usage.input_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
        cache_read: usage.cached_input_tokens.unwrap_or(0),
        cache_write: usage.cache_write_tokens.unwrap_or(0),
        cache_write_1h: usage.cache_write_1h_tokens.unwrap_or(0),
    };
    crate::models::pricing::cost(crate::models::Api::Generic, model, &tokens)
}

/// Whether an event goes in the branch's log: all but liveness, which
/// only moves the stall clock.
fn recorded(event: &Event) -> bool {
    !matches!(event, Event::Progress { .. })
}

enum Phase {
    Opening,
    Running(u64),
    Stopping {
        turn: u64,
        why: Stop,
        since: Instant,
    },
}

enum Stop {
    Limit(&'static str),
    Cancelled(String),
    Failure(String),
    /// Stopping after a stall, with [`Budget::stall_action`]
    /// [`StallAction::Interrupt`].
    Stall,
    /// Cutting the hold of a turn held open for background tasks after its
    /// answer, at [`HOLD_CAP`] or its duration limit: it ends with the
    /// outcome it is held with, not as interrupted or over a limit.
    Hold(TurnOutcome),
}

/// Whether a turn held open since `since` for background tasks has been
/// held too long: only without a duration limit (`deadline`), which bounds
/// the hold itself, after `cap`.
fn hold_cut(
    since: Option<Instant>,
    now: Instant,
    deadline: Option<Instant>,
    cap: Duration,
) -> bool {
    deadline.is_none() && since.is_some_and(|since| now.duration_since(since) >= cap)
}

/// Run the turn under `lease` and record its result. Harness failures
/// become the branch's status; errors are state errors, after which the
/// record says `Failed` if it could still be written. A lost lease is
/// [`Error::Fenced`], and nothing more is written.
pub(crate) fn execute(turn: Turn<'_>, lease: Lease) -> Result<Branch, Error> {
    let store = turn.yard.store();
    let fence = lease.fence().clone();
    let mut record = turn.record.clone();
    let mut recorder = Recorder::fenced(&store, &fence, turn.options.observer.clone());
    let bounds = Bounds {
        budget: delegation::effective_budget(&record, &turn.options.budget),
        // A branch writing its plan runs read-only, whatever the caller
        // passed; a delegating parent's denials still come first.
        policy: delegation::effective_policy(
            &record,
            &crate::plan::policy_for(&record, &turn.options.policy),
        ),
    };
    let result = (|| {
        recorder.record(Activity::Status(record.info.status.clone()))?;
        if let Some(note) = &turn.note {
            recorder.record(Activity::Warning(note.clone()))?;
        }
        if matches!(record.info.status, BranchStatus::Failed { .. }) {
            return Ok(());
        }
        if let Some(by) = store.backend().cancel_requested(&fence)? {
            recorder.record(Activity::Warning(format!(
                "cancelled by {by} before the turn started"
            )))?;
            record.info.status = BranchStatus::Interrupted;
        } else if let Some(limit) = exhausted(&store, &record, &bounds.budget) {
            record.info.status = BranchStatus::BudgetExceeded { limit };
        } else if let Err(reason) = match crate::placement::sandboxed(record.provider.as_ref()) {
            // A sandboxed branch's setup runs in its sandbox, in the turn.
            true => Ok(()),
            false => crate::workspace::prepare(
                turn.yard,
                &mut record,
                &fence,
                &mut recorder,
                &|| lease.lost(),
                &crate::workspace::Runner::Host,
                None,
            )?,
        } {
            // Setup that did not complete runs again on the next turn.
            record.info.status = match store.backend().cancel_requested(&fence)? {
                Some(_) => BranchStatus::Interrupted,
                None => BranchStatus::Failed { reason },
            };
        } else if let Err(reason) = graph::bind(turn.yard, &record) {
            record.info.status = BranchStatus::Failed { reason };
        } else {
            let driven = drive(&mut recorder, &turn, &mut record, &bounds, &lease);
            graph::unbind(turn.yard, &record);
            let driven = driven?;
            let driven_held_on = driven.held_on.clone();
            conclude(
                turn.yard,
                turn.prompt,
                &fence,
                &mut record,
                &mut recorder,
                driven,
            )?;
            // A delegating turn that ended while its children still run,
            // or whose cut hold they settled in, waits on them; see
            // `crate::wake`.
            crate::wake::park(
                turn.yard,
                &mut record,
                &mut recorder,
                &bounds.budget,
                &driven_held_on,
            )?;
        }
        Ok(())
    })();
    let result = match result {
        Ok(()) => {
            crate::wake::remember(turn.yard, &record, turn.options);
            recorder.finish(lease, &record)
        }
        Err(error) => Err(error),
    };
    let result = match result {
        Ok(()) => Ok(Branch {
            yard: turn.yard.clone(),
            info: record.info,
        }),
        Err(error @ Error::Fenced(_)) => Err(error),
        Err(error) => {
            record.info.status = BranchStatus::Failed {
                reason: error.to_string(),
            };
            branchyard_support::best_effort(
                "finish the turn's journal step",
                store.backend().finish(&fence, Some(&record), None),
            );
            Err(error)
        }
    };
    // Children its branch now contains are merged, however they got
    // there (a merge its harness ran, a checkpoint of an integration).
    crate::integrate::reconcile_children(turn.yard, &fence.branch);
    // Siblings waiting for this branch may start, or be blocked, now, and
    // a parked ancestor may wake.
    graph::settled(turn.yard, &fence.branch, Some(turn.options));
    // Its own children may all have settled before it parked.
    branchyard_support::best_effort(
        "wake a parked branch",
        crate::wake::look(turn.yard, &fence.branch, Some(turn.options)),
    );
    // The outcome store learns how the turn ended; best-effort, it never
    // changes what happened.
    if result.is_ok() {
        branchyard_support::best_effort(
            "record the fleet outcome",
            crate::fleet::observe(turn.yard, &fence.branch, None),
        );
        // A delegated child's plan awaiting approval goes to its parent.
        crate::plan::settled(turn.yard, &fence.branch);
        if matches!(&result, Ok(b) if b.info.status == BranchStatus::Ready) {
            crate::knowledge::on_end(
                turn.yard,
                &fence.branch,
                crate::knowledge::DistillTrigger::Ready,
            );
        }
    }
    result
}

/// Journal a step whose intent and outcome are recorded together, after
/// the fact.
fn journal(
    store: &Store,
    fence: &Fence,
    step: &str,
    intent: Value,
    outcome: &Value,
) -> Result<(), Error> {
    store
        .backend()
        .begin_step(fence, fence.turn, step, &intent)?;
    store
        .backend()
        .finish_step(fence, fence.turn, step, outcome)
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A limit already used up before the turn starts. Cost counts what the
/// branch's children reserved.
pub(crate) fn exhausted(
    store: &crate::state::Store,
    record: &Record,
    budget: &Budget,
) -> Option<String> {
    if budget.max_turns.is_some_and(|max| record.info.turns >= max) {
        return Some("max_turns".into());
    }
    if let Some(max) = budget.max_usd {
        let own = record.info.cost_usd.unwrap_or(0.0);
        if own + delegation::reserved(store, record) >= max {
            return Some("max_usd".into());
        }
    }
    None
}

/// Ask the harness to stop the turn in flight; its `TurnEnded` follows.
/// The driver may have no turn in flight any more although the engine has
/// not yet seen it end: the harness ended it in the same message that
/// prompted the stop (Claude Code's `result` carries the usage that went
/// over a limit and the turn's end together). That turn's `TurnEnded` is
/// already on its way, so this is not a failure.
fn stop(session: &mut Session) -> Result<(), RuntimeError> {
    match session.interrupt() {
        Err(RuntimeError::Rejected(Rejected::NoTurn)) => Ok(()),
        other => other,
    }
}

/// The branch's cost from its session's cumulative estimate: see
/// `Record::cost_baseline`.
fn spent(reported: f64, baseline: Option<f64>) -> f64 {
    (reported - baseline.unwrap_or(0.0)).max(0.0)
}

fn drive(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &mut Record,
    bounds: &Bounds,
    lease: &Lease,
) -> Result<Driven, Error> {
    let fence = lease.fence();
    let started = Instant::now();
    let store = turn.yard.store();
    let deadline = bounds
        .budget
        .max_duration
        .and_then(|limit| started.checked_add(limit));
    // The deadline is durable, so recovery can say whether it had passed.
    let deadline_ms = bounds
        .budget
        .max_duration
        .map(|limit| deadline_capped(now_ms(), limit));
    store.backend().set_deadline(fence, deadline_ms)?;
    let driven = run(recorder, turn, record, bounds, lease, started, deadline)?;
    journal(
        &store,
        fence,
        STEP_TURN_END,
        json!({ "submitted": driven.submitted }),
        &to_value(&driven.end),
    )?;
    Ok(driven)
}

fn run(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &mut Record,
    bounds: &Bounds,
    lease: &Lease,
    started: Instant,
    deadline: Option<Instant>,
) -> Result<Driven, Error> {
    let fence = lease.fence();
    let store = turn.yard.store();
    let mut driven = Driven {
        end: End::Failed {
            reason: String::new(),
        },
        submitted: false,
        session: None,
        cost: None,
        metered: None,
        live_cost: None,
        held_on: Vec::new(),
    };
    let sandboxed = crate::placement::sandboxed(record.provider.as_ref());
    // Revoked when this function returns, after the harness and its
    // sandbox are gone. The delegation tools reach the engine over a host
    // socket with the host's `by`, so a sandboxed harness does not get them
    // yet.
    let projection = match sandboxed {
        true => Err(Error::Unsupported(
            "delegation is not yet available to a sandboxed harness".into(),
        )),
        false => crate::projection::project(
            turn.yard,
            record,
            turn.options,
            bounds.budget.clone(),
            bounds.policy.clone(),
        ),
    };
    let projection = match projection {
        Ok(projection) => projection,
        // Asked for now: the turn cannot run as asked.
        Err(error) if turn.options.delegation.is_some() => {
            driven.end = End::failed(format!("could not offer delegation: {error}"));
            return Ok(driven);
        }
        // Kept from an earlier turn: run without the tools, and say so.
        Err(error) if record.grant.is_some() => {
            recorder.record(Activity::Warning(format!(
                "this turn runs without delegation tools: {error}"
            )))?;
            None
        }
        Err(_) => None,
    };
    // The branch's connectors: packages, index and this turn's gateway
    // token in its home, before the sandbox exists. The token file is
    // removed, and the gateway's audit log read a last time, when this
    // function returns.
    let deadline_ms =
        deadline.map(|at| deadline_capped(now_ms(), at.saturating_duration_since(Instant::now())));
    // One scope: the person's ceiling over what the branch asked for,
    // which its connectors, models, network and token all follow.
    let (scoped, narrowed) = crate::access::scoped(turn.yard, record);
    if let Some(narrowed) = narrowed {
        recorder.record(Activity::Access(Box::new(narrowed)))?;
    }
    let scopes = crate::access::TokenScopes::of(&scoped);
    let mut connectors = match crate::connectors::prepare(turn.yard, &scoped, deadline_ms, &scopes)
    {
        Ok(connectors) => connectors,
        Err(reason) => {
            driven.end = End::failed(format!("could not provide connectors: {reason}"));
            return Ok(driven);
        }
    };
    for warning in connectors.iter().flat_map(|prepared| &prepared.warnings) {
        recorder.record(Activity::Warning(warning.clone()))?;
    }
    // The effect ledger's proxy in front of the gateway: every effectful
    // call is decided and written to the ledger before it is made. It
    // stops when this function returns, after the harness is gone.
    let _effects = match connectors.as_mut() {
        Some(prepared) => match effects_proxy(
            recorder,
            turn,
            &scoped,
            prepared,
            deadline_ms,
            bounds.policy.preset(),
        ) {
            Ok(proxy) => proxy,
            Err(reason) => {
                driven.end = End::failed(format!("could not start the effect ledger: {reason}"));
                return Ok(driven);
            }
        },
        None => None,
    };
    // The model gateway, on the same token when the turn has one; it
    // stops when this function returns, after the harness is gone.
    let models = match crate::models::prepare(
        turn.yard,
        &scoped,
        connectors.as_ref().map(|c| c.token.as_str()),
        deadline_ms,
        &scopes,
        bounds.budget.clone(),
        delegation::reserved(&store, record),
    ) {
        Ok(models) => models,
        Err(reason) => {
            driven.end = End::failed(format!("could not provide the model gateway: {reason}"));
            return Ok(driven);
        }
    };
    let mut model_gateway = None;
    let mut egress_extra = Vec::new();
    let mut model_env = Vec::new();
    let mut model_scrub = Vec::new();
    match models {
        None => {}
        Some(crate::models::Prepared::Direct { hosts, activity }) => {
            recorder.record(Activity::Model(Box::new(activity)))?;
            egress_extra.extend(hosts);
        }
        Some(crate::models::Prepared::Gateway {
            gateway,
            env,
            scrub,
            url,
            activity,
        }) => {
            recorder.record(Activity::Model(Box::new(activity)))?;
            egress_extra.extend(crate::egress::gateway_rule(&url));
            model_env = env;
            model_scrub = scrub;
            model_gateway = Some(*gateway);
        }
    }
    let _audit = connectors
        .as_ref()
        .map(|_| crate::connectors::AuditTail::start(turn.yard));
    // The one path for MCP servers and instructions, the task's and the
    // delegation tools', and for everything else the home needs. Applied
    // before a sandbox exists, so its mount or home transfer carries it.
    // Adopted repository knowledge that matches the branch, in its
    // instructions; see `crate::knowledge`.
    let briefing = crate::knowledge::briefing_for(turn.yard, record);
    if !briefing.omitted.is_empty() {
        recorder.record(Activity::Warning(format!(
            "knowledge {} matched but did not fit the {}-token budget",
            briefing
                .omitted
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", "),
            turn.yard.knowledge_settings().budget_tokens
        )))?;
    }
    let provisioned = match crate::provisioning::prepare(
        record,
        turn.profile,
        projection.as_ref(),
        connectors.as_ref(),
        Some(&briefing),
        store.dir(),
    ) {
        Ok(provisioned) => provisioned,
        Err(reason) => {
            driven.end = End::failed(format!("could not provision {}: {reason}", turn.profile.id));
            return Ok(driven);
        }
    };
    if let Some(activity) = provisioned.activity {
        recorder.record(activity)?;
    }
    // Declared before the session so a sandbox outlives it.
    let mut placement = match Placement::prepare(turn.yard, record, fence, &turn.sandbox) {
        Ok(placement) => placement,
        Err(reason) => {
            driven.end = End::failed(reason);
            return Ok(driven);
        }
    };
    if let Some(started) = placement.started() {
        recorder.record(crate::snapshots::event(started.clone()))?;
        // A seed is used once: later turns resume the kept sandbox or
        // start fresh.
        if record.sandbox_seed.take().is_some() {
            store.write_fenced(record, fence)?;
        }
        // The branch's workspace, set up in its sandbox, or inherited from
        // the branch whose sandbox this one was branched from.
        if let Some(end) = sandbox_setup(recorder, turn, record, lease, &placement, &started)? {
            if let Some(warning) = placement.discard() {
                recorder.record(Activity::Warning(warning))?;
            }
            driven.end = end;
            return Ok(driven);
        }
    }
    // A fresh session's cumulative cost starts again from nothing (after a
    // rewind, or a lost session), and a resumed one other than the
    // baseline's from its own total when it was left, lower than the
    // latest session's when a rewind went back to an older one: the
    // branch's cost is what it had spent before, and what the session's
    // total adds to it. The session left keeps its total, what the branch
    // has counted of it. A turn that resumes the baseline's own session,
    // and a fork's first (its baseline was set when it was forked), keep
    // the baseline. Written at once, so a turn cut off and resumed reads
    // the session's total the same way.
    let resumed = match &turn.mode {
        SessionMode::Fresh => None,
        SessionMode::Resume(session) => Some(session.as_str().to_owned()),
        SessionMode::Fork(_) => record.cost_session.clone(),
    };
    let leaving = turn.mode == SessionMode::Fresh || resumed != record.cost_session;
    if leaving {
        let before = record.info.cost_usd.unwrap_or(0.0);
        if let Some(left) = record.cost_session.take() {
            let total = before + record.cost_baseline.unwrap_or(0.0);
            record.session_costs.insert(left, total);
        }
        record.cost_baseline = match &resumed {
            None => record.info.cost_usd.map(|cost| -cost),
            Some(session) => match record.session_costs.get(session) {
                Some(total) => Some(total - before),
                None => record.cost_baseline,
            },
        };
        record.cost_session = resumed;
        store.write_fenced(record, fence)?;
    }
    let record: &Record = record;
    // Every local harness learns which branch it is on, so `by` inside it
    // never mistakes it for a person, and finds `by` itself; only a
    // delegated one gets a token.
    if !placement.is_sandbox() {
        placement.set_env(ENV_ROOT, &turn.yard.root.display().to_string());
        placement.set_env(ENV_BRANCH, &record.info.name);
        if let Some(by) = crate::projection::by_path(turn.options) {
            for (name, value) in crate::projection::by_env(&by) {
                placement.set_env(&name, &value);
            }
        }
        placement.set_env(
            crate::workspace::ENV_WORKTREE,
            &record.info.worktree.display().to_string(),
        );
        if let Some(port) = store.ports().port(&record.info.name)? {
            placement.set_env(crate::workspace::ENV_PORT, &port.to_string());
        }
        // A local process runs directly on the host filesystem, so every
        // scratch area this branch may reach is simply its host directory;
        // see `docs/storage.md`. Microsandbox gets these as mounts instead
        // (`placement::scratch_mounts`); Substrate gets none (it has no
        // host mounts and the worktree crosses as a bundle, not scratch
        // areas too).
        if let Ok(areas) = crate::storage::authorized_scratch(turn.yard, &record.info.name) {
            for area in areas {
                let path = crate::storage::scratch_dir(&store, &area.name);
                placement.set_env(
                    &crate::storage::scratch_env_var(&area.name),
                    &path.display().to_string(),
                );
            }
        }
    }
    if let Some(projection) = &projection {
        for (name, value) in &projection.env {
            placement.set_env(name, value);
        }
    }
    // A secret's source variable is not the harness's: it gets the secret
    // only as its plan delivers it.
    for name in &provisioned.scrub {
        placement.remove_env(name);
    }
    for var in &provisioned.env {
        placement.set_env(&var.name, &var.value);
    }
    // The model gateway's variables replace the provider's own, and the
    // credentials that would reach the provider around it are taken out.
    for name in &model_scrub {
        placement.remove_env(name);
    }
    for (name, value) in &model_env {
        placement.set_env(name, value);
    }
    if let Some(parent) = &turn.options.trace_parent {
        placement.set_env(crate::ENV_TRACEPARENT, parent);
    }
    // The branch's network policy: its proxy's variables last, so nothing
    // above replaces them, and its namespace when the harness starts. The
    // proxy lives with the placement, after the harness is gone.
    if let Some(gateway) = connectors
        .as_ref()
        .and_then(|c| crate::egress::gateway_rule(&c.gateway_url))
    {
        egress_extra.push(gateway);
    }
    match crate::egress::prepare(turn.yard, &scoped, egress_extra) {
        Ok(None) => {}
        Ok(Some(egress)) => {
            recorder.record(egress.applied())?;
            placement.egress(egress);
        }
        Err(reason) => {
            driven.end = End::failed(format!("could not apply the network policy: {reason}"));
            return Ok(driven);
        }
    }
    // A per-turn MCP file lives until this function returns, after the
    // harness is gone.
    let _turn_file = provisioned.turn_file;
    let driver = turn.profile.driver_with(turn.command.clone());
    // A harness that can hold a spending limit itself is given what is
    // left of the branch's, so it stops before it goes over rather than
    // after it reports; the engine's own check below still holds. Not on
    // the model gateway, which refuses an over-budget call itself and
    // whose metered cost replaces the harness's own.
    let self_limit = driver.capabilities().budget && model_gateway.is_none();
    let max_budget_usd = match (self_limit, bounds.budget.max_usd) {
        (true, Some(max)) => {
            Some(max - record.info.cost_usd.unwrap_or(0.0) - delegation::reserved(&store, record))
                .filter(|left| *left > 0.0)
        }
        _ => None,
    };
    let open = Open {
        max_budget_usd,
        mcp_servers: provisioned.session.mcp_servers,
        instructions: provisioned.session.instructions,
        model: provisioned.session.model,
        mcp_config_file: provisioned.mcp_config_file,
        remote_mcp_servers: provisioned.session.remote_mcp_servers,
        ..Open::new(turn.mode.clone(), placement.cwd())
    };
    let mut steering = Steering::new(
        turn.profile.id,
        driver.capabilities().steer,
        driver.steer_boundary(),
    );
    // Journaled before the spawn: every process of a local harness carries
    // this marker, so recovery finds them even if this engine stops before
    // the harness's pid is recorded below.
    let spawn = (!placement.is_sandbox()).then(|| {
        format!(
            "{}-{}-{}",
            store.owner().id,
            fence.incarnation,
            fence.generation
        )
    });
    if let Some(spawn) = &spawn {
        placement.set_env(crate::proc::ENV_SPAWN, spawn);
    }
    // A local harness gets a temporary directory of its own, so what one
    // branch leaves in a shared `/tmp` cannot shadow another's files (a
    // stray `/tmp/inspect.py` once shadowed Python's `inspect`). A
    // sandbox has a `/tmp` of its own already.
    if !placement.is_sandbox() {
        match private_tmp(&store, &record.info.name) {
            Ok(dir) => {
                let dir = dir.display().to_string();
                for name in TMP_VARIABLES {
                    placement.set_env(name, &dir);
                }
            }
            Err(error) => {
                driven.end =
                    End::failed(format!("could not create its temporary directory: {error}"));
                return Ok(driven);
            }
        }
    }
    let intent = json!({
        "command": turn.command,
        "sandbox": placement.is_sandbox(),
        "spawn": spawn,
        "host": crate::proc::host(),
    });
    store
        .backend()
        .begin_step(fence, fence.turn, STEP_START, &intent)?;
    let mut session = match placement.start(driver, open) {
        Ok(session) => session,
        Err(error) => {
            let reason = match error {
                RuntimeError::Spawn { argv, source } => {
                    format!("could not start {}: {source}", argv.join(" "))
                }
                RuntimeError::Rejected(rejected) => format!("the driver refused: {rejected}"),
                other => other.to_string(),
            };
            let outcome = json!({ "error": reason });
            store
                .backend()
                .finish_step(fence, fence.turn, STEP_START, &outcome)?;
            driven.end = End::failed(reason);
            return Ok(driven);
        }
    };
    // A local harness leads its own process group; recovery kills that
    // group if this engine stops, when pid and start time still match.
    let identity = match placement.is_sandbox() {
        true => None,
        false => session.process_id().parse::<u32>().ok().and_then(|pid| {
            Some(ProcessRow {
                pid,
                pgid: pid,
                start: crate::proc::start_time(pid)?,
                host: crate::proc::host().to_owned(),
            })
        }),
    };
    if let Some(process) = &identity {
        store.backend().record_process(fence, process)?;
    }
    let outcome = match &identity {
        Some(p) => json!({ "pid": p.pid, "pgid": p.pgid, "start": p.start }),
        None => json!({ "process": session.process_id() }),
    };
    store
        .backend()
        .finish_step(fence, fence.turn, STEP_START, &outcome)?;
    let mut phase = Phase::Opening;
    // Whether the harness has named the session it runs. Until it has, a
    // resume or fork may have landed somewhere else.
    let mut confirmed = false;
    let mut kill = false;
    // Stall detection: `last_activity` moves forward on every protocol
    // event, including a permission answer. It is never checked while the
    // engine itself is blocked (delivering a permission answer synchronously
    // stops the loop from ticking at all), while a child branch is running
    // (`delegation::any_child_running`), or while the harness is blocked in
    // `ask --wait` for an answer (`inbox::waiting_for_answer`, recorded in
    // the store by whichever process runs the wait), nor while the driver
    // holds the turn open after its answer for background tasks the
    // harness still reports running (a long command in the background says
    // nothing); that hold is bounded by the turn's duration limit, or else
    // by `HOLD_CAP`.
    let mut last_activity = started;
    let mut stalled = false;
    let mut held_since: Option<Instant> = None;
    let hold_cap = bounds.budget.hold_cap.unwrap_or(HOLD_CAP);
    let mut awaiting_since: Option<Instant> = None;
    let mut held_on: Vec<String> = Vec::new();
    let mut cut_hold = false;
    let follow_up_grace = bounds.budget.follow_up_grace.unwrap_or(FOLLOW_UP_GRACE);
    // What the branch has spent, while the turn runs: see `LiveCost`.
    // Written to its record as it changes, so a parent inspecting it, or
    // the branch inspecting itself, sees a figure and not "unknown".
    let mut live = LiveCost::new(record.info.cost_usd.unwrap_or(0.0));
    let shown: Cell<Option<f64>> = Cell::new(None);
    let write = |stalled: Option<bool>| -> Result<(), Error> {
        let mut updated = record.clone();
        if let Some(value) = stalled {
            updated.info.stalled = value;
        }
        if let Some(cost) = shown.get() {
            updated.info.cost_usd = Some(cost);
        }
        store.write_fenced(&updated, fence)
    };
    let write_stalled = |value: bool| write(Some(value));
    let fail = |confirmed: bool, detail: String| -> End {
        End::failed(match (&turn.mode, confirmed) {
            (SessionMode::Fresh, _) | (_, true) => detail,
            (SessionMode::Resume(session), false) => format!(
                "could not resume session {session} in {}: {detail}",
                record.info.worktree.display()
            ),
            (SessionMode::Fork(session), false) => format!(
                "could not fork session {session} into {}: {detail}. The session began in {}; \
                 {} may keep sessions per working directory, in which case it cannot be forked \
                 into another worktree. Fork with a fresh session instead.",
                record.info.worktree.display(),
                turn.fork_source
                    .as_deref()
                    .map_or_else(|| "another worktree".into(), |p| p.display().to_string()),
                turn.profile.harness,
            ),
        })
    };

    let end = loop {
        let now = Instant::now();
        let late = deadline.is_some_and(|deadline| now >= deadline);
        if lease.lost() {
            kill = true;
            break End::failed(format!(
                "this engine lost {}'s lease to another; it stopped the turn",
                record.info.name
            ));
        }
        let cancelled = match phase {
            Phase::Stopping { .. } => None,
            _ => store.backend().cancel_requested(fence).unwrap_or(None),
        };
        match (&phase, cancelled) {
            (Phase::Opening, Some(by)) => {
                kill = true;
                break End::cancelled(by);
            }
            (Phase::Running(n), Some(by)) => {
                let n = *n;
                if let Err(error) = stop(&mut session) {
                    recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                    kill = true;
                    break End::cancelled(by);
                }
                phase = Phase::Stopping {
                    turn: n,
                    why: Stop::Cancelled(by),
                    since: now,
                };
            }
            _ => {}
        }
        steering.poll(recorder, &mut session, &store, fence, &phase)?;
        let holding = session.holding();
        // The cap covers the whole hold, the wait for the follow-up included.
        if session.held() && held_since.is_none() {
            held_on = delegation::descendants(&store, &record.info.name)
                .unwrap_or_default()
                .into_iter()
                .filter(|info| crate::wake::unsettled(&info.status))
                .map(|info| info.name)
                .collect();
        }
        held_since = session.held().then(|| held_since.unwrap_or(now));
        awaiting_since = session
            .awaiting_follow_up()
            .then(|| awaiting_since.unwrap_or(now));
        // A turn on the model gateway is metered exactly; its limit is
        // held here as a harness's own estimate is below.
        if let Some(gateway) = &model_gateway {
            let metered = gateway.metered();
            if driven.metered != Some(metered) && metered > 0.0 {
                driven.metered = Some(metered);
                let own = record.info.cost_usd.unwrap_or(0.0) + metered;
                if let Some(projection) = &projection {
                    projection.observe_cost(own);
                }
                let over = bounds
                    .budget
                    .max_usd
                    .is_some_and(|max| own + delegation::reserved(&store, record) > max);
                if let (true, Phase::Running(n)) = (over, &phase) {
                    let n = *n;
                    if let Err(error) = stop(&mut session) {
                        recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                        kill = true;
                        break End::budget("max_usd");
                    }
                    phase = Phase::Stopping {
                        turn: n,
                        why: Stop::Limit("max_usd"),
                        since: Instant::now(),
                    };
                }
            }
        }
        if let (Phase::Running(n), Some(window)) = (&phase, bounds.budget.stall_after) {
            let n = *n;
            let idle = now.duration_since(last_activity);
            if !stalled
                && idle >= window
                && !holding
                && !delegation::any_child_running(&store, &record.info.name)
                && !crate::inbox::waiting_for_answer(&store, &record.info.name)
            {
                stalled = true;
                let since_ms = now_ms().saturating_sub(idle.as_millis() as u64);
                recorder.record(Activity::Stalled { since_ms })?;
                write_stalled(true)?;
                if bounds.budget.stall_action == StallAction::Interrupt {
                    if let Err(error) = stop(&mut session) {
                        recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                        kill = true;
                        break End::Stalled;
                    }
                    phase = Phase::Stopping {
                        turn: n,
                        why: Stop::Stall,
                        since: now,
                    };
                }
            }
        }
        match &phase {
            Phase::Opening if late => {
                kill = true;
                break End::budget("max_duration");
            }
            Phase::Opening if now.duration_since(started) >= READY_TIMEOUT => {
                kill = true;
                break fail(
                    confirmed,
                    format!("no handshake within {}s", READY_TIMEOUT.as_secs()),
                );
            }
            Phase::Running(n) if late => {
                let n = *n;
                // A held turn has its answer: the limit cuts the hold, and
                // the turn keeps the outcome it is held with.
                let why = match session.held_outcome() {
                    Some(outcome) => {
                        recorder.record(Activity::Warning(
                            "the turn reached its duration limit while held open for the \
                             harness's background tasks; ending the hold, the turn keeps its \
                             outcome"
                                .into(),
                        ))?;
                        Stop::Hold(outcome)
                    }
                    None => Stop::Limit("max_duration"),
                };
                cut_hold = matches!(why, Stop::Hold(_));
                if let Err(error) = stop(&mut session) {
                    recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                    kill = true;
                    break match why {
                        Stop::Hold(outcome) => End::Outcome { outcome },
                        _ => End::budget("max_duration"),
                    };
                }
                phase = Phase::Stopping {
                    turn: n,
                    why,
                    since: now,
                };
            }
            Phase::Running(n) if hold_cut(held_since, now, deadline, hold_cap) => {
                let n = *n;
                recorder.record(Activity::Warning(format!(
                    "the turn was held open {} for the harness's background tasks, with no \
                     duration limit; ending the hold, the turn keeps its outcome",
                    branchyard_support::time::human_duration(hold_cap)
                )))?;
                let outcome = session.held_outcome().unwrap_or(TurnOutcome::Interrupted);
                cut_hold = true;
                if let Err(error) = stop(&mut session) {
                    recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                    kill = true;
                    break End::Outcome { outcome };
                }
                phase = Phase::Stopping {
                    turn: n,
                    why: Stop::Hold(outcome),
                    since: now,
                };
            }
            Phase::Running(n)
                if awaiting_since
                    .is_some_and(|since| now.duration_since(since) >= follow_up_grace) =>
            {
                let n = *n;
                recorder.record(Activity::Warning(format!(
                    "the harness started no cycle on its ended background tasks' notification \
                     within {} of the turn's hold; ending the hold, the turn keeps its outcome",
                    branchyard_support::time::human_duration(follow_up_grace)
                )))?;
                let outcome = session.held_outcome().unwrap_or(TurnOutcome::Interrupted);
                cut_hold = true;
                if let Err(error) = stop(&mut session) {
                    recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                    kill = true;
                    break End::Outcome { outcome };
                }
                phase = Phase::Stopping {
                    turn: n,
                    why: Stop::Hold(outcome),
                    since: now,
                };
            }
            Phase::Stopping { why, since, .. } if now.duration_since(*since) >= INTERRUPT_GRACE => {
                recorder.record(Activity::Warning(format!(
                    "the harness did not end the turn within {}s of the interrupt; killing it",
                    INTERRUPT_GRACE.as_secs()
                )))?;
                kill = true;
                break match why {
                    Stop::Limit(limit) => End::budget(*limit),
                    Stop::Cancelled(by) => End::cancelled(by.clone()),
                    Stop::Failure(reason) => End::failed(reason.clone()),
                    Stop::Stall => End::Stalled,
                    Stop::Hold(outcome) => End::Outcome {
                        outcome: outcome.clone(),
                    },
                };
            }
            _ => {}
        }

        let event = match session.next_event(TICK) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(RuntimeError::HarnessExited { stderr }) => {
                let detail = match stderr.is_empty() {
                    true => "the harness exited".to_owned(),
                    false => format!("the harness exited: {stderr}"),
                };
                break fail(confirmed, detail);
            }
            Err(error) => break fail(confirmed, error.to_string()),
        };
        // Liveness only: it moves the stall clock and is not recorded.
        if recorded(&event) {
            recorder.record(Activity::Harness(event.clone()))?;
            steering.event(recorder, &store, fence, &event)?;
        }
        last_activity = Instant::now();
        if stalled && matches!(phase, Phase::Running(_)) {
            stalled = false;
            recorder.record(Activity::Resumed)?;
            write_stalled(false)?;
        }
        match event {
            Event::Ready if matches!(phase, Phase::Opening) => {
                // Pending inbox messages are prepended here, at the last
                // point before the prompt may reach the harness. Journaled
                // first, and the messages it carries acknowledged (marked
                // delivered) in the same transaction: from here the prompt
                // may have reached the harness, and recovery must never
                // submit it or its messages again; a failure before leaves
                // them pending (see `crate::inbox`).
                let submission =
                    crate::inbox::begin_submit(&store, fence, &record.info.name, turn.prompt)?;
                if !submission.delivered.is_empty() {
                    recorder.record(Activity::MessagesDelivered {
                        ids: submission.delivered.clone(),
                        via: DeliveredVia::TurnStart {
                            boundary: "turn_start".to_owned(),
                        },
                    })?;
                }
                recorder.record(Activity::Prompt(submission.prompt.clone()))?;
                match session.submit(&submission.prompt) {
                    Ok(n) => {
                        driven.submitted = true;
                        store.backend().finish_step(
                            fence,
                            fence.turn,
                            STEP_SUBMIT,
                            &json!({ "turn": n }),
                        )?;
                        phase = Phase::Running(n);
                    }
                    // Refused with nothing written: the prompt never
                    // reached the harness, so its messages are pending
                    // again for the next turn.
                    Err(error @ RuntimeError::Rejected(_)) => {
                        crate::inbox::abandon_submit(&store, fence, &submission)?;
                        if !submission.delivered.is_empty() {
                            recorder.record(Activity::Warning(format!(
                                "the harness refused the prompt; messages {:?} are pending again",
                                submission.delivered
                            )))?;
                        }
                        break fail(confirmed, format!("submit failed: {error}"));
                    }
                    Err(error) => break fail(confirmed, format!("submit failed: {error}")),
                }
            }
            Event::OpenFailed { reason } => {
                break fail(confirmed, format!("open failed: {reason}"))
            }
            Event::SessionStarted { session: id, .. } => {
                confirmed = true;
                driven.session = Some(id);
            }
            Event::ProtocolViolation { detail }
                if !confirmed && turn.mode != SessionMode::Fresh =>
            {
                // Continuing would run the prompt in a session other than
                // the one asked for.
                kill = true;
                break fail(confirmed, format!("protocol violation: {detail}"));
            }
            Event::PermissionRequested { request, .. } => {
                let stopping = matches!(phase, Phase::Stopping { .. });
                let cancelled = || {
                    lease.lost()
                        || store
                            .backend()
                            .cancel_requested(fence)
                            .ok()
                            .flatten()
                            .is_some()
                };
                let answered = answer(
                    recorder,
                    &mut session,
                    turn,
                    &bounds.policy,
                    &request,
                    stopping,
                    (deadline_ms, &cancelled),
                )?;
                // The policy may have blocked delivering this answer for a
                // while (an `--ask` prompt, say); that time is never a
                // stall, so the idle window starts over from here rather
                // than from when the request first arrived.
                last_activity = Instant::now();
                if let Some(reason) = answered {
                    if let Phase::Running(n) = phase {
                        if stop(&mut session).is_err() {
                            kill = true;
                            break End::failed(reason);
                        }
                        phase = Phase::Stopping {
                            turn: n,
                            why: Stop::Failure(reason),
                            since: Instant::now(),
                        };
                    }
                }
            }
            Event::UsageObserved { usage, .. } => {
                // On the model gateway, the metered cost is the branch's.
                if model_gateway.is_some() {
                    continue;
                }
                if usage.cumulative {
                    let Some(cost) = usage.cost_usd else { continue };
                    driven.cost = Some(driven.cost.map_or(cost, |c: f64| c.max(cost)));
                    live.reported(spent(driven.cost.unwrap_or(cost), record.cost_baseline));
                } else {
                    // One model call's: the harness's own figure, or the
                    // catalog's price of its tokens.
                    let Some(cost) = usage.cost_usd.or_else(|| priced(&usage)) else {
                        continue;
                    };
                    live.add(cost);
                }
                let own = live.total();
                if shown.get() != Some(own) {
                    shown.set(Some(own));
                    write(None)?;
                }
                if let Some(projection) = &projection {
                    projection.observe_cost(own);
                }
                let over = bounds
                    .budget
                    .max_usd
                    .is_some_and(|max| own + delegation::reserved(&store, record) > max);
                if let (true, Phase::Running(n)) = (over, &phase) {
                    let n = *n;
                    if let Err(error) = stop(&mut session) {
                        recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                        kill = true;
                        break End::budget("max_usd");
                    }
                    phase = Phase::Stopping {
                        turn: n,
                        why: Stop::Limit("max_usd"),
                        since: Instant::now(),
                    };
                }
            }
            Event::TurnEnded { turn: n, outcome } if phase_turn(&phase) == Some(n) => {
                let stopped_itself = max_budget_usd.is_some()
                    && matches!(&outcome, TurnOutcome::LimitReached { limit }
                        if limit == branchyard_harness::BUDGET_LIMIT);
                break match phase {
                    // The harness stopped at the limit it was given, which
                    // its last cost report may also have crossed.
                    Phase::Running(_)
                    | Phase::Stopping {
                        why: Stop::Limit("max_usd"),
                        ..
                    } if stopped_itself => End::Budget {
                        limit: "max_usd".into(),
                        harness_usd: max_budget_usd,
                    },
                    Phase::Stopping {
                        why: Stop::Limit(limit),
                        ..
                    } => End::budget(limit),
                    Phase::Stopping {
                        why: Stop::Cancelled(by),
                        ..
                    } => End::cancelled(by),
                    Phase::Stopping {
                        why: Stop::Failure(reason),
                        ..
                    } => End::failed(reason),
                    Phase::Stopping {
                        why: Stop::Stall, ..
                    } => End::Stalled,
                    // The hold was cut: the turn keeps the outcome it was
                    // held with, whatever the cycle the interrupt stopped
                    // ended with.
                    Phase::Stopping {
                        why: Stop::Hold(held),
                        ..
                    } => End::Outcome { outcome: held },
                    _ => End::Outcome { outcome },
                };
            }
            Event::OutcomeUnknown { turn: n, reason } if phase_turn(&phase) == Some(n) => {
                break fail(
                    confirmed,
                    format!("the turn's outcome is unknown: {reason}"),
                );
            }
            _ => {}
        }
    };
    driven.end = end;
    driven.live_cost = shown.get();
    if cut_hold {
        driven.held_on = held_on;
    }

    if !kill {
        // Whatever the harness already sent after the turn ended.
        for _ in 0..DRAIN_MAX {
            match session.next_event(Duration::ZERO) {
                Ok(Some(event)) if recorded(&event) => {
                    recorder.record(Activity::Harness(event.clone()))?;
                    steering.event(recorder, &store, fence, &event)?;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    }
    steering.finish(recorder, &store, fence)?;
    if let Some(id) = session.session_id() {
        driven.session = Some(id.clone());
    }
    if kill {
        match session.kill() {
            Ok(events) => {
                for event in events.into_iter().filter(recorded) {
                    recorder.record(Activity::Harness(event))?;
                }
            }
            Err(error) => recorder.record(Activity::Warning(format!("kill failed: {error}")))?,
        }
        for activity in placement.release(turn.yard, record, fence) {
            recorder.record(activity)?;
        }
        if let Some(gateway) = model_gateway {
            driven.metered = Some(gateway.finish());
        }
        return Ok(driven);
    }
    match session.close(CLOSE_GRACE) {
        Ok(closed) => {
            for event in closed.events.into_iter().filter(recorded) {
                recorder.record(Activity::Harness(event))?;
            }
            if closed.forced {
                recorder.record(Activity::Warning(format!(
                    "the harness did not exit within {}s of closing its input and was killed",
                    CLOSE_GRACE.as_secs()
                )))?;
            }
            if !closed.survivors.is_empty() {
                recorder.record(Activity::Warning(format!(
                    "processes outlived the harness and were killed: {}",
                    closed.survivors.join(", ")
                )))?;
            }
            if let Some(cost) = closed.cost_usd {
                driven.cost = Some(driven.cost.map_or(cost, |c| c.max(cost)));
            }
        }
        Err(error) => recorder.record(Activity::Warning(format!("close failed: {error}")))?,
    }
    for activity in placement.release(turn.yard, record, fence) {
        recorder.record(activity)?;
    }
    // Calls still in flight are given a moment; what the turn's calls
    // cost is the branch's.
    if let Some(gateway) = model_gateway {
        driven.metered = Some(gateway.finish());
    }
    Ok(driven)
}

/// Run a sandboxed branch's workspace setup in its sandbox, or take it
/// from the branch its sandbox was branched from. `Some(end)` when the turn
/// cannot go on.
fn sandbox_setup(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &mut Record,
    lease: &Lease,
    placement: &Placement,
    started: &crate::SandboxEvent,
) -> Result<Option<End>, Error> {
    let Some((provider, name)) = placement.sandbox() else {
        return Ok(None);
    };
    if record.workspace.as_ref().is_none_or(|w| w.ready) {
        return Ok(None);
    }
    let fence = lease.fence();
    let store = turn.yard.store();
    let mounted = placement.mounts_worktree();
    let inherit = match started {
        crate::SandboxEvent::Started { origin, .. } => {
            crate::snapshots::inherited(turn.yard, record, origin, mounted)
        }
        _ => None,
    };
    let runner = crate::workspace::Runner::Sandbox {
        provider,
        name,
        cwd: placement.cwd(),
        mounted,
    };
    match crate::workspace::prepare(
        turn.yard,
        record,
        fence,
        recorder,
        &|| lease.lost(),
        &runner,
        inherit.as_ref(),
    )? {
        Ok(()) => Ok(None),
        Err(reason) => Ok(Some(match store.backend().cancel_requested(fence)? {
            Some(by) => End::Cancelled { by },
            None => End::Failed { reason },
        })),
    }
}

/// This turn's steered input: polled from the store at most every
/// [`TICK`], written to the harness while the turn runs, and settled as the
/// harness answers.
struct Steering {
    profile: &'static str,
    /// Whether the driver can take input mid-turn at all.
    offered: bool,
    /// The protocol boundary a delivered steer landed at
    /// ([`branchyard_harness::Driver::steer_boundary`]), recorded on
    /// [`DeliveredVia::Steer`].
    boundary: &'static str,
    /// Store IDs of the input written to the driver, in the driver's
    /// numbering from 1.
    written: Vec<u64>,
    next_poll: Instant,
}

impl Steering {
    fn new(profile: &'static str, offered: bool, boundary: &'static str) -> Steering {
        Steering {
            profile,
            offered,
            boundary,
            written: Vec::new(),
            next_poll: Instant::now(),
        }
    }

    fn refuse(
        recorder: &mut Recorder,
        store: &Store,
        fence: &Fence,
        id: u64,
        by: &str,
        reason: String,
    ) -> Result<(), Error> {
        recorder.record(Activity::Warning(format!(
            "steered input {id} from {by} was not delivered: {reason}"
        )))?;
        store
            .backend()
            .settle_steer(fence, id, &SteerState::Refused { reason })
            .map(|_| ())
    }

    /// Record that steered input `steer` delivered inbox message `message`,
    /// naming the protocol boundary it landed at.
    fn delivered(
        &self,
        recorder: &mut Recorder,
        steer: u64,
        message: Option<u64>,
    ) -> Result<(), Error> {
        match message {
            Some(id) => recorder.record(Activity::MessagesDelivered {
                ids: vec![id],
                via: DeliveredVia::Steer {
                    steer,
                    boundary: self.boundary.to_owned(),
                },
            }),
            None => Ok(()),
        }
    }

    /// Write pending input into a running turn. Before the prompt is
    /// submitted it waits; while the turn is being stopped it is refused.
    fn poll(
        &mut self,
        recorder: &mut Recorder,
        session: &mut Session,
        store: &Store,
        fence: &Fence,
        phase: &Phase,
    ) -> Result<(), Error> {
        let now = Instant::now();
        if now < self.next_poll || matches!(phase, Phase::Opening) {
            return Ok(());
        }
        self.next_poll = now + TICK;
        // A failed read is retried at the next poll, as a cancel's is.
        let Ok(pending) = store.backend().pending_steers(fence) else {
            return Ok(());
        };
        for row in pending {
            let refusal = match phase {
                Phase::Stopping { .. } => Some("the turn is being stopped".to_owned()),
                _ if !self.offered => Some(format!(
                    "{} cannot take input during a running turn",
                    self.profile
                )),
                // Its message raced the turn's start and is in the prompt.
                _ if row.message.is_some() && row.message_delivered => {
                    Some("its message was delivered at the turn's start".to_owned())
                }
                _ => match session.steer(&row.text) {
                    Ok(()) => {
                        self.written.push(row.id);
                        recorder.record(Activity::Steered {
                            id: row.id,
                            by: row.by.clone(),
                            text: row.text.clone(),
                        })?;
                        let message =
                            store
                                .backend()
                                .settle_steer(fence, row.id, &SteerState::Written)?;
                        self.delivered(recorder, row.id, message)?;
                        None
                    }
                    // The harness cannot take it yet; the next poll retries.
                    Err(RuntimeError::Rejected(Rejected::SteerNotYet)) => break,
                    Err(RuntimeError::Rejected(rejected)) => Some(rejected.to_string()),
                    Err(error) => Some(format!("could not write it to the harness: {error}")),
                },
            };
            if let Some(reason) = refusal {
                Self::refuse(recorder, store, fence, row.id, &row.by, reason)?;
            }
        }
        Ok(())
    }

    /// Settle written input the harness accepted or dropped.
    fn event(
        &mut self,
        recorder: &mut Recorder,
        store: &Store,
        fence: &Fence,
        event: &Event,
    ) -> Result<(), Error> {
        let (steer, state) = match event {
            Event::SteerAccepted { steer, .. } => (*steer, SteerState::Accepted),
            Event::SteerRejected { steer, reason, .. } => (
                *steer,
                SteerState::Refused {
                    reason: reason.clone(),
                },
            ),
            _ => return Ok(()),
        };
        let index = usize::try_from(steer).ok().and_then(|n| n.checked_sub(1));
        match index.and_then(|i| self.written.get(i)) {
            Some(id) => {
                let message = store.backend().settle_steer(fence, *id, &state)?;
                self.delivered(recorder, *id, message)
            }
            None => Ok(()),
        }
    }

    /// Refuse what the turn never took: it ended first.
    fn finish(
        &mut self,
        recorder: &mut Recorder,
        store: &Store,
        fence: &Fence,
    ) -> Result<(), Error> {
        let Ok(pending) = store.backend().pending_steers(fence) else {
            return Ok(());
        };
        for row in pending {
            Self::refuse(
                recorder,
                store,
                fence,
                row.id,
                &row.by,
                "the turn ended before it could be delivered".into(),
            )?;
        }
        Ok(())
    }
}

fn phase_turn(phase: &Phase) -> Option<u64> {
    match phase {
        Phase::Opening => None,
        Phase::Running(n) | Phase::Stopping { turn: n, .. } => Some(*n),
    }
}

/// Answer one permission request through the policy and record the
/// decision. While stopping, deny without asking. Returns a reason to stop
/// the turn when the answer could not be delivered.
fn answer(
    recorder: &mut Recorder,
    session: &mut Session,
    turn: &Turn<'_>,
    policy: &Policy,
    request: &PermissionRequest,
    stopping: bool,
    (deadline_ms, cancelled): (Option<u64>, &dyn Fn() -> bool),
) -> Result<Option<String>, Error> {
    let (decision, source) = if stopping {
        let decision = PermissionDecision::Deny {
            message: "The turn is being interrupted.".into(),
        };
        (decision, DecisionSource::Engine)
    } else {
        let decided = policy.decide_with_source(&turn.record.info.name, request);
        // Approvals only tighten what the policy allowed.
        match decided {
            (PermissionDecision::Allow, source) => crate::effects::tool::decide(
                turn.yard,
                &turn.record,
                &request.tool,
                &request.input,
                policy.preset(),
                deadline_ms,
                cancelled,
            )
            .unwrap_or((PermissionDecision::Allow, source)),
            denied => denied,
        }
    };
    let (allowed, message) = match &decision {
        PermissionDecision::Allow => (true, None),
        PermissionDecision::Deny { message } => (false, Some(message.clone())),
    };
    match session.respond(&request.key, decision) {
        Ok(()) => {
            recorder.record(Activity::Decision {
                tool: request.tool.clone(),
                allowed,
                message,
                source,
            })?;
            Ok(None)
        }
        Err(error) => {
            let reason = format!(
                "could not deliver the answer to {}'s permission request: {error}",
                request.tool
            );
            recorder.record(Activity::Decision {
                tool: request.tool.clone(),
                allowed: false,
                message: Some(format!("{reason}; interrupting the turn")),
                source: DecisionSource::Engine,
            })?;
            Ok(Some(reason))
        }
    }
}

/// Start the turn's effect-ledger proxy and give it to the harness as the
/// gateway. `None` when the yard turned it off, or for a sandboxed turn
/// with no address a sandbox reaches it at (said in a warning: its calls
/// reach the ledger only from the gateway's audit log).
fn effects_proxy(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &Record,
    prepared: &mut crate::connectors::Prepared,
    deadline_ms: Option<u64>,
    preset: Option<crate::PolicyPreset>,
) -> Result<Option<crate::effects::proxy::EffectProxy>, String> {
    let Some(gateway) = turn.yard.connectors() else {
        return Ok(None);
    };
    let settings = &gateway.effects;
    if !settings.enabled {
        return Ok(None);
    }
    let sandboxed = crate::placement::sandboxed(record.provider.as_ref());
    let host = match (sandboxed, &settings.sandbox_host) {
        (false, _) => match settings.listen {
            std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
            std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_owned(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        },
        (true, Some(host)) => host.clone(),
        (true, None) => {
            recorder
                .record(Activity::Warning(
                    "this sandboxed turn calls the connector gateway directly: its effects reach                      the ledger only from the gateway's audit log, after the fact, and are not                      approved first; set [connectors] effects_sandbox_host (docs/effects.md)"
                        .into(),
                ))
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
    };
    let proxy = crate::effects::proxy::EffectProxy::start(
        turn.yard,
        record,
        settings.listen,
        crate::effects::proxy::ProxySpec {
            upstream: gateway.url.clone(),
            token: prepared.token.clone(),
            deadline_ms,
            preset,
        },
    )
    .map_err(|e| e.to_string())?;
    let url = format!("http://{host}:{}/mcp", proxy.port());
    for var in prepared.env.iter_mut() {
        if var.name == crate::connectors::ENV_GATEWAY_URL {
            var.value = url.clone();
        }
    }
    prepared.gateway_url = url.clone();
    recorder
        .record(Activity::Effect(Box::new(
            crate::effects::EffectActivity::Proxy { url },
        )))
        .map_err(|e| e.to_string())?;
    Ok(Some(proxy))
}

/// What the `snapshot` step recorded.
#[derive(Serialize, Deserialize)]
struct Snapshotted {
    candidate: Option<CandidateInfo>,
    error: Option<String>,
}

/// Snapshot the candidate and settle the branch's record after the turn.
/// The snapshot is a journaled step: when an earlier attempt of this turn
/// recorded it, its outcome is used and nothing is committed again.
pub(crate) fn conclude(
    yard: &Yard,
    prompt: &str,
    fence: &Fence,
    record: &mut Record,
    recorder: &mut Recorder,
    driven: Driven,
) -> Result<(), Error> {
    let store = yard.store();
    let excluded = crate::workspace::excluded(record);
    let merged = record.merged.take();
    let info = &mut record.info;
    if driven.submitted {
        info.turns += 1;
    }
    if let Some(session) = &driven.session {
        info.session = Some(session.to_string());
        record.cost_session = Some(session.to_string());
    }
    match (driven.metered, driven.cost) {
        (Some(metered), _) => info.cost_usd = Some(info.cost_usd.unwrap_or(0.0) + metered),
        (None, Some(cost)) => info.cost_usd = Some(spent(cost, record.cost_baseline)),
        (None, None) => {
            if let Some(live) = driven.live_cost {
                info.cost_usd = Some(live);
            }
        }
    }
    let message = format!("{}: turn {}\n\n{}\n", info.git_branch, info.turns, prompt);
    let previous = info.candidate.as_ref().map(|c| c.commit.clone());
    let intent = json!({ "message": message });
    let recorded = match store
        .backend()
        .begin_step(fence, fence.turn, STEP_SNAPSHOT, &intent)?
    {
        Begun::Done(outcome) => serde_json::from_value::<Snapshotted>(outcome).ok(),
        Begun::Fresh | Begun::Pending(_) => None,
    };
    let replayed = recorded.is_some();
    let snapshotted = match recorded {
        Some(snapshotted) => snapshotted,
        None => {
            // In a task's own repository, large files become pointers;
            // see `crate::tasks::large`.
            if let Err(error) = crate::tasks::before_snapshot(yard, info) {
                recorder.record(Activity::Warning(format!(
                    "large files were not stored as chunks: {error}"
                )))?;
            }
            let snapshot = {
                let _lock = git::lock();
                let branch = names::validate(&info.name)?;
                match yard.repo.workspace(&branch) {
                    Ok(Some(workspace)) => workspace
                        .excluding(excluded)
                        .snapshot(&message)
                        .map_err(|e| e.to_string()),
                    Ok(None) => Err(format!("no worktree has {} checked out", info.git_branch)),
                    Err(error) => Err(error.to_string()),
                }
            };
            match snapshot {
                Ok(candidate) => Snapshotted {
                    candidate: candidate.map(|c| CandidateInfo {
                        commit: c.head.0,
                        files_changed: c.stat.files_changed as u32,
                        insertions: c.stat.insertions as u32,
                        deletions: c.stat.deletions as u32,
                    }),
                    error: None,
                },
                Err(error) => Snapshotted {
                    candidate: None,
                    error: Some(format!("snapshot failed: {error}")),
                },
            }
        }
    };
    let mut changed = false;
    if snapshotted.error.is_none() {
        let candidate = snapshotted.candidate.clone();
        changed = candidate.as_ref().map(|c| &c.commit) != previous.as_ref();
        if let (true, false, Some(candidate)) = (changed, replayed, &candidate) {
            recorder.record(Activity::Snapshot(candidate.clone()))?;
        }
        // The branch's status says what it holds; that this turn changed
        // nothing is the turn's own to say, in its events.
        let completed = matches!(
            driven.end,
            End::Outcome {
                outcome: TurnOutcome::Completed
            }
        );
        if completed && !changed && !replayed && candidate.is_some() {
            recorder.record(Activity::Warning(
                "the turn changed no file; the branch keeps its candidate from an earlier turn"
                    .into(),
            ))?;
        }
        info.candidate = candidate;
    }
    // A merged branch whose turn changed nothing still holds the candidate
    // already integrated. Only a completed turn keeps it: a failed or lost
    // one says so instead, as the reason its parent and `--retry` act on.
    let merged = merged.filter(|_| snapshotted.error.is_none() && !changed);
    if !replayed {
        store
            .backend()
            .finish_step(fence, fence.turn, STEP_SNAPSHOT, &to_value(&snapshotted))?;
    }
    info.status = match driven.end {
        // `ready` while the branch has a candidate, whichever turn made it
        // (still `merged` when it is the one already integrated);
        // `no_changes` only when it has none.
        End::Outcome {
            outcome: TurnOutcome::Completed,
        } if info.candidate.is_some() => merged.unwrap_or(BranchStatus::Ready),
        End::Outcome {
            outcome: TurnOutcome::Completed,
        } => BranchStatus::NoChanges,
        End::Outcome {
            outcome: TurnOutcome::Interrupted,
        } => BranchStatus::Interrupted,
        End::Outcome {
            outcome: TurnOutcome::Failed { message },
        } => BranchStatus::Failed { reason: message },
        End::Outcome {
            outcome: TurnOutcome::LimitReached { limit },
        } => BranchStatus::BudgetExceeded {
            limit: format!("harness {limit}"),
        },
        End::Outcome {
            outcome: TurnOutcome::Refused,
        } => BranchStatus::Failed {
            reason: "the model refused to continue".into(),
        },
        End::Budget { limit, harness_usd } => {
            if let Some(usd) = harness_usd {
                recorder.record(Activity::Warning(format!(
                    "the harness stopped itself at the ${usd:.4} spending limit it was given, \
                     what was left of {} when the turn started",
                    crate::operations::limit_text(&limit)
                )))?;
            }
            BranchStatus::BudgetExceeded { limit }
        }
        End::Cancelled { by } => {
            recorder.record(Activity::Warning(format!("cancelled by {by}")))?;
            BranchStatus::Interrupted
        }
        End::Failed { reason } => BranchStatus::Failed { reason },
        End::Lost { reason } => {
            recorder.record(Activity::Warning(reason))?;
            BranchStatus::Interrupted
        }
        End::Stalled => {
            recorder.record(Activity::Warning(
                "interrupted after a stall: no harness activity for its stall window".into(),
            ))?;
            BranchStatus::Interrupted
        }
    };
    let snapshot_failed = snapshotted.error.is_some();
    if let Some(error) = snapshotted.error {
        info.status = match &info.status {
            BranchStatus::Failed { reason } => BranchStatus::Failed {
                reason: format!("{reason}; {error}"),
            },
            _ => BranchStatus::Failed { reason: error },
        };
    }
    if driven.submitted {
        // The summary a rewind left for this turn reached the harness, and
        // so did the note about a lost turn before it.
        record.context = None;
        record.lost = None;
        // A prompt was submitted: a lost one is no longer the next retry.
        record.retry = None;
        match snapshot_failed {
            false => crate::checkpoint::record_turn(yard, fence, record, recorder)?,
            // The worktree is no longer at a known checkpoint.
            true => record.checkpoint = None,
        }
    }
    // A planning turn that completed proposes its plan and waits.
    crate::plan::conclude(yard, record, recorder, changed)?;
    Ok(())
}

impl End {
    fn failed(reason: impl Into<String>) -> End {
        End::Failed {
            reason: reason.into(),
        }
    }

    fn budget(limit: impl Into<String>) -> End {
        End::Budget {
            limit: limit.into(),
            harness_usd: None,
        }
    }

    fn cancelled(by: impl Into<String>) -> End {
        End::Cancelled { by: by.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forks_cost_excludes_its_parents() {
        assert_eq!(spent(0.5, None), 0.5);
        assert_eq!(spent(0.75, Some(0.5)), 0.25);
        assert_eq!(spent(0.25, Some(0.5)), 0.0);
    }

    /// A hold is cut after the cap only when the turn has no duration
    /// limit, which bounds the hold itself, and only while it lasts.
    #[test]
    fn a_hold_without_a_duration_limit_is_cut_at_the_cap() {
        let cap = Duration::from_secs(60);
        let since = Instant::now();
        let later = since + cap;
        assert!(hold_cut(Some(since), later, None, cap));
        assert!(!hold_cut(
            Some(since),
            later - Duration::from_secs(1),
            None,
            cap
        ));
        assert!(!hold_cut(None, later, None, cap), "not held");
        assert!(
            !hold_cut(Some(since), later, Some(later + cap), cap),
            "the duration limit bounds it"
        );
    }
}
