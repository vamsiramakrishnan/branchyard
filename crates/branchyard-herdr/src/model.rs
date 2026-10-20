//! What Herdr is told about each branch: the state mapping and the
//! per-branch tracking that feeds it. Pure; the bridge does the I/O.

use std::collections::BTreeMap;

use branchyard::{Activity, BranchInfo, BranchStatus, Event, ReportedState};

/// Herdr's agent states, as `herdr pane report-agent --state` takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HerdrState {
    Working,
    Blocked,
    Idle,
}

impl HerdrState {
    pub fn as_str(self) -> &'static str {
        match self {
            HerdrState::Working => "working",
            HerdrState::Blocked => "blocked",
            HerdrState::Idle => "idle",
        }
    }
}

/// One report: a state and the message shown with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub state: HerdrState,
    pub message: Option<String>,
}

/// The report for a branch in `status`, with `pending` the tool of a
/// permission request still waiting for an answer, if any, and `said` what
/// the branch's harness last reported about itself this turn
/// (`report_status`).
///
/// `running` is `working`, or `blocked` while a permission request waits
/// (only a local `by run --ask` on a served repository leaves one waiting)
/// or while the harness says it is blocked; every settled status is `idle`,
/// with what happened as the message. The harness's own summary is the
/// message of a running branch, and a claim never moves a branch out of
/// `working` except to `blocked`, nor outranks a settled status. A stale
/// claim is not passed here: [`Branches::expire`] forgets one whose
/// time-to-live has passed, and a new turn or status forgets it too.
pub fn report(status: &BranchStatus, pending: Option<&str>, said: Option<&Said>) -> Report {
    let (state, message) = match status {
        BranchStatus::Running => match (pending, said) {
            (Some(tool), _) => (
                HerdrState::Blocked,
                Some(format!("waiting for permission: {tool}")),
            ),
            (None, Some(said)) => (
                match said.state {
                    ReportedState::Blocked => HerdrState::Blocked,
                    _ => HerdrState::Working,
                },
                Some(said.message()),
            ),
            (None, None) => (HerdrState::Working, None),
        },
        BranchStatus::Ready => (HerdrState::Idle, Some("ready to merge".to_owned())),
        BranchStatus::NoChanges => (HerdrState::Idle, Some("no changes".to_owned())),
        BranchStatus::Merged { target, .. } => {
            (HerdrState::Idle, Some(format!("merged into {target}")))
        }
        BranchStatus::Failed { reason } => (HerdrState::Idle, Some(format!("failed: {reason}"))),
        BranchStatus::Interrupted => (HerdrState::Idle, Some("interrupted".to_owned())),
        BranchStatus::BudgetExceeded { limit } => {
            (HerdrState::Idle, Some(format!("budget exceeded: {limit}")))
        }
        BranchStatus::Waiting => (
            HerdrState::Idle,
            Some("waiting for its prerequisites".to_owned()),
        ),
        BranchStatus::Blocked { reason } => (HerdrState::Idle, Some(format!("blocked: {reason}"))),
        BranchStatus::Discarded { reason } => {
            (HerdrState::Idle, Some(format!("discarded: {reason}")))
        }
        BranchStatus::AwaitingPlanApproval => (
            HerdrState::Blocked,
            Some("its plan awaits approval".to_owned()),
        ),
        BranchStatus::WaitingOnChildren => (
            HerdrState::Idle,
            Some("waiting on its children; woken when they settle".to_owned()),
        ),
    };
    Report {
        state,
        message: message.map(|m| one_line(&m, 160)),
    }
}

/// What a branch's harness last reported about itself (`report_status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Said {
    /// The state it claimed.
    pub state: ReportedState,
    /// Its one-line summary.
    pub summary: String,
    /// When it goes stale, from the report's time-to-live: milliseconds
    /// since the Unix epoch. None when it gave none.
    pub expires_at_ms: Option<u64>,
}

impl Said {
    /// The summary, led by the state unless it is `working`.
    fn message(&self) -> String {
        match self.state {
            ReportedState::Working => self.summary.clone(),
            state => format!("{state}: {}", self.summary),
        }
    }
}

/// `text` on one line, whitespace runs collapsed, at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// What the bridge knows of one branch.
#[derive(Clone, Debug, PartialEq)]
pub struct Tracked {
    pub status: BranchStatus,
    /// The tool of a permission request with no decision yet.
    pub pending: Option<String>,
    /// The harness's own latest report this turn, if it made one.
    pub said: Option<Said>,
}

impl Tracked {
    pub fn report(&self) -> Report {
        report(&self.status, self.pending.as_deref(), self.said.as_ref())
    }
}

/// Every branch the bridge has seen, by name.
#[derive(Debug, Default)]
pub struct Branches {
    pub branches: BTreeMap<String, Tracked>,
}

impl Branches {
    /// Replace what is known with a listing. Returns every listed name.
    pub fn snapshot(&mut self, listed: &[BranchInfo]) -> Vec<String> {
        self.branches.clear();
        for info in listed {
            self.branches.insert(
                info.name.clone(),
                Tracked {
                    status: info.status.clone(),
                    pending: None,
                    said: None,
                },
            );
        }
        listed.iter().map(|info| info.name.clone()).collect()
    }

    /// Forget every claim whose time-to-live has passed at `now_ms`, so
    /// a pane is no longer marked blocked on a stale `blocked`. Returns
    /// the branches whose report may have changed.
    pub fn expire(&mut self, now_ms: u64) -> Vec<String> {
        let mut expired = Vec::new();
        for (name, tracked) in &mut self.branches {
            if tracked
                .said
                .as_ref()
                .and_then(|s| s.expires_at_ms)
                .is_some_and(|at| now_ms >= at)
            {
                tracked.said = None;
                expired.push(name.clone());
            }
        }
        expired
    }

    /// When the next claim goes stale, if any has a time-to-live.
    pub fn next_expiry(&self) -> Option<u64> {
        self.branches
            .values()
            .filter_map(|t| t.said.as_ref().and_then(|s| s.expires_at_ms))
            .min()
    }

    /// Apply one feed entry, recorded at `at_ms`. Returns whether the
    /// branch's report may have changed.
    pub fn apply(&mut self, branch: &str, at_ms: u64, activity: &Activity) -> bool {
        // A branch first seen through the feed started after the listing,
        // so it is running until its status says otherwise.
        let tracked = self
            .branches
            .entry(branch.to_owned())
            .or_insert_with(|| Tracked {
                status: BranchStatus::Running,
                pending: None,
                said: None,
            });
        match activity {
            Activity::Status(status) => {
                tracked.status = status.clone();
                tracked.pending = None;
                tracked.said = None;
            }
            Activity::Prompt(_) => {
                tracked.status = BranchStatus::Running;
                tracked.pending = None;
                tracked.said = None;
            }
            Activity::StatusReport {
                state,
                summary,
                ttl_seconds,
                ..
            } => {
                tracked.said = Some(Said {
                    state: *state,
                    summary: summary.clone(),
                    expires_at_ms: ttl_seconds
                        .map(|s| at_ms.saturating_add(s.saturating_mul(1000))),
                });
            }
            Activity::Harness(Event::PermissionRequested { request, .. }) => {
                tracked.pending = Some(request.tool.clone());
            }
            Activity::Decision { .. }
            | Activity::Harness(Event::PermissionWithdrawn { .. })
            | Activity::Harness(Event::TurnEnded { .. }) => tracked.pending = None,
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{DecisionSource, PermissionKey, PermissionRequest};
    use serde_json::json;

    fn state(status: BranchStatus) -> (&'static str, Option<String>) {
        let r = report(&status, None, None);
        (r.state.as_str(), r.message)
    }

    #[test]
    fn statuses_map_to_herdr_states() {
        assert_eq!(state(BranchStatus::Running), ("working", None));
        assert_eq!(
            state(BranchStatus::Ready),
            ("idle", Some("ready to merge".into()))
        );
        assert_eq!(
            state(BranchStatus::NoChanges),
            ("idle", Some("no changes".into()))
        );
        assert_eq!(
            state(BranchStatus::Merged {
                target: "main".into(),
                commit: "abc".into()
            }),
            ("idle", Some("merged into main".into()))
        );
        assert_eq!(
            state(BranchStatus::Failed {
                reason: "the harness exited\nwith status 3".into()
            }),
            (
                "idle",
                Some("failed: the harness exited with status 3".into())
            )
        );
        assert_eq!(
            state(BranchStatus::Interrupted),
            ("idle", Some("interrupted".into()))
        );
        assert_eq!(
            state(BranchStatus::BudgetExceeded {
                limit: "max_turns 1".into()
            }),
            ("idle", Some("budget exceeded: max_turns 1".into()))
        );
    }

    #[test]
    fn a_waiting_permission_request_blocks_a_running_branch() {
        let r = report(&BranchStatus::Running, Some("write marker"), None);
        assert_eq!(r.state, HerdrState::Blocked);
        assert_eq!(
            r.message.as_deref(),
            Some("waiting for permission: write marker")
        );
        // A settled branch has nothing to answer.
        assert_eq!(
            report(&BranchStatus::Ready, Some("x"), None).state,
            HerdrState::Idle
        );
    }

    #[test]
    fn long_messages_are_cut() {
        let long = "x".repeat(500);
        let r = report(&BranchStatus::Failed { reason: long }, None, None);
        assert_eq!(r.message.unwrap().chars().count(), 161);
        assert_eq!(one_line("  a \n\t b ", 10), "a b");
    }

    #[test]
    fn feed_activity_moves_a_branch_through_its_states() {
        let mut branches = Branches::default();
        let request = PermissionRequest {
            key: PermissionKey("k".into()),
            tool: "shell".into(),
            input: json!({}),
        };
        let now = |b: &Branches| b.branches["b"].report().state;

        assert!(branches.apply("b", 0, &Activity::Prompt("go".into())));
        assert_eq!(now(&branches), HerdrState::Working);
        assert!(branches.apply(
            "b",
            0,
            &Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request,
            })
        ));
        assert_eq!(now(&branches), HerdrState::Blocked);
        assert!(branches.apply(
            "b",
            0,
            &Activity::Decision {
                tool: "shell".into(),
                allowed: true,
                message: None,
                source: DecisionSource::Asked,
            }
        ));
        assert_eq!(now(&branches), HerdrState::Working);
        assert!(!branches.apply("b", 0, &Activity::Warning("w".into())));
        assert!(branches.apply("b", 0, &Activity::Status(BranchStatus::Ready)));
        assert_eq!(now(&branches), HerdrState::Idle);
        // A branch a trigger keeps alive going idle or dormant is still
        // idle to herdr: nothing to report.
        for state in [
            branchyard::LifecycleState::Idle,
            branchyard::LifecycleState::Dormant,
            branchyard::LifecycleState::Reincarnated,
        ] {
            let lifecycle = Activity::Lifecycle {
                state,
                reason: "r".into(),
                idle_since_ms: 1,
            };
            assert!(!branches.apply("b", 0, &lifecycle));
        }
        assert_eq!(now(&branches), HerdrState::Idle);
    }

    fn said(state: ReportedState, summary: &str) -> Activity {
        Activity::StatusReport {
            state,
            summary: summary.into(),
            link: None,
            ttl_seconds: None,
        }
    }

    /// A `blocked` claim with a time-to-live marks the pane blocked until
    /// it passes, then the branch is plain working again.
    #[test]
    fn a_stale_blocked_claim_stops_blocking_the_pane() {
        let mut branches = Branches::default();
        let now = |b: &Branches| {
            let r = b.branches["b"].report();
            (r.state, r.message)
        };
        assert!(branches.apply("b", 0, &Activity::Prompt("go".into())));
        assert_eq!(branches.next_expiry(), None);
        let timed = Activity::StatusReport {
            state: ReportedState::Blocked,
            summary: "waiting on CI".into(),
            link: None,
            ttl_seconds: Some(60),
        };
        assert!(branches.apply("b", 10_000, &timed));
        assert_eq!(branches.next_expiry(), Some(70_000));
        assert!(branches.expire(69_999).is_empty());
        assert_eq!(
            now(&branches),
            (HerdrState::Blocked, Some("blocked: waiting on CI".into()))
        );
        assert_eq!(branches.expire(70_000), ["b"]);
        assert_eq!(now(&branches), (HerdrState::Working, None));
        assert_eq!(branches.next_expiry(), None);
        assert!(branches.expire(u64::MAX).is_empty(), "once");
        // Without a ttl, nothing expires it.
        assert!(branches.apply("b", 0, &said(ReportedState::Blocked, "stuck")));
        assert!(branches.expire(u64::MAX).is_empty());
        assert_eq!(now(&branches).0, HerdrState::Blocked);
    }

    /// What a harness says of itself is the message of a running branch,
    /// moves it to `blocked` only when it says so, never outranks a settled
    /// status, and is forgotten when the next turn starts.
    #[test]
    fn a_running_branch_shows_what_its_harness_says_of_itself() {
        let mut branches = Branches::default();
        let now = |b: &Branches| {
            let r = b.branches["b"].report();
            (r.state, r.message)
        };

        assert!(branches.apply("b", 0, &Activity::Prompt("go".into())));
        assert!(branches.apply("b", 0, &said(ReportedState::Working, "reading the test")));
        assert_eq!(
            now(&branches),
            (HerdrState::Working, Some("reading the test".into()))
        );
        // A permission request still outranks the claim.
        let request = PermissionRequest {
            key: PermissionKey("k".into()),
            tool: "shell".into(),
            input: json!({}),
        };
        assert!(branches.apply(
            "b",
            0,
            &Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request,
            })
        ));
        assert_eq!(now(&branches).0, HerdrState::Blocked);
        assert_eq!(
            now(&branches).1.as_deref(),
            Some("waiting for permission: shell")
        );
        assert!(!branches.apply("b", 0, &Activity::Warning("w".into())));
        assert!(branches.apply("b", 0, &said(ReportedState::Blocked, "needs a key")));
        assert_eq!(
            now(&branches),
            (
                HerdrState::Blocked,
                Some("waiting for permission: shell".into())
            )
        );
        assert!(branches.apply(
            "b",
            0,
            &Activity::Decision {
                tool: "shell".into(),
                allowed: true,
                message: None,
                source: DecisionSource::Asked,
            }
        ));
        assert_eq!(
            now(&branches),
            (HerdrState::Blocked, Some("blocked: needs a key".into()))
        );
        // Any other claim leaves a running branch working.
        for state in [
            ReportedState::Idle,
            ReportedState::Complete,
            ReportedState::Failure,
        ] {
            assert!(branches.apply("b", 0, &said(state, "x")));
            assert_eq!(
                now(&branches),
                (HerdrState::Working, Some(format!("{state}: x")))
            );
        }
        // A settled status is the protocol's word, whatever was claimed.
        assert!(branches.apply("b", 0, &said(ReportedState::Working, "still going")));
        assert!(branches.apply("b", 0, &Activity::Status(BranchStatus::Ready)));
        assert_eq!(
            now(&branches),
            (HerdrState::Idle, Some("ready to merge".into()))
        );
        // The next turn starts without it.
        assert!(branches.apply("b", 0, &said(ReportedState::Working, "again")));
        assert!(branches.apply("b", 0, &Activity::Prompt("more".into())));
        assert_eq!(now(&branches), (HerdrState::Working, None));
    }

    #[test]
    fn a_branch_first_seen_in_the_feed_is_running() {
        let mut branches = Branches::default();
        assert!(!branches.apply("new", 0, &Activity::Warning("w".into())));
        assert_eq!(branches.branches["new"].status, BranchStatus::Running);
    }
}
