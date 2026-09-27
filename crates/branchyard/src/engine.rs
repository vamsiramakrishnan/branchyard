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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::Profile;
use branchyard_harness::{Open, Rejected, SessionMode};
use branchyard_runtime::{RuntimeError, Session};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::delegation;
use crate::placement::Placement;
use crate::projection::{ENV_BRANCH, ENV_ROOT};
use crate::record::Recorder;
use crate::state::{now_ms, Begun, Fence, Lease, ProcessRow, Record, Store};
use crate::{
    git, names, Activity, Branch, BranchStatus, Budget, CandidateInfo, DecisionSource, Error,
    Event, NativeSession, PermissionDecision, PermissionRequest, Policy, SteerState, TaskOptions,
    TurnOutcome, Yard,
};

/// How long a harness may take to complete its handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a harness may take to exit once its stdin is closed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);
/// How long a harness may take to end a turn after an interrupt.
const INTERRUPT_GRACE: Duration = Duration::from_secs(30);
/// How often limits are checked while the harness is quiet.
const TICK: Duration = Duration::from_millis(100);
/// Events drained without waiting once the turn has ended.
const DRAIN_MAX: usize = 10_000;

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
}

/// How a turn ended, before the snapshot. Journaled as the `turn_end`
/// step's outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub(crate) enum End {
    Outcome {
        outcome: TurnOutcome,
    },
    /// Stopped by the engine at this budget limit.
    Budget {
        limit: String,
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
        policy: delegation::effective_policy(&record, &turn.options.policy),
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
        } else {
            let driven = drive(&mut recorder, &turn, &record, &bounds, &lease)?;
            conclude(
                turn.yard,
                turn.prompt,
                &fence,
                &mut record,
                &mut recorder,
                driven,
            )?;
        }
        Ok(())
    })();
    let result = match result {
        Ok(()) => recorder.finish(lease, &record),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => Ok(Branch {
            yard: turn.yard.clone(),
            info: record.info,
        }),
        Err(error @ Error::Fenced(_)) => Err(error),
        Err(error) => {
            record.info.status = BranchStatus::Failed {
                reason: error.to_string(),
            };
            let _ = store.backend().finish(&fence, Some(&record), None);
            Err(error)
        }
    }
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
fn exhausted(store: &crate::state::Store, record: &Record, budget: &Budget) -> Option<String> {
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

/// The branch's own share of a cumulative estimate.
fn spent(reported: f64, baseline: Option<f64>) -> f64 {
    (reported - baseline.unwrap_or(0.0)).max(0.0)
}

fn drive(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &Record,
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
        .map(|limit| now_ms().saturating_add(limit.as_millis() as u64));
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

#[allow(clippy::too_many_arguments)]
fn run(
    recorder: &mut Recorder,
    turn: &Turn<'_>,
    record: &Record,
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
    // The one path for MCP servers and instructions, the task's and the
    // delegation tools', and for everything else the home needs. Applied
    // before a sandbox exists, so its mount or home transfer carries it.
    let provisioned = match crate::provisioning::prepare(
        record,
        turn.profile,
        projection.as_ref(),
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
    let mut placement = match Placement::prepare(turn.yard, record, fence) {
        Ok(placement) => placement,
        Err(reason) => {
            driven.end = End::failed(reason);
            return Ok(driven);
        }
    };
    // Every local harness learns which branch it is on, so `by` inside it
    // never mistakes it for a person; only a delegating one gets a token.
    if !placement.is_sandbox() {
        placement.set_env(ENV_ROOT, &turn.yard.root.display().to_string());
        placement.set_env(ENV_BRANCH, &record.info.name);
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
    // A per-turn MCP file lives until this function returns, after the
    // harness is gone.
    let _turn_file = provisioned.turn_file;
    let open = Open {
        mcp_servers: provisioned.session.mcp_servers,
        instructions: provisioned.session.instructions,
        model: provisioned.session.model,
        mcp_config_file: provisioned.mcp_config_file,
        remote_mcp_servers: provisioned.session.remote_mcp_servers,
        ..Open::new(turn.mode.clone(), placement.cwd())
    };
    let driver = turn.profile.driver_with(turn.command.clone());
    let mut steering = Steering::new(turn.profile.id, driver.capabilities().steer);
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
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "another worktree".into()),
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
                if let Err(error) = session.interrupt() {
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
                if let Err(error) = session.interrupt() {
                    recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                    kill = true;
                    break End::budget("max_duration");
                }
                phase = Phase::Stopping {
                    turn: n,
                    why: Stop::Limit("max_duration"),
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
        recorder.record(Activity::Harness(event.clone()))?;
        steering.event(&store, fence, &event)?;
        match event {
            Event::Ready if matches!(phase, Phase::Opening) => {
                recorder.record(Activity::Prompt(turn.prompt.to_owned()))?;
                // Journaled first: from here the prompt may have reached the
                // harness, and recovery must never submit it again.
                let intent = json!({ "prompt": turn.prompt });
                store
                    .backend()
                    .begin_step(fence, fence.turn, STEP_SUBMIT, &intent)?;
                match session.submit(turn.prompt) {
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
                if let Some(reason) = answer(
                    recorder,
                    &mut session,
                    turn,
                    &bounds.policy,
                    &request,
                    stopping,
                )? {
                    if let Phase::Running(n) = phase {
                        if session.interrupt().is_err() {
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
            Event::UsageObserved { usage, .. } if usage.cumulative => {
                let Some(cost) = usage.cost_usd else { continue };
                driven.cost = Some(driven.cost.map_or(cost, |c: f64| c.max(cost)));
                if let Some(projection) = &projection {
                    projection.observe_cost(spent(cost, record.cost_baseline));
                }
                let over = bounds.budget.max_usd.is_some_and(|max| {
                    spent(cost, record.cost_baseline) + delegation::reserved(&store, record) > max
                });
                if let (true, Phase::Running(n)) = (over, &phase) {
                    let n = *n;
                    if let Err(error) = session.interrupt() {
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
                break match phase {
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

    if !kill {
        // Whatever the harness already sent after the turn ended.
        for _ in 0..DRAIN_MAX {
            match session.next_event(Duration::ZERO) {
                Ok(Some(event)) => {
                    recorder.record(Activity::Harness(event.clone()))?;
                    steering.event(&store, fence, &event)?;
                }
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
                for event in events {
                    recorder.record(Activity::Harness(event))?;
                }
            }
            Err(error) => recorder.record(Activity::Warning(format!("kill failed: {error}")))?,
        }
        if let Some(warning) = placement.release() {
            recorder.record(Activity::Warning(warning))?;
        }
        return Ok(driven);
    }
    match session.close(CLOSE_GRACE) {
        Ok(closed) => {
            for event in closed.events {
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
    if let Some(warning) = placement.release() {
        recorder.record(Activity::Warning(warning))?;
    }
    Ok(driven)
}

/// This turn's steered input: polled from the store at most every
/// [`TICK`], written to the harness while the turn runs, and settled as the
/// harness answers.
struct Steering {
    profile: &'static str,
    /// Whether the driver can take input mid-turn at all.
    offered: bool,
    /// Store IDs of the input written to the driver, in the driver's
    /// numbering from 1.
    written: Vec<u64>,
    next_poll: Instant,
}

impl Steering {
    fn new(profile: &'static str, offered: bool) -> Steering {
        Steering {
            profile,
            offered,
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
                _ => match session.steer(&row.text) {
                    Ok(()) => {
                        self.written.push(row.id);
                        recorder.record(Activity::Steered {
                            id: row.id,
                            by: row.by.clone(),
                            text: row.text.clone(),
                        })?;
                        store
                            .backend()
                            .settle_steer(fence, row.id, &SteerState::Delivered)?;
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
    fn event(&mut self, store: &Store, fence: &Fence, event: &Event) -> Result<(), Error> {
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
            Some(id) => store.backend().settle_steer(fence, *id, &state),
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
) -> Result<Option<String>, Error> {
    let (decision, source) = if stopping {
        let decision = PermissionDecision::Deny {
            message: "The turn is being interrupted.".into(),
        };
        (decision, DecisionSource::Engine)
    } else {
        policy.decide_with_source(&turn.record.info.name, request)
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
    let info = &mut record.info;
    if driven.submitted {
        info.turns += 1;
    }
    if let Some(session) = &driven.session {
        info.session = Some(session.to_string());
    }
    if let Some(cost) = driven.cost {
        info.cost_usd = Some(spent(cost, record.cost_baseline));
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
            let snapshot = {
                let _lock = git::lock();
                let branch = names::validate(&info.name)?;
                match yard.repo.workspace(&branch) {
                    Ok(Some(workspace)) => workspace.snapshot(&message).map_err(|e| e.to_string()),
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
        info.candidate = candidate;
    }
    if !replayed {
        store
            .backend()
            .finish_step(fence, fence.turn, STEP_SNAPSHOT, &to_value(&snapshotted))?;
    }
    info.status = match driven.end {
        End::Outcome {
            outcome: TurnOutcome::Completed,
        } if changed && info.candidate.is_some() => BranchStatus::Ready,
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
        End::Budget { limit } => BranchStatus::BudgetExceeded { limit },
        End::Cancelled { by } => {
            recorder.record(Activity::Warning(format!("cancelled by {by}")))?;
            BranchStatus::Interrupted
        }
        End::Failed { reason } => BranchStatus::Failed { reason },
        End::Lost { reason } => {
            recorder.record(Activity::Warning(reason))?;
            BranchStatus::Interrupted
        }
    };
    if let Some(error) = snapshotted.error {
        info.status = match &info.status {
            BranchStatus::Failed { reason } => BranchStatus::Failed {
                reason: format!("{reason}; {error}"),
            },
            _ => BranchStatus::Failed { reason: error },
        };
    }
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
}
