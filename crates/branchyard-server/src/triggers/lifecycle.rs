//! The lifecycle of the branches a trigger keeps alive (`deliver.lifecycle`,
//! `docs/triggers.md#lifecycle`): park one that has rested a while, mark it
//! dormant after longer so its next run starts a fresh session, and delete
//! it after longest.
//!
//! The decision is a pure function of the clock and the branch, so it is
//! tested without one ([`decide`]). [`Engine::rest`] reads the branches,
//! asks [`decide`] and has the server apply the answer; it runs on the
//! dispatcher's ticks, at most every [`EVERY_MS`].
//!
//! All three thresholds count from the end of the branch's last turn, in
//! absolute time: a branch idle for `dormant_days` is dormant, whether or
//! not it was seen idle before, and one idle for `delete_days` is deleted.
//! A run that continues the branch ends its rest.

use std::io;
use std::sync::atomic::Ordering;

use branchyard_client::triggers::{Lifecycle, RunState, TriggerRun};

use super::engine::Engine;
use super::StoredTrigger;

/// How often the dispatcher looks at the branches of triggers with a
/// lifecycle: thresholds are minutes and days, so a look a few times a
/// minute is plenty, and each look reads the trigger's runs.
pub const EVERY_MS: u64 = 30_000;

const MINUTE: u64 = 60;
const DAY: u64 = 86_400;

/// The thresholds of a [`Lifecycle`] in seconds, which is what the
/// decision compares: absolute time, so minutes and days can be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Thresholds {
    /// Park a branch idle this long.
    pub idle: Option<u64>,
    /// Mark it dormant after this long.
    pub dormant: Option<u64>,
    /// Delete it after this long.
    pub delete: Option<u64>,
}

impl Thresholds {
    /// The thresholds `lifecycle` sets, in seconds; no check.
    pub fn of(lifecycle: &Lifecycle) -> Thresholds {
        Thresholds {
            idle: lifecycle.idle_minutes.map(|m| u64::from(m) * MINUTE),
            dormant: lifecycle.dormant_days.map(|d| u64::from(d) * DAY),
            delete: lifecycle.delete_days.map(|d| u64::from(d) * DAY),
        }
    }
}

/// `deliver.lifecycle`, held to the rules: at least one threshold, each at
/// least 1, and those given rise (idle, dormant, delete), compared in
/// absolute time. The error names the field.
pub fn validate(lifecycle: &Lifecycle) -> Result<Thresholds, String> {
    for (field, value) in [
        ("idle_minutes", lifecycle.idle_minutes),
        ("dormant_days", lifecycle.dormant_days),
        ("delete_days", lifecycle.delete_days),
    ] {
        if value == Some(0) {
            return Err(format!("deliver.lifecycle.{field} is at least 1"));
        }
    }
    if *lifecycle == Lifecycle::default() {
        return Err(
            "deliver.lifecycle sets none of idle_minutes, dormant_days and delete_days".into(),
        );
    }
    let at = Thresholds::of(lifecycle);
    let rising = [
        ("idle_minutes", at.idle),
        ("dormant_days", at.dormant),
        ("delete_days", at.delete),
    ];
    let mut before: Option<(&str, u64)> = None;
    for (field, value) in rising {
        let Some(value) = value else { continue };
        if let Some((earlier, seconds)) = before {
            if seconds >= value {
                return Err(format!(
                    "deliver.lifecycle.{earlier} ({}) must be less than {field} ({}): a branch \
                     goes idle, then dormant, then is deleted",
                    span(seconds),
                    span(value)
                ));
            }
        }
        before = Some((field, value));
    }
    Ok(at)
}

/// Where a branch is in its lifecycle, as the decision sees it: nothing
/// has been done to it since its last turn ended, it was parked idle, or it
/// was marked dormant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Its last turn ended after anything was done to it.
    Active,
    /// Parked: what it held was released.
    Idle,
    /// Marked dormant: its next run starts a fresh session.
    Dormant,
}

impl Stage {
    /// The stage a branch's recorded lifecycle state means.
    pub fn of(state: Option<branchyard::LifecycleState>) -> Stage {
        match state {
            Some(branchyard::LifecycleState::Idle) => Stage::Idle,
            Some(branchyard::LifecycleState::Dormant) => Stage::Dormant,
            _ => Stage::Active,
        }
    }
}

/// What to do to a branch now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Park it: release what it holds and record that it is idle.
    Park,
    /// Mark it dormant, so its next run starts its session over.
    GoDormant,
    /// Remove it.
    Delete,
}

/// What, if anything, to do to a branch at `now_ms`, which last ended a
/// turn at `last_turn_end_ms`, is `running` a turn (or owed one), and is at
/// `stage`. The furthest threshold reached wins: a branch left alone long
/// enough (a server that was down) goes straight to dormant or deleted. An
/// action is returned once: the stage it leaves the branch in is the one it
/// is not returned for again.
pub fn decide(
    now_ms: u64,
    last_turn_end_ms: u64,
    running: bool,
    stage: Stage,
    thresholds: &Thresholds,
) -> Option<Action> {
    if running {
        return None;
    }
    let idle_for = now_ms.saturating_sub(last_turn_end_ms);
    let reached = |limit: Option<u64>| limit.is_some_and(|s| idle_for / 1000 >= s);
    if reached(thresholds.delete) {
        return Some(Action::Delete);
    }
    if reached(thresholds.dormant) && stage != Stage::Dormant {
        return Some(Action::GoDormant);
    }
    if reached(thresholds.idle) && stage == Stage::Active {
        return Some(Action::Park);
    }
    None
}

/// A span for a reason or an error: the largest whole unit, `31m`, `2h`,
/// `7d`.
pub fn span(seconds: u64) -> String {
    match seconds {
        s if s >= DAY => format!("{}d", s / DAY),
        s if s >= 3600 => format!("{}h", s / 3600),
        s if s >= MINUTE => format!("{}m", s / MINUTE),
        s => format!("{s}s"),
    }
}

/// What a branch looks like to the decision: what the sink's `standing`
/// reports for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    /// A turn runs on it or is owed to it.
    pub running: bool,
    /// When it last ended a turn, in milliseconds since the Unix epoch.
    pub last_turn_end_ms: u64,
    /// Where it is in its lifecycle.
    pub stage: Stage,
}

impl Engine {
    /// Look at the branches `trigger` delivered to and act on those that
    /// have rested long enough. Returns the actions taken, as
    /// `(branch, action)`. Does nothing for a trigger without a lifecycle.
    pub fn rest_one(
        &self,
        trigger: &StoredTrigger,
        now_ms: u64,
    ) -> io::Result<Vec<(String, Action)>> {
        let Some(lifecycle) = trigger
            .spec
            .deliver
            .as_ref()
            .and_then(|d| d.lifecycle.as_ref())
        else {
            return Ok(Vec::new());
        };
        let thresholds = Thresholds::of(lifecycle);
        let mut taken = Vec::new();
        for branch in self.store.delivered_to(&trigger.id)? {
            let standing = match self.sink.standing(trigger, &branch) {
                Ok(Some(standing)) => standing,
                // Gone, settled for good, or not ours to look at.
                Ok(None) => continue,
                Err(why) => {
                    branchyard_support::best_effort_once(
                        &format!("looking at {branch} for trigger {}", trigger.spec.name),
                        Err::<(), _>(why),
                    );
                    continue;
                }
            };
            let Some(action) = decide(
                now_ms,
                standing.last_turn_end_ms,
                standing.running,
                standing.stage,
                &thresholds,
            ) else {
                continue;
            };
            let idle = span(now_ms.saturating_sub(standing.last_turn_end_ms) / 1000);
            let reason = match action {
                Action::Park => format!("no turn ended for {idle}, so it is idle"),
                Action::GoDormant => format!("no turn ended for {idle}, so it is dormant"),
                Action::Delete => format!("no turn ended for {idle}, so it was deleted"),
            };
            match self.sink.rest(trigger, &branch, action, &reason) {
                Ok(()) => {
                    if action == Action::Delete {
                        self.note_deleted(trigger, &branch, &reason, now_ms)?;
                    }
                    taken.push((branch, action));
                }
                // A turn started since the look, or the branch went: the
                // next look decides again.
                Err(why) => tracing::info!(
                    trigger = %trigger.spec.name,
                    branch = %branch,
                    action = ?action,
                    reason = %why,
                    "lifecycle step not taken"
                ),
            }
        }
        Ok(taken)
    }

    /// Record on the trigger that its `branch` was deleted: the branch's
    /// log goes with it, the trigger's runs do not.
    fn note_deleted(
        &self,
        trigger: &StoredTrigger,
        branch: &str,
        reason: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        let mut run = TriggerRun {
            id: super::new_id("run"),
            trigger: trigger.id.clone(),
            key: format!("lifecycle:deleted:{branch}:{now_ms}"),
            state: RunState::Lifecycle,
            at_ms: now_ms,
            scheduled_ms: None,
            missed: None,
            last_missed_ms: None,
            event: None,
            reason: Some(format!("{branch}: {reason}")),
            precheck: None,
            operation: None,
            branches: vec![branch.to_owned()],
            outcome: None,
            finished_at_ms: None,
        };
        run.finished_at_ms = Some(now_ms);
        self.store.record(&run).map(|_| ())
    }

    /// The lifecycle of every enabled trigger with one, on the repositories
    /// served here, if [`EVERY_MS`] has passed since the last look.
    /// Returns what was done, as `(trigger, branch, action)`.
    pub fn rest(&self) -> io::Result<Vec<(String, String, Action)>> {
        let now = self.clock.now();
        let last = self.rested_at.load(Ordering::SeqCst);
        // A clock set back is no reason to wait for it to catch up.
        if last != 0 && now >= last && now - last < EVERY_MS {
            return Ok(Vec::new());
        }
        // One look at a time per engine; a loser skips this tick.
        if self
            .rested_at
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(Vec::new());
        }
        let repos = self.sink.repos();
        let mut done = Vec::new();
        for trigger in self.store.list(None)? {
            let served = repos.iter().any(|(name, _)| *name == trigger.spec.repo);
            if !trigger.enabled || !served {
                continue;
            }
            for (branch, action) in self.rest_one(&trigger, now)? {
                done.push((trigger.spec.name.clone(), branch, action));
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;
    const DAY_MS: u64 = 86_400_000;

    fn all() -> Thresholds {
        Thresholds {
            idle: Some(30 * 60),
            dormant: Some(7 * DAY),
            delete: Some(30 * DAY),
        }
    }

    fn at(idle_for_ms: u64, running: bool, stage: Stage, t: &Thresholds) -> Option<Action> {
        // The branch ended its turn at 1,000,000 ms.
        decide(1_000_000 + idle_for_ms, 1_000_000, running, stage, t)
    }

    #[test]
    fn a_branch_is_left_alone_until_a_threshold_is_reached() {
        let t = all();
        assert_eq!(at(0, false, Stage::Active, &t), None);
        assert_eq!(at(29 * MIN, false, Stage::Active, &t), None);
        // Exactly at the threshold counts.
        assert_eq!(at(30 * MIN, false, Stage::Active, &t), Some(Action::Park));
        assert_eq!(at(30 * MIN - 1, false, Stage::Active, &t), None);
        assert_eq!(at(6 * DAY_MS, false, Stage::Idle, &t), None);
    }

    #[test]
    fn each_stage_is_acted_on_once_and_the_furthest_threshold_wins() {
        let t = all();
        // Parked: not parked again, but dormant when its time comes.
        assert_eq!(at(31 * MIN, false, Stage::Idle, &t), None);
        assert_eq!(
            at(7 * DAY_MS, false, Stage::Idle, &t),
            Some(Action::GoDormant)
        );
        // Dormant: nothing until the delete threshold.
        assert_eq!(at(7 * DAY_MS, false, Stage::Dormant, &t), None);
        assert_eq!(at(29 * DAY_MS, false, Stage::Dormant, &t), None);
        assert_eq!(
            at(30 * DAY_MS, false, Stage::Dormant, &t),
            Some(Action::Delete)
        );
        // A server that was down: straight to the furthest.
        assert_eq!(
            at(8 * DAY_MS, false, Stage::Active, &t),
            Some(Action::GoDormant)
        );
        assert_eq!(
            at(31 * DAY_MS, false, Stage::Active, &t),
            Some(Action::Delete)
        );
        assert_eq!(
            at(31 * DAY_MS, false, Stage::Idle, &t),
            Some(Action::Delete)
        );
    }

    #[test]
    fn a_branch_running_a_turn_is_never_touched() {
        let t = all();
        for stage in [Stage::Active, Stage::Idle, Stage::Dormant] {
            assert_eq!(at(365 * DAY_MS, true, stage, &t), None);
        }
    }

    #[test]
    fn only_the_thresholds_that_are_set_count() {
        let idle_only = Thresholds {
            idle: Some(60),
            ..Thresholds::default()
        };
        assert_eq!(
            at(2 * MIN, false, Stage::Active, &idle_only),
            Some(Action::Park)
        );
        assert_eq!(at(400 * DAY_MS, false, Stage::Idle, &idle_only), None);
        let dormant_only = Thresholds {
            dormant: Some(DAY),
            ..Thresholds::default()
        };
        assert_eq!(at(2 * MIN, false, Stage::Active, &dormant_only), None);
        assert_eq!(
            at(DAY_MS, false, Stage::Active, &dormant_only),
            Some(Action::GoDormant)
        );
        // Deleting needs no dormancy first.
        let delete_only = Thresholds {
            delete: Some(2 * DAY),
            ..Thresholds::default()
        };
        assert_eq!(
            at(2 * DAY_MS, false, Stage::Active, &delete_only),
            Some(Action::Delete)
        );
        assert_eq!(
            at(DAY_MS, false, Stage::Active, &Thresholds::default()),
            None
        );
    }

    #[test]
    fn a_clock_behind_the_log_is_no_time_at_all() {
        // The turn "ended" after now: a host clock set back, or a log
        // written by a host ahead of this one.
        let t = all();
        assert_eq!(decide(500, 1_000_000, false, Stage::Active, &t), None);
    }

    #[test]
    fn the_stage_follows_the_recorded_state() {
        use branchyard::LifecycleState as S;
        assert_eq!(Stage::of(None), Stage::Active);
        assert_eq!(Stage::of(Some(S::Idle)), Stage::Idle);
        assert_eq!(Stage::of(Some(S::Dormant)), Stage::Dormant);
        // A reincarnation or a deletion leaves nothing to resume.
        assert_eq!(Stage::of(Some(S::Reincarnated)), Stage::Active);
        assert_eq!(Stage::of(Some(S::Deleted)), Stage::Active);
    }

    fn lifecycle(idle: Option<u32>, dormant: Option<u32>, delete: Option<u32>) -> Lifecycle {
        Lifecycle {
            idle_minutes: idle,
            dormant_days: dormant,
            delete_days: delete,
        }
    }

    #[test]
    fn thresholds_are_held_to_the_rules_and_name_the_field() {
        let ok = validate(&lifecycle(Some(30), Some(7), Some(30))).unwrap();
        assert_eq!(ok, all());
        validate(&lifecycle(None, None, Some(1))).unwrap();
        validate(&lifecycle(Some(1), None, None)).unwrap();
        // A minute count is compared with days in absolute time.
        validate(&lifecycle(Some(1439), Some(1), None)).unwrap();
        for (given, field) in [
            (lifecycle(Some(0), None, None), "idle_minutes"),
            (lifecycle(None, Some(0), None), "dormant_days"),
            (lifecycle(None, None, Some(0)), "delete_days"),
        ] {
            let e = validate(&given).unwrap_err();
            assert_eq!(e, format!("deliver.lifecycle.{field} is at least 1"));
        }
        let e = validate(&Lifecycle::default()).unwrap_err();
        assert!(e.starts_with("deliver.lifecycle sets none of"), "{e}");
        // Idle, dormant and delete must rise, in absolute time.
        let e = validate(&lifecycle(Some(1440), Some(1), None)).unwrap_err();
        assert!(
            e.contains("deliver.lifecycle.idle_minutes (1d) must be less than dormant_days (1d)"),
            "{e}"
        );
        let e = validate(&lifecycle(Some(2 * 1440), Some(1), None)).unwrap_err();
        assert!(e.contains("idle_minutes (2d) must be less than dormant_days (1d)"));
        let e = validate(&lifecycle(None, Some(7), Some(7))).unwrap_err();
        assert!(e.contains("dormant_days (7d) must be less than delete_days (7d)"));
        let e = validate(&lifecycle(Some(30), None, Some(7))).unwrap();
        assert_eq!(e.delete, Some(7 * DAY));
        let e = validate(&lifecycle(Some(10_000), None, Some(6))).unwrap_err();
        assert!(e.contains("idle_minutes (6d) must be less than delete_days (6d)"));
    }

    #[test]
    fn spans_read_in_the_largest_whole_unit() {
        assert_eq!(span(59), "59s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(31 * 60 + 5), "31m");
        assert_eq!(span(3600), "1h");
        assert_eq!(span(86_400), "1d");
        assert_eq!(span(7 * 86_400 + 3600), "7d");
    }
}
