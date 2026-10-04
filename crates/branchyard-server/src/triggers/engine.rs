//! The trigger dispatcher: what every server and `by worker` runs beside
//! its operation dispatcher, for the repositories it serves.
//!
//! Each tick it
//!
//! 1. claims every due schedule time ([`Engine::claim_due`]): the time
//!    that fell most recently within the catch-up window fires once, older
//!    ones are recorded together as one `missed` run, and the trigger's
//!    next time is set, all in one compare-and-set transaction, so two
//!    dispatchers never claim the same time;
//! 2. claims pending runs ([`Engine::claim_pending`]): those a webhook
//!    recorded, and those whose dispatcher stopped before finishing them;
//! 3. fires each claimed run ([`Engine::fire`]): it resolves the
//!    repository, renders the task, routes it if asked, runs the precheck
//!    in a fresh worktree, and admits the task through the server's
//!    admission path with the idempotency key `trigger:<id>` /
//!    `<run key>`, so a run fired twice (its dispatcher died after
//!    admitting, before recording) is the same operation;
//! 4. settles fired runs whose task ended ([`Engine::settle`]), counting a
//!    failure toward pausing the trigger.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use branchyard_client::api::{Operation, OperationState, TaskRequest};
use branchyard_client::triggers::{RunOutcome, RunState, TriggerRun};

use sha2::Digest;

use super::store::TriggerStore;
use super::template::{self, Context};
use super::{precheck, target, Clock, Schedule, StoredTrigger};

/// How long a dispatcher holds a run it claimed: the longest precheck
/// and a margin. A run whose dispatcher died is fired by another after
/// this; the idempotency key keeps that from admitting a second task.
pub const RUN_LEASE_MS: u64 = (precheck::MAX_TIMEOUT_SECONDS + 60) * 1000;

/// How long a run given back for later waits before any dispatcher fires
/// it again.
pub const DEFER_MS: u64 = 30_000;

/// Most scheduled times a claim counts one by one when recording what was
/// missed; more are reported as at least this many.
const MAX_MISSED_COUNTED: u64 = 100_000;

/// Why the sink admitted no task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The task was refused: the run failed, and counts toward pausing.
    Failed(String),
    /// Not now (the server is stopping, or the registry cannot be read):
    /// the run stays pending, and any dispatcher may fire it after
    /// [`DEFER_MS`] (at once after a restart on a data directory).
    Later(String),
}

/// A task the sink admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    pub operation: String,
    pub branches: Vec<String>,
}

/// What firing needs from the server it runs in.
pub trait Sink: Send + Sync {
    /// The repositories this process serves: (name, root).
    fn repos(&self) -> Vec<(String, PathBuf)>;
    /// Admit `request` as `trigger`'s principal, with the idempotency key
    /// `key` scoped to the trigger; the operation, or why not.
    fn admit(
        &self,
        trigger: &StoredTrigger,
        key: &str,
        request: TaskRequest,
    ) -> Result<Admitted, Refusal>;
    /// An operation as the registry has it now.
    fn operation(&self, id: &str) -> Option<Operation>;
    /// Whether the operator lets `repo`'s triggers run prechecks.
    fn prechecks_allowed(&self, repo: &str) -> bool;
    /// `request` with a harness the repository's fleet table picked, the
    /// router seeded with `seed` (from the run's key, so firing the run
    /// again picks the same).
    fn route(
        &self,
        trigger: &StoredTrigger,
        request: TaskRequest,
        seed: u64,
    ) -> Result<TaskRequest, String>;
    /// Where precheck worktrees go.
    fn scratch(&self) -> PathBuf;
}

pub struct Engine {
    pub store: Arc<dyn TriggerStore>,
    pub clock: Clock,
    pub sink: Arc<dyn Sink>,
    /// This dispatcher's name on its claims.
    pub worker: String,
}

/// What one [`Engine::tick`] did.
#[derive(Debug, Default)]
pub struct Tick {
    pub fired: Vec<TriggerRun>,
    pub settled: usize,
}

fn new_run(trigger: &str, key: String, state: RunState, at_ms: u64) -> TriggerRun {
    TriggerRun {
        id: super::new_id("run"),
        trigger: trigger.to_owned(),
        key,
        state,
        at_ms,
        scheduled_ms: None,
        missed: None,
        last_missed_ms: None,
        event: None,
        reason: None,
        precheck: None,
        operation: None,
        branches: Vec::new(),
        outcome: None,
        finished_at_ms: None,
    }
}

/// The least catch-up window: a tick that runs a little late never
/// misses the time it was due for.
pub const MIN_CATCH_UP_MS: u64 = 60_000;

/// The scheduled times of a schedule from `first` (a due time) to `upto`:
/// the latest, and those before `cutoff`, which are missed. The latest is
/// among the missed when it is before `cutoff` too.
#[derive(Debug, PartialEq)]
struct Slots {
    latest: Option<u64>,
    missed: u64,
    first_missed: Option<u64>,
    last_missed: Option<u64>,
}

fn slots(schedule: &Schedule, first: u64, upto: u64, cutoff: u64) -> Slots {
    let mut out = Slots {
        latest: None,
        missed: 0,
        first_missed: None,
        last_missed: None,
    };
    if first > upto {
        return out;
    }
    if let Schedule::Every(every) = schedule {
        // Arithmetic, however long the server was down.
        let n = (upto - first) / every;
        out.latest = Some(first + n * every);
        if first < cutoff {
            let m = ((cutoff - 1 - first) / every).min(n);
            out.missed = m + 1;
            out.first_missed = Some(first);
            out.last_missed = Some(first + m * every);
        }
        return out;
    }
    let mut t = first;
    loop {
        if t < cutoff && out.missed < MAX_MISSED_COUNTED {
            out.missed += 1;
            out.first_missed.get_or_insert(t);
            out.last_missed = Some(t);
        }
        out.latest = Some(t);
        // Past the most it counts, skip to the window's start.
        let from = match out.missed >= MAX_MISSED_COUNTED && t < cutoff {
            true => cutoff - 1,
            false => t,
        };
        match schedule.next_after(from, first) {
            Some(next) if next <= upto => t = next,
            _ => break,
        }
    }
    out
}

impl Engine {
    fn repo_names(&self) -> Vec<String> {
        self.sink
            .repos()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Claim every due schedule time of the repositories served here; the
    /// runs to fire, each with its fence.
    pub fn claim_due(&self) -> io::Result<Vec<(TriggerRun, i64)>> {
        let now = self.clock.now();
        let mut claimed = Vec::new();
        for trigger in self.store.due(&self.repo_names(), now)? {
            let Some(expected) = trigger.next_due_ms else {
                continue;
            };
            let schedule = match Schedule::of(&trigger.spec.when) {
                Ok(Some(schedule)) => schedule,
                Ok(None) => continue,
                Err(e) => {
                    tracing::error!(trigger = %trigger.spec.name, error = %e, "unreadable schedule");
                    continue;
                }
            };
            let window = (trigger.spec.policy.catch_up_seconds * 1000).max(MIN_CATCH_UP_MS);
            let found = slots(&schedule, expected, now, now.saturating_sub(window));
            let Some(latest) = found.latest else {
                continue;
            };
            let next = schedule.next_after(now, latest);
            // Times between the missed ones and the latest fire once, as
            // the latest; the latest is missed only when it is too old.
            let fire = found.last_missed != Some(latest);
            let mut runs = Vec::new();
            if let (count @ 1.., Some(first), Some(last)) =
                (found.missed, found.first_missed, found.last_missed)
            {
                let mut run = new_run(
                    &trigger.id,
                    format!("missed:{first}"),
                    RunState::Missed,
                    now,
                );
                run.scheduled_ms = Some(first);
                run.missed = Some(count);
                run.last_missed_ms = Some(last);
                run.reason = Some(format!(
                    "{count} scheduled time{} passed while no dispatcher could fire {}, more \
                     than the {}s catch-up window ago",
                    if count == 1 { "" } else { "s" },
                    if count == 1 { "it" } else { "them" },
                    window / 1000
                ));
                run.finished_at_ms = Some(now);
                runs.push(run);
            }
            let pending = fire.then(|| {
                let mut run = new_run(
                    &trigger.id,
                    format!("schedule:{latest}"),
                    RunState::Pending,
                    now,
                );
                run.scheduled_ms = Some(latest);
                run
            });
            runs.extend(pending.clone());
            if self.store.claim_schedule(
                &trigger.id,
                expected,
                next,
                &runs,
                &self.worker,
                now + RUN_LEASE_MS,
            )? {
                claimed.extend(pending.map(|run| (run, 1)));
            }
        }
        Ok(claimed)
    }

    /// Claim one pending run nobody holds, or whose holder stopped.
    pub fn claim_pending(&self) -> io::Result<Option<(TriggerRun, i64)>> {
        let now = self.clock.now();
        self.store
            .claim_run(&self.repo_names(), &self.worker, now, now + RUN_LEASE_MS)
    }

    /// Fire a claimed run, record what happened, and return it.
    pub fn fire(&self, mut run: TriggerRun, fence: i64) -> io::Result<TriggerRun> {
        let trigger = self.store.get(&run.trigger)?;
        let pause_after = trigger
            .as_ref()
            .map(|t| t.spec.policy.pause_after_failures)
            .unwrap_or(0);
        match trigger {
            None => {
                run.state = RunState::SkippedDisabled;
                run.reason = Some("the trigger was removed before it fired".into());
            }
            Some(t) if !t.enabled => {
                run.state = RunState::SkippedDisabled;
                run.reason = Some("the trigger was disabled before it fired".into());
            }
            Some(t) => self.attempt(&t, &mut run),
        }
        if run.state == RunState::Pending {
            // Not now: any dispatcher may fire it a little later.
            let until = self.clock.now() + DEFER_MS;
            self.store.defer_run(&run.id, fence, until)?;
            return Ok(run);
        }
        run.finished_at_ms = Some(self.clock.now());
        let accounted = self.store.finish_run(&run, fence, pause_after)?;
        if let Some(why) = &accounted.paused {
            tracing::warn!(trigger = %run.trigger, reason = %why, "trigger paused");
        }
        if !accounted.written {
            tracing::warn!(run = %run.id, "another dispatcher took this trigger run over");
        }
        Ok(run)
    }

    /// The task `run` of `trigger` creates, rendered, with its branch name.
    pub fn render(trigger: &StoredTrigger, run: &TriggerRun) -> Result<TaskRequest, String> {
        let spec = &trigger.spec;
        let cx = Context {
            event: run.event.as_ref(),
            trigger_id: &trigger.id,
            trigger_name: &spec.name,
            trigger_repo: &spec.repo,
            run_id: &run.id,
            scheduled_at_ms: run.scheduled_ms.unwrap_or(run.at_ms),
        };
        let mut request = spec.task.clone();
        request.prompt = template::render(&spec.task.prompt, &cx)?;
        let name = match &spec.task.name {
            Some(name) => template::render(name, &cx)?,
            None => default_name(trigger, run),
        };
        request.name = Some(template::branch_name(&name));
        Ok(request)
    }

    fn attempt(&self, trigger: &StoredTrigger, run: &mut TriggerRun) {
        let fail = |run: &mut TriggerRun, why: String| {
            run.state = RunState::Failed;
            run.reason = Some(why);
        };
        let root = match target::resolve(&trigger.spec.repo, &self.sink.repos()) {
            Ok(root) => root,
            Err(why) => return fail(run, why.to_owned()),
        };
        let mut request = match Engine::render(trigger, run) {
            Ok(request) => request,
            Err(e) => return fail(run, format!("rendering the task: {e}")),
        };
        if trigger.spec.route.is_some() {
            let seed = u64::from_be_bytes(
                sha2::Sha256::digest(run.key.as_bytes())[..8]
                    .try_into()
                    .expect("eight bytes"),
            );
            request = match self.sink.route(trigger, request, seed) {
                Ok(request) => request,
                Err(e) => return fail(run, format!("routing: {e}")),
            };
        }
        if let Some(check) = &trigger.spec.precheck {
            if !self.sink.prechecks_allowed(&trigger.spec.repo) {
                return fail(
                    run,
                    format!(
                        "this server does not allow prechecks for repository {} \
                         (allow_trigger_prechecks)",
                        trigger.spec.repo
                    ),
                );
            }
            match run_precheck(&self.sink.scratch(), &root, trigger, run, check) {
                Err(e) => return fail(run, e),
                Ok(result) => {
                    let passed = result.passed();
                    if !passed {
                        run.state = RunState::SkippedPrecheck;
                        run.reason = Some(precheck::failure(&result));
                    }
                    run.precheck = Some(result);
                    if !passed {
                        return;
                    }
                }
            }
        }
        match self.sink.admit(trigger, &run.key, request) {
            Ok(admitted) => {
                run.state = RunState::Fired;
                run.operation = Some(admitted.operation);
                run.branches = admitted.branches;
                run.reason = None;
            }
            Err(Refusal::Failed(e)) => fail(run, e),
            Err(Refusal::Later(why)) => run.reason = Some(why),
        }
    }

    /// Record how fired runs' tasks ended, for those that have.
    pub fn settle(&self) -> io::Result<usize> {
        let mut settled = 0;
        for run in self.store.unsettled(&self.repo_names())? {
            let Some(id) = &run.operation else {
                continue;
            };
            let Some(op) = self.sink.operation(id) else {
                continue;
            };
            let Some(outcome) = outcome(&op) else {
                continue;
            };
            let pause_after = self
                .store
                .get(&run.trigger)?
                .map(|t| t.spec.policy.pause_after_failures)
                .unwrap_or(0);
            let accounted = self.store.settle(&run.id, &outcome, pause_after)?;
            if let Some(why) = &accounted.paused {
                tracing::warn!(trigger = %run.trigger, reason = %why, "trigger paused");
            }
            settled += usize::from(accounted.written);
        }
        Ok(settled)
    }

    /// Claim, fire and settle everything due now, one run at a time.
    pub fn tick(&self) -> io::Result<Tick> {
        let mut tick = Tick::default();
        for (run, fence) in self.claim_due()? {
            tick.fired.push(self.fire(run, fence)?);
        }
        while let Some((run, fence)) = self.claim_pending()? {
            tick.fired.push(self.fire(run, fence)?);
        }
        tick.settled = self.settle()?;
        Ok(tick)
    }
}

/// `<trigger>-<number>-<hash>` for an event, `<trigger>-<yyyymmdd-hhmm>`
/// for a schedule.
fn default_name(trigger: &StoredTrigger, run: &TriggerRun) -> String {
    let name = &trigger.spec.name;
    match &run.event {
        Some(event) => {
            let hash = hex::encode(sha2::Sha256::digest(event.id.as_bytes()));
            match &event.number {
                Some(n) => format!("{name}-{n}-{}", &hash[..6]),
                None => format!("{name}-{}", &hash[..8]),
            }
        }
        None => {
            let at = template::rfc3339(run.scheduled_ms.unwrap_or(run.at_ms));
            // 2026-10-01T09:00:00Z -> 20261001-0900
            let digits: String = at.chars().filter(char::is_ascii_digit).collect();
            format!("{name}-{}-{}", &digits[..8], &digits[8..12])
        }
    }
}

/// Run `check` in a fresh worktree of `root` for `run`.
pub fn run_precheck(
    scratch: &std::path::Path,
    root: &std::path::Path,
    trigger: &StoredTrigger,
    run: &TriggerRun,
    check: &branchyard_client::triggers::Precheck,
) -> Result<branchyard_client::triggers::PrecheckResult, String> {
    let dir = scratch.join("prechecks").join(&run.id);
    let worktree = precheck::Worktree::add(root, &dir, trigger.spec.task.base.as_deref())
        .map_err(|e| format!("making the precheck's worktree: {e}"))?;
    let mut env = vec![
        ("BRANCHYARD_TRIGGER".to_owned(), trigger.spec.name.clone()),
        ("BRANCHYARD_TRIGGER_RUN".to_owned(), run.key.clone()),
    ];
    let event_file = dir.with_extension("event.json");
    if let Some(event) = &run.event {
        let text = serde_json::to_vec_pretty(event).map_err(|e| e.to_string())?;
        std::fs::write(&event_file, text).map_err(|e| format!("{}: {e}", event_file.display()))?;
        env.push((
            "BRANCHYARD_TRIGGER_EVENT".to_owned(),
            event_file.display().to_string(),
        ));
    }
    let result = precheck::run(
        &check.command,
        std::time::Duration::from_secs(check.timeout_seconds),
        &worktree.path,
        &env,
    );
    drop(worktree);
    branchyard_support::cleanup_file(&event_file);
    Ok(result)
}

/// How a finished operation's task ended; `None` while it runs.
pub fn outcome(op: &Operation) -> Option<RunOutcome> {
    match op.state {
        OperationState::Queued | OperationState::Running => None,
        OperationState::Failed | OperationState::Interrupted => Some(RunOutcome {
            ok: false,
            detail: match &op.error {
                Some(e) => format!("operation {} {}: {}", op.id, state(op.state), e.message),
                None => format!("operation {} {}", op.id, state(op.state)),
            },
        }),
        OperationState::Succeeded => {
            let branches = op
                .result
                .as_ref()
                .map(|r| r.branches.as_slice())
                .unwrap_or_default();
            let bad = branches.iter().find(|b| {
                matches!(
                    b.status,
                    branchyard::BranchStatus::Failed { .. } | branchyard::BranchStatus::Interrupted
                )
            });
            Some(match bad {
                Some(b) => RunOutcome {
                    ok: false,
                    detail: format!("branch {} ended {}", b.name, status(&b.status)),
                },
                None => RunOutcome {
                    ok: true,
                    detail: format!(
                        "operation {} succeeded{}",
                        op.id,
                        match branches.is_empty() {
                            true => String::new(),
                            false => format!(
                                ": {}",
                                branches
                                    .iter()
                                    .map(|b| format!("{} {}", b.name, status(&b.status)))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        }
                    ),
                },
            })
        }
    }
}

fn status(status: &branchyard::BranchStatus) -> String {
    use branchyard::BranchStatus as S;
    match status {
        S::Running => "running".into(),
        S::Waiting => "waiting".into(),
        S::Blocked { reason } => format!("blocked ({reason})"),
        S::Ready => "ready".into(),
        S::NoChanges => "with no changes".into(),
        S::Interrupted => "interrupted".into(),
        S::BudgetExceeded { limit } => format!("over its {limit} budget"),
        S::Failed { reason } => format!("failed ({reason})"),
        S::Merged { target, .. } => format!("merged into {target}"),
        S::AwaitingPlanApproval => "awaiting approval of its plan".into(),
    }
}

fn state(state: OperationState) -> &'static str {
    match state {
        OperationState::Queued => "queued",
        OperationState::Running => "running",
        OperationState::Succeeded => "succeeded",
        OperationState::Failed => "failed",
        OperationState::Interrupted => "was interrupted",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use branchyard_client::triggers::{TriggerEvent, When};

    use super::*;
    use crate::triggers::store::{conformance, SqliteTriggers};

    /// Records admissions; admits with the idempotency key as the store's
    /// unique index would.
    #[derive(Default)]
    struct FakeSink {
        root: PathBuf,
        admitted: Mutex<BTreeMap<String, TaskRequest>>,
        refuse: Mutex<Option<String>>,
        ops: Mutex<BTreeMap<String, Operation>>,
    }

    impl Sink for FakeSink {
        fn repos(&self) -> Vec<(String, PathBuf)> {
            vec![("app".into(), self.root.clone())]
        }
        fn admit(
            &self,
            t: &StoredTrigger,
            key: &str,
            request: TaskRequest,
        ) -> Result<Admitted, Refusal> {
            if let Some(why) = self.refuse.lock().unwrap().clone() {
                return Err(match why.strip_prefix("later: ") {
                    Some(why) => Refusal::Later(why.to_owned()),
                    None => Refusal::Failed(why),
                });
            }
            let id = format!("op:{}:{key}", t.id);
            self.admitted
                .lock()
                .unwrap()
                .entry(id.clone())
                .or_insert(request);
            Ok(Admitted {
                operation: id,
                branches: vec!["b".into()],
            })
        }
        fn operation(&self, id: &str) -> Option<Operation> {
            self.ops.lock().unwrap().get(id).cloned()
        }
        fn prechecks_allowed(&self, _: &str) -> bool {
            true
        }
        fn route(&self, _: &StoredTrigger, r: TaskRequest, _: u64) -> Result<TaskRequest, String> {
            Ok(r)
        }
        fn scratch(&self) -> PathBuf {
            std::env::temp_dir()
        }
    }

    fn checkout() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "by-engine-{}-{}",
            std::process::id(),
            crate::triggers::new_id("t")
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn engines(
        n: usize,
        clock: &Arc<AtomicU64>,
    ) -> (Vec<Engine>, Arc<FakeSink>, Arc<SqliteTriggers>) {
        let store = Arc::new(SqliteTriggers::memory());
        let sink = Arc::new(FakeSink {
            root: checkout(),
            ..FakeSink::default()
        });
        let engines = (0..n)
            .map(|i| Engine {
                store: store.clone(),
                clock: Clock::manual(clock.clone()),
                sink: sink.clone(),
                worker: format!("w{i}"),
            })
            .collect();
        (engines, sink, store)
    }

    fn scheduled(store: &SqliteTriggers, when: When, next_due: u64) -> StoredTrigger {
        let mut t = conformance::trigger("default", "nightly", true);
        t.spec.when = when;
        t.next_due_ms = Some(next_due);
        assert!(store.create(&t).unwrap());
        t
    }

    const MINUTE: u64 = 60_000;

    #[test]
    fn two_dispatchers_fire_a_scheduled_time_once() {
        let clock = Arc::new(AtomicU64::new(10 * MINUTE));
        let (engines, sink, store) = engines(4, &clock);
        let t = scheduled(&store, When::Interval { seconds: 60 }, 10 * MINUTE);
        // Four dispatchers tick at the same instant, on threads.
        let fired: Vec<usize> = std::thread::scope(|s| {
            let handles: Vec<_> = engines
                .iter()
                .map(|e| s.spawn(move || e.tick().unwrap().fired.len()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(fired.iter().sum::<usize>(), 1, "{fired:?}");
        assert_eq!(sink.admitted.lock().unwrap().len(), 1);
        let got = store.get(&t.id).unwrap().unwrap();
        assert_eq!(got.next_due_ms, Some(11 * MINUTE));
        // Nothing more until the next minute; then once more.
        for e in &engines {
            assert!(e.tick().unwrap().fired.is_empty());
        }
        clock.store(11 * MINUTE, Ordering::SeqCst);
        let total: usize = engines.iter().map(|e| e.tick().unwrap().fired.len()).sum();
        assert_eq!(total, 1);
        let runs = store.runs(&t.id, 10).unwrap();
        let keys: Vec<&str> = runs.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                format!("schedule:{}", 11 * MINUTE),
                format!("schedule:{}", 10 * MINUTE)
            ]
        );
        assert!(runs.iter().all(|r| r.state == RunState::Fired));
        let task = sink
            .admitted
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(task.name.as_deref(), Some("nightly-19700101-0010"));
    }

    #[test]
    fn missed_times_within_the_window_fire_once_and_older_ones_are_recorded() {
        let clock = Arc::new(AtomicU64::new(0));
        let (engines, sink, store) = engines(1, &clock);
        let mut t = conformance::trigger("default", "hourly", true);
        t.spec.when = When::Cron {
            expr: "0 * * * *".into(),
            timezone: "UTC".into(),
        };
        t.spec.policy.catch_up_seconds = 2 * 3600;
        t.next_due_ms = Some(60 * MINUTE);
        assert!(store.create(&t).unwrap());
        // Down from 00:30 to 10:20: 01:00..10:00 passed. 09:00 and 10:00
        // are within two hours and fire once, as 10:00; 01:00..08:00 are
        // missed.
        clock.store(10 * 60 * MINUTE + 20 * MINUTE, Ordering::SeqCst);
        let tick = engines[0].tick().unwrap();
        assert_eq!(tick.fired.len(), 1);
        assert_eq!(tick.fired[0].scheduled_ms, Some(10 * 60 * MINUTE));
        let runs = store.runs(&t.id, 10).unwrap();
        let missed = runs.iter().find(|r| r.state == RunState::Missed).unwrap();
        assert_eq!(missed.missed, Some(8));
        assert_eq!(missed.scheduled_ms, Some(60 * MINUTE));
        assert_eq!(missed.last_missed_ms, Some(8 * 60 * MINUTE));
        assert_eq!(
            store.get(&t.id).unwrap().unwrap().next_due_ms,
            Some(11 * 60 * MINUTE)
        );
        assert_eq!(sink.admitted.lock().unwrap().len(), 1);

        // A window of 0 is a minute: a tick a little late still fires.
        let mut t2 = conformance::trigger("default", "short", true);
        t2.spec.policy.catch_up_seconds = 0;
        t2.next_due_ms = Some(19 * 60 * MINUTE);
        assert!(store.create(&t2).unwrap());
        clock.store(20 * 60 * MINUTE + 30_000, Ordering::SeqCst);
        let tick = engines[0].tick().unwrap();
        let mut fired: Vec<(String, Option<u64>)> = tick
            .fired
            .iter()
            .map(|r| (r.trigger.clone(), r.scheduled_ms))
            .collect();
        fired.sort();
        let mut expected = vec![
            (t.id.clone(), Some(20 * 60 * MINUTE)),
            (t2.id.clone(), Some(20 * 60 * MINUTE)),
        ];
        expected.sort();
        assert_eq!(fired, expected);
        let missed_of = |id: &str| {
            store
                .runs(id, 10)
                .unwrap()
                .into_iter()
                .filter(|r| r.state == RunState::Missed)
                .map(|r| r.missed.unwrap())
                .collect::<Vec<_>>()
        };
        // Every minute from 19:00 to 19:59; 11:00..18:00 of the hourly one
        // (and the 01:00..08:00 before).
        assert_eq!(missed_of(&t2.id), [60]);
        assert_eq!(missed_of(&t.id), [8, 8]);
    }

    #[test]
    fn slots_split_into_missed_and_the_one_that_fires() {
        let every = Schedule::Every(10);
        assert_eq!(
            slots(&every, 100, 155, 125),
            Slots {
                latest: Some(150),
                missed: 3,
                first_missed: Some(100),
                last_missed: Some(120)
            }
        );
        assert_eq!(slots(&every, 100, 100, 50).missed, 0);
        assert_eq!(slots(&every, 100, 150, 151).last_missed, Some(150));
        let cron =
            Schedule::Cron(crate::triggers::cron::Cron::parse("*/10 * * * *", "UTC").unwrap());
        let m = 60_000;
        assert_eq!(
            slots(&cron, 0, 35 * m, 25 * m),
            Slots {
                latest: Some(30 * m),
                missed: 3,
                first_missed: Some(0),
                last_missed: Some(20 * m)
            }
        );
    }

    #[test]
    fn a_dispatcher_that_died_mid_fire_is_taken_over_without_a_second_task() {
        let clock = Arc::new(AtomicU64::new(10 * MINUTE));
        let (engines, sink, store) = engines(2, &clock);
        let t = scheduled(&store, When::Interval { seconds: 3600 }, 10 * MINUTE);
        // The first claims the time and admits, then dies before recording.
        let claimed = engines[0].claim_due().unwrap();
        assert_eq!(claimed.len(), 1);
        let trigger = store.get(&t.id).unwrap().unwrap();
        let task = Engine::render(&trigger, &claimed[0].0).unwrap();
        sink.admit(&trigger, &claimed[0].0.key, task).unwrap();
        // Another dispatcher sees nothing until the claim expires...
        assert!(engines[1].tick().unwrap().fired.is_empty());
        clock.store(10 * MINUTE + RUN_LEASE_MS + 1, Ordering::SeqCst);
        // ...then fires it again, and admission's key makes it one task.
        let tick = engines[1].tick().unwrap();
        assert_eq!(tick.fired.len(), 1);
        assert_eq!(tick.fired[0].state, RunState::Fired);
        assert_eq!(sink.admitted.lock().unwrap().len(), 1);
        // The first one's late finish is refused by the fence.
        let mut late = claimed[0].0.clone();
        late.state = RunState::Failed;
        assert!(!store.finish_run(&late, claimed[0].1, 3).unwrap().written);
    }

    #[test]
    fn failures_pause_the_trigger_and_successes_settle() {
        let clock = Arc::new(AtomicU64::new(MINUTE));
        let (engines, sink, store) = engines(1, &clock);
        let mut t = conformance::trigger("default", "on-issue", false);
        t.spec.task.prompt = "Fix #{{event.number}}: {{event.title}}".into();
        assert!(store.create(&t).unwrap());
        let event = |n: u32| TriggerEvent {
            source: "github".into(),
            kind: "issues.opened".into(),
            id: format!("d-{n}"),
            number: Some(n.to_string()),
            title: Some(format!("Bug {n}")),
            ..TriggerEvent::default()
        };
        let deliver = |n: u32| {
            let mut run = new_run(&t.id, format!("event:d-{n}"), RunState::Pending, MINUTE);
            run.event = Some(event(n));
            store.record(&run).unwrap();
            engines[0].tick().unwrap()
        };
        let fired = deliver(1).fired;
        assert_eq!(fired[0].state, RunState::Fired);
        let op_id = fired[0].operation.clone().unwrap();
        let task = sink.admitted.lock().unwrap()[&op_id].clone();
        assert_eq!(task.prompt, "Fix #1: Bug 1");
        assert!(task.name.unwrap().starts_with("on-issue-1-"));
        // Its task fails: one failure.
        sink.ops.lock().unwrap().insert(
            op_id.clone(),
            Operation {
                id: op_id.clone(),
                repo: "app".into(),
                kind: branchyard_client::api::OperationKind::Task,
                state: OperationState::Failed,
                branches: vec![],
                cursor: 0,
                created_at_ms: 0,
                finished_at_ms: Some(1),
                end_cursor: None,
                result: None,
                error: None,
                requires: vec![],
                priority: 0,
                waiting: None,
            },
        );
        assert_eq!(engines[0].tick().unwrap().settled, 1);
        assert_eq!(store.get(&t.id).unwrap().unwrap().failures, 1);
        // Two refused admissions: the third failure in a row pauses it.
        *sink.refuse.lock().unwrap() = Some("quota_exceeded: max_running".into());
        assert_eq!(deliver(2).fired[0].state, RunState::Failed);
        assert!(store.get(&t.id).unwrap().unwrap().enabled);
        deliver(3);
        let got = store.get(&t.id).unwrap().unwrap();
        assert!(!got.enabled);
        assert!(got
            .paused_reason
            .unwrap()
            .contains("quota_exceeded: max_running"));
        // A delivery recorded for a paused trigger is skipped, not fired.
        *sink.refuse.lock().unwrap() = None;
        assert_eq!(deliver(4).fired[0].state, RunState::SkippedDisabled);

        // A server that is stopping gives a run back rather than failing it.
        store.set_state(&t.id, true, None, 0, None).unwrap();
        *sink.refuse.lock().unwrap() = Some("later: shutting_down".into());
        let mut run = new_run(&t.id, "event:d-5".into(), RunState::Pending, MINUTE);
        run.event = Some(event(5));
        store.record(&run).unwrap();
        let (claimed, fence) = engines[0].claim_pending().unwrap().unwrap();
        let given_back = engines[0].fire(claimed, fence).unwrap();
        assert_eq!(given_back.state, RunState::Pending);
        assert_eq!(store.get(&t.id).unwrap().unwrap().failures, 0);
        *sink.refuse.lock().unwrap() = None;
        assert!(engines[0].tick().unwrap().fired.is_empty(), "not at once");
        clock.store(MINUTE + DEFER_MS + 1, Ordering::SeqCst);
        let tick = engines[0].tick().unwrap();
        assert_eq!(tick.fired.len(), 1, "claimable after the deferral");
        assert_eq!(tick.fired[0].state, RunState::Fired);
    }
}
