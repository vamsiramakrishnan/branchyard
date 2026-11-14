//! One turn on one branch: start the harness in the branch's worktree,
//! submit the prompt, answer and record everything until the turn ends,
//! enforce the budget, snapshot the candidate and close the harness.
//!
//! A delegating branch's turn also gets Branchyard's MCP server and a token
//! that lives exactly as long as the turn; a sandboxed one gets the server
//! over HTTP from this host (`crate::endpoint`). Its cost limit counts what its
//! children reserved, and a cancel request from an ancestor interrupts it
//! like a budget limit does.
//!
//! A cost limit is a limit, not accounting: as each usage report arrives
//! (or each model call the gateway meters ends) the turn is compared with
//! its limit and stopped at once when the report took it to the limit or
//! past it, not at the turn's end; and a branch below a parent with a
//! cost limit is stopped, over budget, as soon as the tree under that
//! parent has spent the limit ([`delegation::caps_above`]).
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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use branchyard_harness::profiles::Profile;
use branchyard_harness::{Open, Rejected, SessionMode};
use branchyard_runtime::{RuntimeError, Session};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::delegation;
use crate::endpoint::Reach;
use crate::graph;
use crate::placement::{Placement, SandboxPlan};
use crate::projection::{ENV_BRANCH, ENV_ROOT};
use crate::record::Recorder;
use crate::startup::{Phase as StartPhase, StartupPath};
use crate::state::{Begun, Fence, Lease, ProcessRow, Record, Store};
use crate::store_codec::deadline_capped;
use crate::{
    git, names, Activity, Branch, BranchStatus, Budget, CandidateInfo, DecisionSource,
    DeliveredVia, Error, Event, NativeSession, PermissionDecision, PermissionRequest, Policy,
    RecordedEvent, StallAction, SteerState, TaskOptions, TurnOutcome, Yard,
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
/// How often a delegated branch's turn looks at what the trees above it
/// have spent, besides when its own spending changes: the other branches
/// of a tree spend on their own, and this is how soon one notices that
/// the tree has spent its parent's limit.
const TREE_POLL: Duration = Duration::from_secs(1);
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

/// The warning a branch records when the engine interrupts its turn after
/// a stall ([`StallAction::Interrupt`]); the fleet reads it back to fail
/// over (`fleet::stall_fault`).
pub(crate) const STALL_WARNING: &str =
    "interrupted after a stall: no harness activity for its stall window";

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
    /// If the engine cut the turn's hold, the descendants the turn's
    /// latest answer cannot have seen settle: those still running when
    /// the harness last answered (the hold's start, or a cycle's result
    /// during it), and those spawned since. Its branch is woken for those
    /// that settled ([`crate::wake::park`]).
    pub held_on: Vec<String>,
    /// The exact values of the turn's secrets, for the memory scrubber at
    /// the turn's end; taken out before the turn is concluded, never
    /// journaled.
    pub secret_values: Vec<String>,
    /// The branch's newest event `seq` when the turn's prompt was about to
    /// be recorded: where the memory derivation starts reading.
    pub turn_from: Option<u64>,
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

/// Whether a branch that has spent `own`, with `held` of its cost limit
/// held by its children, has used up `max`: past it, or, for a report that
/// added spend (`grew`), to it. A report that added nothing cannot use up
/// a limit whose whole remainder the branch handed to its children, so a
/// parent that reserved everything it had left is not stopped for it.
fn used_up(max: Option<f64>, own: f64, held: f64, grew: bool) -> bool {
    max.is_some_and(|max| {
        let committed = own + held;
        committed > max + delegation::EPSILON_USD
            || (grew && committed >= max - delegation::EPSILON_USD)
    })
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
///
/// A step the turn survived (a sandbox that would not tear down, a lease
/// that would not release) is logged where it happens and, so that it is
/// still there when the run is over, recorded as a warning in the branch's
/// own log: see [`crate::SURVIVED_FAILURE`].
pub(crate) fn execute(turn: Turn<'_>, lease: Lease) -> Result<Branch, Error> {
    let yard = turn.yard.clone();
    let branch = lease.fence().branch.clone();
    let (result, survived) = branchyard_support::collect_failures(|| execute_turn(turn, lease));
    // Another engine owns the branch now: its log is not this one's to write.
    if !matches!(result, Err(Error::Fenced(_))) {
        record_survived(&yard, &branch, &survived);
    }
    result
}

/// How many survived failures one turn writes into its branch's log; the
/// rest are in the process's log.
const SURVIVED_RECORDED: usize = 20;

/// Append a warning to `branch`'s log for each of `survived`, once each.
fn record_survived(yard: &Yard, branch: &str, survived: &[branchyard_support::Failure]) {
    let store = yard.store();
    let mut seen = std::collections::HashSet::new();
    for failure in survived
        .iter()
        .filter(|f| seen.insert((f.context.as_str(), f.error.as_str())))
        .take(SURVIVED_RECORDED)
    {
        let event = crate::RecordedEvent {
            at_ms: branchyard_support::time::now_ms(),
            activity: Activity::Warning(format!(
                "{}{}: {}",
                crate::SURVIVED_FAILURE,
                failure.context,
                failure.error
            )),
        };
        branchyard_support::best_effort(
            "record a survived failure in the branch's log",
            store.append(branch, &event, None),
        );
    }
}

fn execute_turn(turn: Turn<'_>, lease: Lease) -> Result<Branch, Error> {
    let store = turn.yard.store();
    let fence = lease.fence().clone();
    let mut record = turn.record.clone();
    let _capture_cleanup = crate::contract::capture::staging_cleanup(turn.yard, &fence);
    // A turn recovery did not conclude counted its metered calls in the
    // cost it left; this turn starts from that figure.
    record.metered_from = None;
    let mut recorder = Recorder::fenced(&store, &fence, turn.options.observer.clone());
    recorder.trace_live(&record.info.harness, turn.options.trace_parent.as_deref());
    let bounds = Bounds {
        budget: delegation::effective_budget(&record, &turn.options.budget),
        // A branch writing its plan runs read-only, whatever the caller
        // passed; a delegating parent's denials still come first.
        policy: delegation::effective_policy(
            &record,
            &crate::plan::policy_for(&record, &turn.options.policy),
        ),
    };
    // The turn's secret values, kept here for the memory scrubber once the
    // turn has ended; this process is the one that holds them.
    let mut secret_values: Vec<String> = Vec::new();
    // Whether this turn submitted its prompt: only a turn that was driven
    // holds its secret values, so only it is derived at its end.
    let mut driven_turn = false;
    let mut turn_from: Option<u64> = None;
    let result = (|| {
        turn.yard
            .admit_provider(record.provider.as_ref(), turn.options)?;
        recorder.record(Activity::Status(record.info.status.clone()))?;
        if let Some(note) = &turn.note {
            recorder.record(Activity::Warning(note.clone()))?;
        }
        if matches!(record.info.status, BranchStatus::Failed { .. }) {
            return Ok(());
        }
        if let Some(by) = store.backend().cancel_requested(&fence)? {
            recorder.record(Activity::Warning(format!(
                "cancelled {} before the turn started",
                crate::operations::actor_text(&by)
            )))?;
            record.info.status = BranchStatus::Interrupted;
        } else if let Some(limit) = exhausted(&store, &record, &bounds.budget) {
            record.info.status = BranchStatus::BudgetExceeded { limit };
        } else if let Some(cap) = delegation::capped_above(&store, &record.info.name, None) {
            // The tree under a parent has spent the parent's limit: this
            // branch does not run, whoever sent it.
            recorder.record(Activity::Warning(cap.stop_note()))?;
            delegation::note_tree_stop(
                &store,
                &record.info.name,
                &cap,
                turn.options.observer.clone(),
            );
            record.info.status = BranchStatus::BudgetExceeded {
                limit: "max_usd".into(),
            };
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
            let mut driven = driven?;
            secret_values = std::mem::take(&mut driven.secret_values);
            driven_turn = driven.submitted;
            turn_from = driven.turn_from;
            let park_input = crate::wake::ParkInput::new(&bounds.budget, &driven.held_on);
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
            crate::wake::park(turn.yard, &mut record, &mut recorder, &park_input)?;
        }
        Ok(())
    })();
    let result = result.and_then(|()| {
        crate::contract::collect_artifacts(turn.yard, &record, &fence)?;
        crate::contract::capture_native(turn.yard, &record, &mut recorder)
    });
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
        // A child that owed its parent a result and gave none is noted in
        // the parent's log.
        crate::contract::settled(turn.yard, &fence.branch, turn.options.observer.clone());
        // The turn's memory observations, derived from its events and
        // scrubbed of its secret values (`docs/memory.md`, section 2).
        // A turn that never ran (cancelled or over budget before it
        // started, or set up and failed) holds no secret values: deriving
        // the branch's last turn again from its log without them would
        // store the text the first derivation scrubbed.
        if let Ok(branch) = &result {
            if driven_turn && branch.info.turns > 0 {
                crate::memory::observe::on_turn_end(
                    turn.yard,
                    &fence.branch,
                    fence.incarnation,
                    branch.info.turns,
                    &secret_values,
                    turn_from,
                );
            }
        }
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

/// Note the phases a prepared placement reached, and name its provider and
/// path for the turn's [`crate::startup::Startup`]. `placement_at` is the
/// offset at which the provider was asked. A local harness reaches none of
/// the sandbox's phases. A copied sandbox reports how long it took to be
/// ready, before its worktree and home went in; a mounted one has its
/// worktree attached at creation.
fn mark_sandbox(
    startup: &mut crate::startup::Timer,
    placement: &Placement,
    placement_at: u64,
) -> (String, StartupPath) {
    let Some(crate::SandboxEvent::Started {
        provider,
        origin,
        placed,
        ..
    }) = placement.started()
    else {
        return ("local".to_owned(), StartupPath::Local);
    };
    let now = startup.offset();
    let ready = match placed {
        Some(placed) => placement_at.saturating_add(placed.ready_ms).min(now),
        None => now,
    };
    let launch = placement
        .sandbox()
        .and_then(|(sandbox, name)| sandbox.launch(name));
    if let Some(ms) = launch.and_then(|launch| launch.image_ready_ms) {
        startup.mark_at(
            StartPhase::ImageReady,
            placement_at.saturating_add(ms).min(ready),
        );
    }
    startup.mark_at(StartPhase::SandboxReady, ready);
    startup.mark_at(StartPhase::SourceAttached, now);
    (provider, crate::startup::path_of(&origin, launch))
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
    // The deadline is durable, so recovery can say whether it had passed;
    // on the store's clock, which recovery compares it with. The turn
    // itself is held to `deadline`, on this process's monotonic clock.
    let deadline_ms = match bounds.budget.max_duration {
        Some(limit) => Some(deadline_capped(store.backend().clock_ms()?, limit)),
        None => None,
    };
    store.backend().set_deadline(fence, deadline_ms)?;
    let driven = run(recorder, turn, record, bounds, lease, started, deadline)?;
    journal(
        &store,
        fence,
        STEP_TURN_END,
        json!({ "submitted": driven.submitted,
            "wake": crate::wake::ParkInput::new(&bounds.budget, &driven.held_on) }),
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
    // The turn's startup phases, counted from its operation's admission
    // (`crate::startup`); recorded once, with the harness's first event.
    let mut startup = crate::startup::Timer::system(turn.options.admitted_ms);
    startup.mark(StartPhase::Queued);
    let mut startup_recorded = false;
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
        secret_values: Vec::new(),
        turn_from: None,
    };
    // A skill log an earlier turn left (it returned before the ingest
    // below) is never this turn's: removed before anything of this turn
    // can write one.
    let stale_home = match &record.home {
        Some(home) => Ok(home.clone()),
        None => crate::capabilities::local_home(store.dir(), &record.info.name),
    };
    if let Some(home) =
        branchyard_support::best_effort("find the home to clear a stale skill log", stale_home)
    {
        branchyard_support::best_effort(
            "clear a stale skill log",
            crate::skill_activity::clear(&home),
        );
    }
    // Revoked when this function returns, after the harness and its
    // sandbox are gone. A local harness runs the delegation tools itself;
    // a sandboxed one is served them over HTTP from this host, below, when
    // its sandbox can reach this host (`crate::endpoint`).
    let reach = crate::endpoint::reach(turn.yard, record);
    let projection = match &reach {
        Reach::Never(why) => {
            if record.grant.is_some() {
                recorder.record(Activity::Warning(format!(
                    "delegation is unavailable in this sandbox: {why}, so this turn runs \
                     without delegation tools"
                )))?;
            }
            Ok(None)
        }
        Reach::Unset(why) => Err(Error::Unsupported(why.clone())),
        Reach::Local | Reach::Host(_) => crate::projection::project(
            turn.yard,
            record,
            turn.options,
            bounds.budget.clone(),
            bounds.policy.clone(),
        ),
    };
    let mut projection = match projection {
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
    let (mut scoped, narrowed) = crate::access::scoped(turn.yard, record);
    // The skills the policy allows as of this call narrow what the branch
    // was admitted with (`TaskOptions::allowed_skills`), for the manifest and
    // for what the turn mounts.
    if turn.options.allowed_skills.is_some() {
        scoped
            .provision
            .get_or_insert_with(Default::default)
            .narrow_allowed_skills(turn.options.allowed_skills.as_deref());
    }
    if let Some(narrowed) = narrowed {
        recorder.record(Activity::Access(Box::new(narrowed)))?;
    }
    let scopes = crate::access::TokenScopes::of(&scoped);
    // The session's turn manifest: resolved when it started, and again
    // only when its tools changed (`crate::manifest`). Kept with the
    // branch, written into its home, and named in its token.
    // A turn that wakes the branch's kept sandbox resolves it again: what
    // the person deselected or the policy withdrew while it slept is gone,
    // and the bundles are the ones this host serves now.
    let waking = crate::placement::wakes_kept_sandbox(turn.yard, record, fence);
    // The repository skills fixed for the branch, read again from their
    // commit: never from the worktree its turns change. The person's own
    // skills, read again from their folder each turn: they are pinned by
    // nothing, so the manifest and the record keep each digest.
    let skills = crate::skills::for_turn(turn.yard.root(), record.skills.as_ref());
    let user_skills = record
        .user_skills
        .as_deref()
        .map(|dir| crate::skills::user_for_turn(dir, &skills));
    let sources = crate::manifest::Sources {
        repository: &skills,
        user: user_skills.as_ref(),
    };
    let manifest = crate::manifest::settle(
        turn.yard,
        &scoped,
        &scopes,
        turn.options.policy_version,
        waking,
        sources,
    );
    if record.manifest.as_ref() != Some(&manifest) {
        record.manifest = Some(manifest.clone());
        store.write_fenced(record, fence)?;
    }
    let manifest_digest = manifest.digest();
    let mut manifest_written = false;
    if let Some(home) = record.home.as_deref() {
        match manifest.write(home) {
            Ok(written) => manifest_written = written,
            Err(error) => {
                driven.end = End::failed(format!("could not write the turn manifest: {error}"));
                return Ok(driven);
            }
        }
    }
    // The skills this host holds are the ones the session was admitted
    // with (`crate::bundles`); a connector's bundle is held to the manifest
    // below, where it is linked or copied. Before anything starts.
    let tool_store = crate::toolstore::for_turn(turn.yard, record);
    if let Err(mismatch) = crate::bundles::verify_skills(&manifest, tool_store.as_deref())
        .and_then(|()| crate::bundles::verify_custom(&manifest, sources))
    {
        recorder.record(Activity::Warning(mismatch.to_string()))?;
        driven.end = End::failed(mismatch.to_string());
        return Ok(driven);
    }
    // A sandboxed harness's delegation tools, served from this host for
    // this turn with a token of its own; stopped, with each session's
    // server, when this function returns, after the harness is gone.
    let serving = match (&reach, projection.as_mut()) {
        (Reach::Host(host), Some(offered)) => Some(crate::endpoint::Endpoint::start(
            turn.yard,
            record,
            host,
            offered,
            &scopes,
            deadline_ms,
        )),
        _ => None,
    };
    let _endpoint = match serving {
        None => None,
        Some(Ok(endpoint)) => Some(endpoint),
        Some(Err(error)) if turn.options.delegation.is_some() => {
            driven.end = End::failed(format!("could not offer delegation: {error}"));
            return Ok(driven);
        }
        Some(Err(error)) => {
            recorder.record(Activity::Warning(format!(
                "this turn runs without delegation tools: {error}"
            )))?;
            projection = None;
            None
        }
    };
    let mut connectors =
        match crate::connectors::prepare(turn.yard, &scoped, deadline_ms, &scopes, Some(&manifest))
        {
            Ok(connectors) => connectors,
            Err(reason) if reason.starts_with(crate::bundles::BUNDLE_MISMATCH) => {
                recorder.record(Activity::Warning(reason.clone()))?;
                driven.end = End::failed(reason);
                return Ok(driven);
            }
            Err(reason) => {
                driven.end = End::failed(format!("could not provide connectors: {reason}"));
                return Ok(driven);
            }
        };
    for warning in connectors.iter().flat_map(|prepared| &prepared.warnings) {
        recorder.record(Activity::Warning(warning.clone()))?;
    }
    // A sandbox whose proxy holds the credentials proves the turn with its
    // secret: the server finds the secret's hash in the branch's record, so
    // it is written before the proxy starts, and cleared when the turn ends
    // (`conclude`).
    if let Some((_, credential)) = connectors
        .as_ref()
        .and_then(|c| c.sandbox.as_ref())
        .map(|s| &s.secret)
    {
        record.credential = Some(credential.clone());
        store.write_fenced(record, fence)?;
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
        crate::models::TurnInputs {
            token: connectors.as_ref().and_then(|c| c.shared_token()),
            deadline_ms,
            scopes: &scopes,
            budget: bounds.budget.clone(),
            reserved: delegation::reserved(&store, record),
            trace_parent: turn.options.trace_parent.as_deref(),
        },
    ) {
        Ok(models) => models,
        Err(reason) => {
            driven.end = End::failed(format!("could not provide the model gateway: {reason}"));
            return Ok(driven);
        }
    };
    let mut model_gateway = None;
    let mut turn_hosts = crate::egress::TurnHosts {
        mirrors: turn.yard.mirrors(),
        ..crate::egress::TurnHosts::default()
    };
    let mut model_env = Vec::new();
    let mut model_scrub = Vec::new();
    // The gateway as a provisioner sees it, for a harness whose provider
    // endpoint is a file in its home.
    let mut provisioned_gateway = None;
    match models {
        None => {}
        Some(crate::models::Prepared::Direct { hosts, activity }) => {
            recorder.record(Activity::Model(Box::new(activity)))?;
            turn_hosts.model_endpoints.extend(hosts);
        }
        Some(crate::models::Prepared::Gateway {
            gateway,
            env,
            scrub,
            url,
            activity,
        }) => {
            if let crate::models::ModelActivity::Gateway { apis, .. } = &activity {
                provisioned_gateway = Some(branchyard_provision::models::ModelGateway {
                    url: url.trim_end_matches('/').to_owned(),
                    anthropic: apis.contains(&crate::models::Api::Anthropic),
                    openai: apis.contains(&crate::models::Api::Openai),
                });
            }
            recorder.record(Activity::Model(Box::new(activity)))?;
            turn_hosts.model_gateway = crate::egress::gateway_rule(&url);
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
    // What its parent assigned it: the paths it owns, the shared brief.
    let assignment = crate::assignment::for_turn(turn.yard, record);
    // The yard's control planes' tokens, read on every turn, on the gateway
    // or not: no secret hands one to a harness, and the variables of this
    // process are looked through with them now (a key file may have
    // changed since the gateway was built). A variable children need that
    // holds one refuses the turn, as it refuses the configuration.
    let guarded = match turn.yard.models() {
        Some(gateway) => {
            if let Err(reason) = gateway.hide_copies() {
                driven.end = End::failed(reason);
                return Ok(driven);
            }
            gateway.control_plane_tokens()
        }
        None => Vec::new(),
    };
    let provisioned = match crate::provisioning::prepare(
        record,
        turn.profile,
        projection.as_ref(),
        connectors.as_ref(),
        crate::provisioning::Standing {
            knowledge: Some(&briefing),
            assignment: assignment.as_deref(),
            skills: &skills,
            user_skills: user_skills.as_ref(),
            manifest: record.home.as_ref().map(|_| manifest_digest.as_str()),
            manifest_written,
            allowed_skills: turn.options.allowed_skills.as_deref(),
            store: tool_store.as_deref(),
            mount_ready: crate::providers::of(record.provider.as_ref())
                .mounts_ready_skills(turn.yard),
        },
        crate::provisioning::Access {
            policy: &bounds.policy,
            model_gateway: provisioned_gateway,
            guarded: &guarded,
        },
        store.dir(),
    ) {
        Ok(provisioned) => provisioned,
        Err(reason) => {
            driven.end = End::failed(format!("could not provision {}: {reason}", turn.profile.id));
            return Ok(driven);
        }
    };
    driven.secret_values.clone_from(&provisioned.secret_values);
    // Declared before the session so a sandbox outlives it. The turn's
    // provisioning is recorded once the sandbox exists, so its activity
    // names the placement the turn ran in.
    startup.mark(StartPhase::Placement);
    let placement_at = startup.offset();
    let mut placement = match Placement::prepare(turn.yard, record, fence, &turn.sandbox) {
        Ok(placement) => placement,
        Err(reason) => {
            recorder.record(provisioned.activity)?;
            for warning in provisioned.warnings {
                recorder.record(Activity::Warning(warning))?;
            }
            driven.end = End::failed(reason);
            return Ok(driven);
        }
    };
    let (startup_provider, startup_path) = mark_sandbox(&mut startup, &placement, placement_at);
    // Branchyard writes no administrator file: a harness's own managed
    // policy is the administrator's, and what Branchyard requires reaches
    // the harness through its session (flags, a hook, a private home's
    // files) instead (`docs/harness-integration.md`).
    recorder.record(provisioned.activity)?;
    for warning in provisioned.warnings {
        recorder.record(Activity::Warning(warning))?;
    }
    // A task that let the harness's own permissions decide what this
    // profile cannot bind says so on the branch, every turn.
    // The branch's own opt-out and home, as admission and provisioning
    // read them.
    let composing = TaskOptions {
        unapproved_tools: turn.options.unapproved_tools || record.unapproved_tools,
        ..turn.options.clone()
    };
    if let Some(note) = crate::harness::unbound_note(
        turn.profile,
        &composing,
        record.home.is_some(),
        &turn.yard.approval_settings(),
    ) {
        recorder.record(Activity::Warning(note))?;
    }
    // The image the provider's receipt names is the one the session was
    // admitted with (`crate::bundles::verify_image`), before the sandbox is
    // used for anything.
    if let Some((provider, name)) = placement.sandbox() {
        // A receipt that cannot be read is no evidence of another image.
        let receipt = branchyard_support::best_effort(
            "read the sandbox's image receipt",
            provider.image_receipt(name),
        )
        .flatten();
        if let Err(mismatch) = crate::bundles::verify_image(&manifest, receipt.as_deref()) {
            recorder.record(Activity::Warning(mismatch.to_string()))?;
            if let Some(warning) = placement.discard() {
                recorder.record(Activity::Warning(warning))?;
            }
            driven.end = End::failed(mismatch.to_string());
            return Ok(driven);
        }
    }
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
    // The turn's file guard: its preset's write scope, held below the
    // harness by Landlock where this host can, and what the branch
    // records of it either way. See `crate::guard`.
    let home = placement.local_home();
    let (writable, guard) =
        match crate::guard::plan(turn.yard, record, bounds.policy.preset(), home.as_deref()) {
            Ok(planned) => planned,
            Err(reason) => {
                driven.end = End::failed(format!(
                    "could not work out where its harness may write: {reason}; the harness was \
                     not started"
                ));
                return Ok(driven);
            }
        };
    if let Some(writable) = writable {
        placement.guard(writable);
    }
    if record.info.guard.as_ref() != Some(&guard) {
        record.info.guard = Some(guard);
        store.write_fenced(record, fence)?;
    }
    let record: &Record = record;
    // Every local harness learns which branch it is on, so `by` inside it
    // never mistakes it for a person, and finds `by` itself; only a
    // delegated one gets a token.
    if !placement.is_sandbox() {
        // The workspace's own variables first, so the branch's win.
        if let Some(spec) = record.workspace.as_ref().map(|w| &w.spec) {
            let mut vars = crate::workspace::variables(
                turn.yard,
                &record.info.name,
                &record.info.worktree,
                store.ports().port(&record.info.name)?,
            );
            vars.push((ENV_ROOT.to_owned(), turn.yard.root.display().to_string()));
            let expanded = spec.expanded_env(&vars);
            for (name, value) in &expanded {
                placement.set_env(name, value);
            }
            // What `shell_setup` sets, run now, on every turn: over the
            // workspace's variables, under the branch's (set below), and
            // never what the harness's environment scrubs (an isolated
            // harness's credentials, a secret's or the gateway's).
            if let Some(snippet) = &spec.shell_setup {
                if turn.yard.hub.scripts_denied() {
                    driven.end = End::failed(format!(
                        "workspace shell_setup was not run: {}",
                        crate::workspace::DENIED
                    ));
                    return Ok(driven);
                }
                let mut start = expanded.clone();
                start.extend(vars.iter().cloned());
                let lost = || {
                    lease
                        .lost()
                        .then(|| "the turn's lease was taken over".to_owned())
                };
                match crate::workspace::shell_env(snippet, &record.info.worktree, &start, &lost) {
                    Ok(changed) => {
                        for (name, value) in changed {
                            let scrubbed = provisioned.scrub.contains(&name)
                                || model_scrub.contains(&name)
                                || !placement.admits(&name);
                            if !scrubbed {
                                placement.set_env(&name, &value);
                            }
                        }
                    }
                    Err(why) => {
                        driven.end =
                            End::failed(format!("workspace {why}; the harness was not started"));
                        return Ok(driven);
                    }
                }
            }
        }
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
    // An operator's manager credential never belongs to a native agent turn,
    // including provider pass-through and provisioned environment entries.
    placement.remove_env(branchyard_support::MANAGER_TOKEN_ENV);
    // The branch's network policy: its proxy's variables last, so nothing
    // above replaces them, and its namespace when the harness starts. The
    // proxy lives with the placement, after the harness is gone.
    if let Some(gateway) = connectors
        .as_ref()
        .and_then(|c| crate::egress::gateway_rule(&c.gateway_url))
    {
        turn_hosts.connector_gateway = Some(gateway);
    }
    // The server a sandbox's credential proxy fetches from, and the proxy
    // itself on loopback.
    for url in connectors.iter().flat_map(|c| &c.reach) {
        turn_hosts
            .connector_reach
            .extend(crate::egress::gateway_rule(url));
    }
    match crate::egress::prepare(turn.yard, &scoped, turn_hosts) {
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
    // A sandbox that runs its own credential proxy has it started now, in
    // the sandbox and before the harness, and stopped (killed) when this
    // function returns. A sidecar is not started here: the sandbox's own
    // definition runs it, reading the settings file in the home.
    let _credential_proxy = match start_credential_proxy(turn, &placement, connectors.as_ref()) {
        Ok(proxy) => proxy,
        Err(reason) => {
            driven.end = End::failed(reason);
            return Ok(driven);
        }
    };
    // A per-turn MCP file lives until this function returns, after the
    // harness is gone.
    let _turn_file = provisioned.turn_file;
    // The provisioner's arguments keep the harness asking Branchyard
    // (Gemini CLI's and Qwen Code's approval mode), after an override too.
    let mut command = turn.command.clone();
    command.extend(provisioned.session.args.iter().cloned());
    let driver = turn.profile.driver_with(command);
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
        permission_profile: provisioned.session.permission_profile,
        // Where the turn's final answer is what the contract reads (see
        // `contract::capture_native`), a harness that can constrain it
        // natively is given the schema.
        json_schema: record
            .result_contract
            .as_ref()
            .filter(|c| c.is_deliverable() || record.info.depth == 0)
            .map(|c| c.schema.clone()),
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
    // What was not given to the harness because its value holds a control
    // plane's token, whoever would have set it: said on the branch, by the
    // variable's name and the backend's.
    for (name, backend) in placement.take_withheld() {
        recorder.record(Activity::Warning(format!(
            "{name} was not given to the harness: its value holds the token of the \
             control-plane backend {backend}, which no harness is given"
        )))?;
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
    startup.mark(StartPhase::HarnessStarted);
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
    // What the turn's latest answer saw of its descendants: those still
    // running, and every one it knew of.
    let mut held_on: Vec<String> = Vec::new();
    let mut held_known: Vec<String> = Vec::new();
    let mut held_answers = 0;
    let mut cut_hold = false;
    let follow_up_grace = bounds.budget.follow_up_grace.unwrap_or(FOLLOW_UP_GRACE);
    // What the branch has spent, while the turn runs: see `LiveCost`.
    // Written to its record as it changes, so a parent inspecting it, or
    // the branch inspecting itself, sees a figure and not "unknown".
    let mut live = LiveCost::new(record.info.cost_usd.unwrap_or(0.0));
    let shown: Cell<Option<f64>> = Cell::new(None);
    // The record's cost also counts the turn's spend so far, as it is
    // reported (or, on the model gateway, as metered calls end);
    // `metered_from` keeps what the branch had spent before the turn,
    // what a gateway turn's total is added to and what the turn's own
    // cost is counted from when it ends, here or in recovery.
    let write = |stalled: Option<bool>| -> Result<(), Error> {
        let mut updated = record.clone();
        if let Some(value) = stalled {
            updated.info.stalled = value;
        }
        if let Some(cost) = shown.get() {
            updated.info.cost_usd = Some(cost);
            updated.metered_from = Some(record.info.cost_usd.unwrap_or(0.0));
        }
        store.write_fenced(&updated, fence)
    };
    // The trees above this branch are looked at when its own spending
    // changes, and every `TREE_POLL` besides.
    let delegated = record.info.parent.is_some();
    let mut tree_check = false;
    let mut tree_polled = Instant::now();
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
        // The answer is brought up to date when the hold begins and at each
        // cycle's result during it.
        if session.held() && (held_since.is_none() || session.held_answers() != held_answers) {
            held_answers = session.held_answers();
            let below = delegation::descendants(&store, &record.info.name).unwrap_or_default();
            held_known = below.iter().map(|info| info.name.clone()).collect();
            held_on = below
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
                // Written at once, so a parent, an ancestor's limit and
                // `by show` see the spend as it happens.
                shown.set(Some(own));
                write(None)?;
                tree_check = true;
                if let Some(projection) = &projection {
                    projection.observe_cost(own);
                }
                let over = used_up(
                    bounds.budget.max_usd,
                    own,
                    delegation::reserved(&store, record),
                    true,
                );
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
        // The tree this branch is in: stop as soon as the tree under a
        // parent with a cost limit has spent it, whichever branch spent it.
        if let (true, Phase::Running(n)) = (delegated, &phase) {
            if tree_check || now.duration_since(tree_polled) >= TREE_POLL {
                tree_check = false;
                tree_polled = now;
                if let Some(cap) = delegation::capped_above(&store, &record.info.name, shown.get())
                {
                    let n = *n;
                    recorder.record(Activity::Warning(cap.stop_note()))?;
                    delegation::note_tree_stop(
                        &store,
                        &record.info.name,
                        &cap,
                        turn.options.observer.clone(),
                    );
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
            // A staged skill's script run, from the call's target.
            if let Event::ToolStarted {
                target: Some(target),
                ..
            } = &event
            {
                if let Some(invoked) = crate::skill_activity::invoked(target) {
                    recorder.record(invoked.into())?;
                }
            }
            steering.event(recorder, &store, fence, &event)?;
        }
        last_activity = Instant::now();
        if !startup_recorded && matches!(phase, Phase::Running(_)) && recorded(&event) {
            startup.mark(StartPhase::FirstEvent);
            startup_recorded = true;
            recorder.record(Activity::Startup(Box::new(
                startup.finish(&startup_provider, startup_path),
            )))?;
        }
        if stalled && matches!(phase, Phase::Running(_)) {
            stalled = false;
            recorder.record(Activity::Resumed)?;
            write_stalled(false)?;
        }
        match event {
            Event::Ready if matches!(phase, Phase::Opening) => {
                startup.mark(StartPhase::HandshakeDone);
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
                for note in submission.expiry_notes(fence.turn) {
                    recorder.record(Activity::Warning(note))?;
                }
                driven.turn_from = Some(store.backend().event_count(&record.info.name)?);
                recorder.record(Activity::Prompt(submission.prompt.clone()))?;
                // Observers can apply a durable cancellation intent at this last
                // boundary. Do not send a model prompt after that cancellation.
                if let Some(by) = store.backend().cancel_requested(fence)? {
                    crate::inbox::abandon_submit(&store, fence, &submission)?;
                    kill = true;
                    break End::cancelled(by);
                }
                let prompt = format!("{}\n\n{}", provisioned.capabilities, submission.prompt);
                match session.submit(&prompt) {
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
                    (&bounds.policy, Path::new(&placement.cwd())),
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
                let before = live.total();
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
                    tree_check = true;
                }
                if let Some(projection) = &projection {
                    projection.observe_cost(own);
                }
                // Checked as the report arrives, not when the turn ends: a
                // report that took the branch to its limit, or past it,
                // stops the turn now.
                let over = used_up(
                    bounds.budget.max_usd,
                    own,
                    delegation::reserved(&store, record),
                    own > before + delegation::EPSILON_USD,
                );
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
        // Spawned since the latest answer, by a cycle cut short: unseen too.
        let spawned = delegation::descendants(&store, &record.info.name)
            .unwrap_or_default()
            .into_iter()
            .map(|info| info.name)
            .filter(|name| !held_known.contains(name));
        held_on.extend(spawned);
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
    // A turn that ended before the harness said anything records the
    // phases it reached.
    if !startup_recorded {
        recorder.record(Activity::Startup(Box::new(
            startup.finish(&startup_provider, startup_path),
        )))?;
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
        // No token is renewed in, or left beside, a sandbox that is paused.
        if let Some(prepared) = connectors.as_mut() {
            prepared.retire();
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
    // The renewer stops and the token file goes before the sandbox is
    // paused (a copied home returns without it), so a paused sandbox holds
    // no token; the next turn mints another (`docs/tools-model.md`, step 7).
    if let Some(prepared) = connectors.as_mut() {
        prepared.retire();
    }
    for activity in placement.release(turn.yard, record, fence) {
        recorder.record(activity)?;
    }
    if matches!(
        driven.end,
        End::Outcome {
            outcome: TurnOutcome::Completed
        }
    ) {
        if let Some(reason) = placement.transfer_error() {
            driven.end = End::failed(format!("could not capture the sandbox worktree: {reason}"));
        }
    }
    // Calls still in flight are given a moment; what the turn's calls
    // cost is the branch's.
    if let Some(gateway) = model_gateway {
        driven.metered = Some(gateway.finish());
    }
    // What the turn did with its skills, from the log the `tools` script
    // kept in the home (`crate::skill_activity`): read once the harness is
    // gone, recorded, and removed. A home this host cannot read (a copied
    // one) has no log here, and records nothing.
    let skills_home = match &record.home {
        Some(home) => Ok(home.clone()),
        None => crate::capabilities::local_home(store.dir(), &record.info.name),
    };
    let ingested = skills_home.and_then(|home| {
        crate::skill_activity::ingest(&home, &manifest).map_err(|e| e.to_string())
    });
    match ingested {
        Ok(ingested) => {
            for activity in ingested.activities {
                recorder.record(activity.into())?;
            }
            for warning in ingested.warnings {
                recorder.record(Activity::Warning(warning))?;
            }
        }
        Err(error) => recorder.record(Activity::Warning(format!(
            "the turn's skill log could not be read: {error}"
        )))?,
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
    (policy, root): (&Policy, &Path),
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
        let decided = policy.decide_in(&turn.record.info.name, request, root);
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

/// Start the credential proxy in the turn's sandbox when its provider runs
/// it as a guest process; `None` when the host holds the credentials or a
/// sidecar does.
fn start_credential_proxy(
    turn: &Turn<'_>,
    placement: &Placement,
    connectors: Option<&crate::connectors::Prepared>,
) -> Result<Option<crate::connectors::guest::GuestProxy>, String> {
    let Some(sandbox) = connectors.and_then(|c| c.sandbox.as_ref()) else {
        return Ok(None);
    };
    if sandbox.site != crate::connectors::CredentialSite::Guest {
        return Ok(None);
    }
    let (Some((provider, name)), Some(gateway)) = (placement.sandbox(), turn.yard.connectors())
    else {
        return Err("the turn has no sandbox to run its credential proxy in".into());
    };
    crate::connectors::guest::start(
        provider,
        name,
        &placement.cwd(),
        &gateway.credential_proxy.guest_command,
        &sandbox.config,
        Some(sandbox.secret.0.as_str()),
    )
    .map(Some)
    .map_err(|reason| format!("could not start the credential proxy: {reason}"))
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
    if prepared.sandbox.is_some() {
        // The token is in the sandbox's proxy, which the host's ledger
        // proxy cannot sit in front of without the harness holding a token
        // again.
        recorder
            .record(Activity::Warning(
                "this turn's gateway credentials are held by a proxy in its sandbox: its effects \
                 reach the ledger only from the gateway's audit log, after the fact, and are not \
                 approved first (docs/connectors.md#the-credential-proxy-in-the-sandbox)"
                    .into(),
            ))
            .map_err(|e| e.to_string())?;
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
    // The harness's token file now holds a short token, renewed until the
    // turn ends; the proxy calls the gateway with the live one.
    let token = prepared.renewing()?;
    let proxy = crate::effects::proxy::EffectProxy::start(
        turn.yard,
        record,
        settings.listen,
        crate::effects::proxy::ProxySpec {
            upstream: gateway.url.clone(),
            token,
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

/// Whether the turn the events end in (the events after its prompt)
/// already recorded its own cost.
fn turn_cost_recorded(events: &[RecordedEvent]) -> bool {
    events
        .iter()
        .rev()
        .take_while(|event| !matches!(event.activity, Activity::Prompt(_)))
        .any(|event| matches!(event.activity, Activity::TurnCost { .. }))
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
    record.credential = None;
    let info = &mut record.info;
    if driven.submitted {
        info.turns += 1;
    }
    if let Some(session) = &driven.session {
        info.session = Some(session.to_string());
        record.cost_session = Some(session.to_string());
    }
    // A turn that wrote its spend into the cost as it ran (on the model
    // gateway, its metered calls as they ended) left beside it what the
    // branch had spent before; otherwise the record still says that.
    let before_metered = record.metered_from.take();
    let before = before_metered.unwrap_or_else(|| info.cost_usd.unwrap_or(0.0));
    let measured = before_metered.is_some()
        || driven.metered.is_some()
        || driven.cost.is_some()
        || driven.live_cost.is_some();
    match (driven.metered, driven.cost) {
        (Some(metered), _) => info.cost_usd = Some(before + metered),
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
    let (recorded, pending) =
        match store
            .backend()
            .begin_step(fence, fence.turn, STEP_SNAPSHOT, &intent)?
        {
            Begun::Done(outcome) => (serde_json::from_value::<Snapshotted>(outcome).ok(), false),
            Begun::Fresh => (None, false),
            Begun::Pending(_) => (None, true),
        };
    let replayed = recorded.is_some();
    // The turn's own cost, now that its metering has settled: what the
    // branch's total grew by, so a harness's running session total counts
    // only what this turn added. Recorded once per turn: before the
    // snapshot step finishes, so a recovery that replays the step does not
    // repeat it, and not again by a recovery of an attempt that recorded it
    // and stopped before the step finished.
    let recorded_cost = pending
        && crate::record::read(&store, &info.name).is_ok_and(|events| turn_cost_recorded(&events));
    if !replayed && measured && !recorded_cost {
        let after = info.cost_usd.unwrap_or(0.0);
        recorder.record(Activity::TurnCost {
            cost_usd: (after - before).max(0.0),
        })?;
    }
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
            recorder.record(Activity::Warning(format!(
                "cancelled {}",
                crate::operations::actor_text(&by)
            )))?;
            BranchStatus::Interrupted
        }
        End::Failed { reason } => BranchStatus::Failed { reason },
        End::Lost { reason } => {
            recorder.record(Activity::Warning(reason))?;
            BranchStatus::Interrupted
        }
        End::Stalled => {
            recorder.record(Activity::Warning(STALL_WARNING.into()))?;
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

    /// A report stops a turn when it takes the branch to its limit or past
    /// it; one that spent nothing new does not stop a branch whose whole
    /// remaining budget its children hold, and no limit never stops one.
    #[test]
    fn a_report_that_reaches_the_limit_stops_the_turn() {
        assert!(!used_up(None, 5.0, 0.0, true), "no limit");
        assert!(!used_up(Some(1.0), 0.9, 0.0, true), "under it");
        assert!(used_up(Some(1.0), 1.0, 0.0, true), "to it");
        assert!(
            used_up(Some(1.0), 0.1 + 0.2 + 0.7, 0.0, true),
            "to it, rounded"
        );
        assert!(used_up(Some(1.0), 1.2, 0.0, true), "past it");
        assert!(
            used_up(Some(1.0), 0.4, 0.7, true),
            "children's share counts"
        );
        // Everything left handed to children: spending more is over, a
        // report that spent nothing is not.
        assert!(!used_up(Some(1.0), 0.4, 0.6, false));
        assert!(used_up(Some(1.0), 0.41, 0.6, true));
        // Past the limit stops it whether or not the report added spend: a
        // child that overshot took the parent over.
        assert!(used_up(Some(1.0), 0.4, 0.7, false));
    }

    /// A recovery that concludes a turn whose earlier attempt recorded the
    /// turn's cost and stopped before its snapshot step finished does not
    /// record it again; an earlier turn's cost does not count.
    #[test]
    fn a_turns_cost_is_recorded_once() {
        let at = |activity| RecordedEvent { at_ms: 0, activity };
        let cost = || at(Activity::TurnCost { cost_usd: 0.25 });
        let prompt = || at(Activity::Prompt("go".into()));
        assert!(turn_cost_recorded(&[prompt(), cost()]));
        assert!(!turn_cost_recorded(&[prompt(), cost(), prompt()]));
        assert!(!turn_cost_recorded(&[prompt()]));
        assert!(turn_cost_recorded(&[cost()]), "a log that lost its prompt");
    }

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
