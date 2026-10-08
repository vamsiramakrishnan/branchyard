//! What Herdr is told about each branch: the state mapping and the
//! per-branch tracking that feeds it. Pure; the bridge does the I/O.

use std::collections::BTreeMap;

use branchyard::{Activity, BranchInfo, BranchStatus, Event};

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
/// permission request still waiting for an answer, if any.
///
/// `running` is `working`, or `blocked` while a permission request waits
/// (only a local `by run --ask` on a served repository leaves one waiting);
/// every settled status is `idle`, with what happened as the message.
pub fn report(status: &BranchStatus, pending: Option<&str>) -> Report {
    let (state, message) = match status {
        BranchStatus::Running => match pending {
            Some(tool) => (
                HerdrState::Blocked,
                Some(format!("waiting for permission: {tool}")),
            ),
            None => (HerdrState::Working, None),
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
    };
    Report {
        state,
        message: message.map(|m| one_line(&m, 160)),
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
}

impl Tracked {
    pub fn report(&self) -> Report {
        report(&self.status, self.pending.as_deref())
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
                },
            );
        }
        listed.iter().map(|info| info.name.clone()).collect()
    }

    /// Apply one feed entry. Returns whether the branch's report may have
    /// changed.
    pub fn apply(&mut self, branch: &str, activity: &Activity) -> bool {
        // A branch first seen through the feed started after the listing,
        // so it is running until its status says otherwise.
        let tracked = self
            .branches
            .entry(branch.to_owned())
            .or_insert_with(|| Tracked {
                status: BranchStatus::Running,
                pending: None,
            });
        match activity {
            Activity::Status(status) => {
                tracked.status = status.clone();
                tracked.pending = None;
            }
            Activity::Prompt(_) => {
                tracked.status = BranchStatus::Running;
                tracked.pending = None;
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
        let r = report(&status, None);
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
        let r = report(&BranchStatus::Running, Some("write marker"));
        assert_eq!(r.state, HerdrState::Blocked);
        assert_eq!(
            r.message.as_deref(),
            Some("waiting for permission: write marker")
        );
        // A settled branch has nothing to answer.
        assert_eq!(
            report(&BranchStatus::Ready, Some("x")).state,
            HerdrState::Idle
        );
    }

    #[test]
    fn long_messages_are_cut() {
        let long = "x".repeat(500);
        let r = report(&BranchStatus::Failed { reason: long }, None);
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

        assert!(branches.apply("b", &Activity::Prompt("go".into())));
        assert_eq!(now(&branches), HerdrState::Working);
        assert!(branches.apply(
            "b",
            &Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request,
            })
        ));
        assert_eq!(now(&branches), HerdrState::Blocked);
        assert!(branches.apply(
            "b",
            &Activity::Decision {
                tool: "shell".into(),
                allowed: true,
                message: None,
                source: DecisionSource::Asked,
            }
        ));
        assert_eq!(now(&branches), HerdrState::Working);
        assert!(!branches.apply("b", &Activity::Warning("w".into())));
        assert!(branches.apply("b", &Activity::Status(BranchStatus::Ready)));
        assert_eq!(now(&branches), HerdrState::Idle);
    }

    #[test]
    fn a_branch_first_seen_in_the_feed_is_running() {
        let mut branches = Branches::default();
        assert!(!branches.apply("new", &Activity::Warning("w".into())));
        assert_eq!(branches.branches["new"].status, BranchStatus::Running);
    }
}
