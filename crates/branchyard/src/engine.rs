//! One turn on one branch: start the harness in the branch's worktree,
//! submit the prompt, answer and record everything until the turn ends,
//! enforce the budget, snapshot the candidate and close the harness.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::Profile;
use branchyard_harness::{Open, SessionMode};
use branchyard_runtime::{RuntimeError, Session};

use crate::record::Recorder;
use crate::state::Record;
use crate::{
    git, harness, names, Activity, Branch, BranchStatus, CandidateInfo, DecisionSource, Error,
    Event, NativeSession, PermissionDecision, PermissionRequest, TaskOptions, TurnOutcome, Yard,
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
}

/// How a turn ended, before the snapshot.
enum End {
    Outcome(TurnOutcome),
    /// Stopped by the engine at this budget limit.
    Budget(String),
    Failed(String),
}

struct Driven {
    end: End,
    submitted: bool,
    session: Option<NativeSession>,
    /// The harness's latest cumulative cost estimate.
    cost: Option<f64>,
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
    Failure(String),
}

/// Run the turn and record its result. Harness failures become the
/// branch's status; errors are state errors, after which the record says
/// `Failed` if it could still be written.
pub(crate) fn execute(turn: Turn<'_>) -> Result<Branch, Error> {
    let store = turn.yard.store();
    let mut record = turn.record.clone();
    let name = record.info.name.clone();
    let mut recorder = Recorder::open(&store, &name, turn.options.observer.clone())?;
    let result = (|| {
        recorder.record(Activity::Status(record.info.status.clone()))?;
        if matches!(record.info.status, BranchStatus::Failed { .. }) {
            return Ok(());
        }
        if let Some(limit) = exhausted(&record, turn.options) {
            record.info.status = BranchStatus::BudgetExceeded { limit };
        } else {
            let driven = drive(&mut recorder, &turn, &record)?;
            conclude(&turn, &mut record, &mut recorder, driven)?;
        }
        store.write(&record)?;
        recorder.record(Activity::Status(record.info.status.clone()))
    })();
    if let Err(error) = result {
        record.info.status = BranchStatus::Failed {
            reason: error.to_string(),
        };
        let _ = store.write(&record);
        return Err(error);
    }
    Ok(Branch {
        yard: turn.yard.clone(),
        info: record.info,
    })
}

/// A limit already used up before the turn starts.
fn exhausted(record: &Record, options: &TaskOptions) -> Option<String> {
    let budget = &options.budget;
    if budget.max_turns.is_some_and(|max| record.info.turns >= max) {
        return Some("max_turns".into());
    }
    if let (Some(max), Some(spent)) = (budget.max_usd, record.info.cost_usd) {
        if spent >= max {
            return Some("max_usd".into());
        }
    }
    None
}

/// The branch's own share of a cumulative estimate.
fn spent(reported: f64, baseline: Option<f64>) -> f64 {
    (reported - baseline.unwrap_or(0.0)).max(0.0)
}

fn drive(recorder: &mut Recorder, turn: &Turn<'_>, record: &Record) -> Result<Driven, Error> {
    let started = Instant::now();
    let deadline = turn
        .options
        .budget
        .max_duration
        .and_then(|limit| started.checked_add(limit));
    let env = harness::environment(record.home.as_deref());
    let open = Open {
        mode: turn.mode.clone(),
        cwd: record.info.worktree.display().to_string(),
        model: None,
    };
    let mut driven = Driven {
        end: End::Failed(String::new()),
        submitted: false,
        session: None,
        cost: None,
    };
    let driver = turn.profile.driver_with(turn.command.clone());
    let mut session = match Session::start(driver, open, &env, None) {
        Ok(session) => session,
        Err(error) => {
            driven.end = End::Failed(match error {
                RuntimeError::Spawn { argv, source } => {
                    format!("could not start {}: {source}", argv.join(" "))
                }
                RuntimeError::Rejected(rejected) => format!("the driver refused: {rejected}"),
                other => other.to_string(),
            });
            return Ok(driven);
        }
    };
    let mut phase = Phase::Opening;
    // Whether the harness has named the session it runs. Until it has, a
    // resume or fork may have landed somewhere else.
    let mut confirmed = false;
    let mut kill = false;
    let fail = |confirmed: bool, detail: String| -> End {
        End::Failed(match (&turn.mode, confirmed) {
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
        match &phase {
            Phase::Opening if late => {
                kill = true;
                break End::Budget("max_duration".into());
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
                    break End::Budget("max_duration".into());
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
                    Stop::Limit(limit) => End::Budget((*limit).into()),
                    Stop::Failure(reason) => End::Failed(reason.clone()),
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
        match event {
            Event::Ready if matches!(phase, Phase::Opening) => {
                recorder.record(Activity::Prompt(turn.prompt.to_owned()))?;
                match session.submit(turn.prompt) {
                    Ok(n) => {
                        driven.submitted = true;
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
                if let Some(reason) = answer(recorder, &mut session, turn, &request, stopping)? {
                    if let Phase::Running(n) = phase {
                        if session.interrupt().is_err() {
                            kill = true;
                            break End::Failed(reason);
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
                let over = turn
                    .options
                    .budget
                    .max_usd
                    .is_some_and(|max| spent(cost, record.cost_baseline) > max);
                if let (true, Phase::Running(n)) = (over, &phase) {
                    let n = *n;
                    if let Err(error) = session.interrupt() {
                        recorder.record(Activity::Warning(format!("interrupt failed: {error}")))?;
                        kill = true;
                        break End::Budget("max_usd".into());
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
                    } => End::Budget(limit.into()),
                    Phase::Stopping {
                        why: Stop::Failure(reason),
                        ..
                    } => End::Failed(reason),
                    _ => End::Outcome(outcome),
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
                Ok(Some(event)) => recorder.record(Activity::Harness(event))?,
                _ => break,
            }
        }
    }
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
    Ok(driven)
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
    request: &PermissionRequest,
    stopping: bool,
) -> Result<Option<String>, Error> {
    let (decision, source) = if stopping {
        let decision = PermissionDecision::Deny {
            message: "The turn is being interrupted.".into(),
        };
        (decision, DecisionSource::Engine)
    } else {
        turn.options
            .policy
            .decide_with_source(&turn.record.info.name, request)
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

/// Snapshot the candidate and settle the branch's record after the turn.
fn conclude(
    turn: &Turn<'_>,
    record: &mut Record,
    recorder: &mut Recorder,
    driven: Driven,
) -> Result<(), Error> {
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
    let message = format!(
        "{}: turn {}\n\n{}\n",
        info.git_branch, info.turns, turn.prompt
    );
    let snapshot = {
        let _lock = git::lock();
        let branch = names::validate(&info.name)?;
        match turn.yard.repo.workspace(&branch) {
            Ok(Some(workspace)) => workspace.snapshot(&message).map_err(|e| e.to_string()),
            Ok(None) => Err(format!("no worktree has {} checked out", info.git_branch)),
            Err(error) => Err(error.to_string()),
        }
    };
    let previous = info.candidate.as_ref().map(|c| c.commit.clone());
    let mut changed = false;
    let snapshot_error = match snapshot {
        Ok(candidate) => {
            let candidate = candidate.map(|c| CandidateInfo {
                commit: c.head.0,
                files_changed: c.stat.files_changed as u32,
                insertions: c.stat.insertions as u32,
                deletions: c.stat.deletions as u32,
            });
            changed = candidate.as_ref().map(|c| &c.commit) != previous.as_ref();
            if let (true, Some(candidate)) = (changed, &candidate) {
                recorder.record(Activity::Snapshot(candidate.clone()))?;
            }
            info.candidate = candidate;
            None
        }
        Err(error) => Some(format!("snapshot failed: {error}")),
    };
    info.status = match driven.end {
        End::Outcome(TurnOutcome::Completed) if changed && info.candidate.is_some() => {
            BranchStatus::Ready
        }
        End::Outcome(TurnOutcome::Completed) => BranchStatus::NoChanges,
        End::Outcome(TurnOutcome::Interrupted) => BranchStatus::Interrupted,
        End::Outcome(TurnOutcome::Failed { message }) => BranchStatus::Failed { reason: message },
        End::Outcome(TurnOutcome::LimitReached { limit }) => BranchStatus::BudgetExceeded {
            limit: format!("harness {limit}"),
        },
        End::Outcome(TurnOutcome::Refused) => BranchStatus::Failed {
            reason: "the model refused to continue".into(),
        },
        End::Budget(limit) => BranchStatus::BudgetExceeded { limit },
        End::Failed(reason) => BranchStatus::Failed { reason },
    };
    if let Some(error) = snapshot_error {
        info.status = match &info.status {
            BranchStatus::Failed { reason } => BranchStatus::Failed {
                reason: format!("{reason}; {error}"),
            },
            _ => BranchStatus::Failed { reason: error },
        };
    }
    Ok(())
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
