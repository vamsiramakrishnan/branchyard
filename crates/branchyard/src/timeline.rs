//! The shape of a branch's work over time, without its content.
//!
//! A client that draws many branches at once (the console's board) needs
//! when each turn began and ended, when each tool call started and what it
//! was called, what each turn cost and when the branch merged. Reading that
//! from [`Branch::events`](crate::Branch::events) meant reading every
//! prompt, message, tool input and provisioning record, and a page of
//! recent events dropped the oldest turns of a long branch. [`Timeline::of`]
//! reduces the whole log to that shape once, on the side that holds it.
//!
//! The console's trajectory page draws one branch's turns as a waterfall
//! from the same read, so a turn also carries what it needs and that holds
//! no content: how it started ([`TimelineTurn::startup`]), its tokens, and
//! the [`TimelineMark`]s that fall inside it (a person's steer, a permission
//! asked and answered, a stall, a delegation, a judge's score). Each names
//! the event it came from by its number in the branch's log, so a client
//! that wants the content reads that one event page.
//!
//! It is the second of four levels a client reads a branch at, each asked
//! for when it is needed: [`BranchInfo`](crate::BranchInfo) (status,
//! counts, cost), this timeline, a page of events (the conversation), and
//! the diff or the inspection. See `docs/server.md`.

use serde::{Deserialize, Serialize};

use crate::startup::Startup;
use crate::{Activity, BranchStatus, Event, RecordedEvent};

/// A branch's turns and merge, as [`Timeline::of`] reads them.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    /// Every turn, oldest first.
    pub turns: Vec<TimelineTurn>,
    /// When the branch merged, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at_ms: Option<u64>,
    /// The running token total the harness last reported for the current
    /// session, when it reports one ([`branchyard_harness::Usage::cumulative`]).
    /// A reader that folds later usage onto this timeline subtracts it from
    /// the next running total, so the tokens already counted are not
    /// counted again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_tokens: Option<u64>,
}

/// One turn: from its prompt to the status that ended it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineTurn {
    /// The number, from 1, of the event that started the turn in the
    /// branch's log: where `GET …/event-page?cursor=` reads its prompt.
    #[serde(default)]
    pub seq: u64,
    /// When the prompt was submitted, or when the first tool call started
    /// for a turn whose prompt is not in the log.
    pub started_at_ms: u64,
    /// When a status other than running or waiting ended it; absent while
    /// it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    /// The status that ended it, such as `ready`, or `failed` with its
    /// reason; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BranchStatus>,
    /// What the turn cost on its own, when something measured it
    /// ([`Activity::TurnCost`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Its tool calls, in the order they started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<TimelineTool>,
    /// How long the turn took to start, phase by phase, and the provider
    /// and path that made its sandbox ready ([`Activity::Startup`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<Startup>,
    /// The input and output tokens the turn used, when the harness
    /// reported any: a running total counts only its growth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// What else happened in the turn, in the order it was recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<TimelineMark>,
}

/// A tool call's start.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTool {
    /// When it started.
    pub at_ms: u64,
    /// The tool's name, as the harness reported it.
    pub name: String,
    /// The harness's ID for the call. With [`TimelineTool::at_ms`] it
    /// names the call, so a client that keeps a timeline current from the
    /// event stream can tell a call it already has from a new one.
    pub call_id: String,
    /// The event's number, from 1, in the branch's log.
    #[serde(default)]
    pub seq: u64,
}

/// What a [`TimelineMark`] marks.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkKind {
    /// A person's input written into the running turn
    /// ([`Activity::Steered`]).
    Steer,
    /// The harness refused or dropped steered input.
    SteerRejected,
    /// The harness asked for permission to use the tool in `label`.
    Permission,
    /// The permission request just before it was answered: `ok` says
    /// whether it was allowed.
    Decision,
    /// No harness activity for the stall window ([`Activity::Stalled`]).
    Stall,
    /// Activity after a stall ([`Activity::Resumed`]).
    Resumed,
    /// A delegation the branch asked for: `label` is the child's name, `ok`
    /// is false when it was refused.
    Delegation,
    /// A judge scored the branch: `score` is from 0 to 100.
    Judged,
    /// The engine noticed something, or the harness warned.
    Warning,
}

/// A point in a turn that is not a tool call; see [`MarkKind`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimelineMark {
    /// When it was recorded.
    pub at_ms: u64,
    /// The event's number, from 1, in the branch's log.
    #[serde(default)]
    pub seq: u64,
    /// What it is.
    pub kind: MarkKind,
    /// The tool a permission was about, or the child a delegation named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether a decision allowed the call, or a delegation was carried out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// A judge's score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

impl TimelineMark {
    fn new(at_ms: u64, seq: u64, kind: MarkKind) -> TimelineMark {
        TimelineMark {
            at_ms,
            seq,
            kind,
            label: None,
            ok: None,
            score: None,
        }
    }
}

impl Timeline {
    /// Read the timeline from a branch's whole event log, oldest first.
    ///
    /// A prompt starts a turn; steered input into a running turn does not,
    /// and steered input after the turn ended starts one. A tool call with
    /// no turn open starts one at the call. A status other than running or
    /// waiting ends the open turn, and `merged` is also the merge time.
    pub fn of(events: &[RecordedEvent]) -> Timeline {
        let mut timeline = Timeline::default();
        let mut open = false;
        // The growth of a running total within a session.
        let mut session_high = 0u64;
        // A startup recorded before any turn is the first turn's.
        let mut early_startup = None;
        for (index, event) in events.iter().enumerate() {
            let at = event.at_ms;
            let seq = index as u64 + 1;
            let mark = |timeline: &mut Timeline, mark: TimelineMark| {
                if let Some(turn) = timeline.turns.last_mut() {
                    turn.marks.push(mark);
                }
            };
            match &event.activity {
                Activity::Prompt(_) => {
                    timeline
                        .turns
                        .push(TimelineTurn::starting(at, seq, early_startup.take()));
                    open = true;
                }
                Activity::Steered { .. } => {
                    if !open {
                        timeline
                            .turns
                            .push(TimelineTurn::starting(at, seq, early_startup.take()));
                        open = true;
                    }
                    mark(&mut timeline, TimelineMark::new(at, seq, MarkKind::Steer));
                }
                Activity::Harness(Event::ToolStarted { name, call_id, .. }) => {
                    if !open {
                        timeline
                            .turns
                            .push(TimelineTurn::starting(at, seq, early_startup.take()));
                        open = true;
                    }
                    if let Some(turn) = timeline.turns.last_mut() {
                        turn.tools.push(TimelineTool {
                            at_ms: at,
                            name: name.clone(),
                            call_id: call_id.clone(),
                            seq,
                        });
                    }
                }
                Activity::Harness(Event::PermissionRequested { request, .. }) => {
                    let mut m = TimelineMark::new(at, seq, MarkKind::Permission);
                    m.label = Some(request.tool.clone());
                    mark(&mut timeline, m);
                }
                Activity::Decision { tool, allowed, .. } => {
                    let mut m = TimelineMark::new(at, seq, MarkKind::Decision);
                    m.label = Some(tool.clone());
                    m.ok = Some(*allowed);
                    mark(&mut timeline, m);
                }
                Activity::Harness(Event::SteerRejected { .. }) => {
                    let m = TimelineMark::new(at, seq, MarkKind::SteerRejected);
                    mark(&mut timeline, m);
                }
                Activity::Harness(Event::Warning { .. }) | Activity::Warning(_) => {
                    let m = TimelineMark::new(at, seq, MarkKind::Warning);
                    mark(&mut timeline, m);
                }
                Activity::Harness(Event::SessionStarted { .. }) => session_high = 0,
                Activity::Harness(Event::UsageObserved { usage, .. }) => {
                    if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                        let n = usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0);
                        let grown = if usage.cumulative {
                            let grown = n.saturating_sub(session_high);
                            session_high = session_high.max(n);
                            grown
                        } else {
                            n
                        };
                        if let Some(turn) = timeline.turns.last_mut() {
                            turn.tokens = Some(turn.tokens.unwrap_or(0) + grown);
                        }
                    }
                }
                Activity::Stalled { .. } => {
                    let m = TimelineMark::new(at, seq, MarkKind::Stall);
                    mark(&mut timeline, m);
                }
                Activity::Resumed => {
                    let m = TimelineMark::new(at, seq, MarkKind::Resumed);
                    mark(&mut timeline, m);
                }
                Activity::Delegation {
                    branch, refused, ..
                } => {
                    let mut m = TimelineMark::new(at, seq, MarkKind::Delegation);
                    m.label = Some(branch.clone()).filter(|b| !b.is_empty());
                    m.ok = Some(!refused);
                    mark(&mut timeline, m);
                }
                Activity::Fleet(fleet) => {
                    if let crate::fleet::FleetActivity::Judged(judged) = &**fleet {
                        let mut m = TimelineMark::new(at, seq, MarkKind::Judged);
                        m.score = Some(judged.score);
                        mark(&mut timeline, m);
                    }
                }
                Activity::Startup(startup) => match timeline.turns.last_mut() {
                    Some(turn) => turn.startup = Some((**startup).clone()),
                    None => early_startup = Some((**startup).clone()),
                },
                Activity::TurnCost { cost_usd } => {
                    if let Some(turn) = timeline.turns.last_mut() {
                        turn.cost_usd = Some(*cost_usd);
                    }
                }
                Activity::Status(status) => {
                    if matches!(status, BranchStatus::Merged { .. }) {
                        timeline.merged_at_ms = Some(at);
                    }
                    if open && !matches!(status, BranchStatus::Running | BranchStatus::Waiting) {
                        if let Some(turn) = timeline.turns.last_mut() {
                            turn.ended_at_ms = Some(at);
                            turn.outcome = Some(status.clone());
                        }
                        open = false;
                    }
                }
                _ => {}
            }
        }
        timeline.session_tokens = (session_high > 0).then_some(session_high);
        timeline
    }
}

impl TimelineTurn {
    fn starting(at_ms: u64, seq: u64, startup: Option<Startup>) -> TimelineTurn {
        TimelineTurn {
            seq,
            started_at_ms: at_ms,
            ended_at_ms: None,
            outcome: None,
            cost_usd: None,
            tools: Vec::new(),
            startup,
            tokens: None,
            marks: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn tool(turn: u64, name: &str) -> Activity {
        Activity::Harness(Event::ToolStarted {
            turn,
            call_id: format!("{name}-{turn}"),
            name: name.into(),
            target: None,
        })
    }

    #[test]
    fn each_prompt_is_a_turn_with_its_calls_cost_and_end() {
        let timeline = Timeline::of(&[
            at(10, Activity::Status(BranchStatus::Running)),
            at(11, Activity::Prompt("fix it".into())),
            at(12, tool(1, "Read")),
            at(13, tool(1, "Edit")),
            at(14, Activity::TurnCost { cost_usd: 0.25 }),
            at(15, Activity::Status(BranchStatus::Ready)),
            at(20, Activity::Prompt("and the docs".into())),
            at(21, Activity::Status(BranchStatus::Running)),
        ]);
        assert_eq!(timeline.turns.len(), 2);
        let first = &timeline.turns[0];
        assert_eq!((first.started_at_ms, first.ended_at_ms), (11, Some(15)));
        assert_eq!(first.outcome, Some(BranchStatus::Ready));
        assert_eq!(first.cost_usd, Some(0.25));
        let names: Vec<_> = first
            .tools
            .iter()
            .map(|t| (t.at_ms, t.name.as_str()))
            .collect();
        assert_eq!(names, [(12, "Read"), (13, "Edit")]);
        assert_eq!(first.tools[0].call_id, "Read-1");
        let second = &timeline.turns[1];
        assert_eq!(
            (second.started_at_ms, second.ended_at_ms),
            (20, None),
            "still running"
        );
        assert_eq!(timeline.merged_at_ms, None);
    }

    #[test]
    fn steering_a_running_turn_is_not_a_new_turn_but_steering_after_it_is() {
        let steer = |id| Activity::Steered {
            id,
            by: "person".into(),
            text: "also this".into(),
        };
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(2, steer(1)),
            at(3, Activity::Status(BranchStatus::Ready)),
            at(4, steer(2)),
            at(5, Activity::Status(BranchStatus::Ready)),
        ]);
        let spans: Vec<_> = timeline
            .turns
            .iter()
            .map(|t| (t.started_at_ms, t.ended_at_ms))
            .collect();
        assert_eq!(spans, [(1, Some(3)), (4, Some(5))]);
    }

    #[test]
    fn a_tool_call_without_its_prompt_opens_a_turn_and_merging_is_recorded() {
        let timeline = Timeline::of(&[
            at(5, tool(1, "Bash")),
            at(6, Activity::Status(BranchStatus::Ready)),
            at(
                9,
                Activity::Status(BranchStatus::Merged {
                    target: "main".into(),
                    commit: "abc".into(),
                }),
            ),
        ]);
        assert_eq!(timeline.turns.len(), 1);
        assert_eq!(timeline.turns[0].started_at_ms, 5);
        assert_eq!(timeline.turns[0].tools.len(), 1);
        assert_eq!(timeline.merged_at_ms, Some(9));
        assert_eq!(
            timeline.turns[0].ended_at_ms,
            Some(6),
            "the merge does not move a closed turn"
        );
    }

    #[test]
    fn a_failed_turn_ends_with_its_outcome_and_no_cost() {
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(
                2,
                Activity::Status(BranchStatus::Failed {
                    reason: "boom".into(),
                }),
            ),
        ]);
        assert_eq!(
            timeline.turns[0].outcome,
            Some(BranchStatus::Failed {
                reason: "boom".into()
            })
        );
        assert_eq!(timeline.turns[0].cost_usd, None);
    }

    #[test]
    fn the_wire_leaves_out_what_is_absent() {
        let timeline = Timeline::of(&[at(1, Activity::Prompt("go".into()))]);
        let wire = serde_json::to_value(&timeline).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"turns": [{"seq": 1, "started_at_ms": 1}]})
        );
        assert_eq!(serde_json::from_value::<Timeline>(wire).unwrap(), timeline);
    }

    #[test]
    fn the_timeline_carries_the_session_running_total_a_reader_folds_onto() {
        use branchyard_harness::Usage;
        let total = |input| {
            Activity::Harness(Event::UsageObserved {
                turn: Some(1),
                usage: Usage {
                    cumulative: true,
                    input_tokens: Some(input),
                    output_tokens: Some(0),
                    ..Usage::default()
                },
            })
        };
        let events = [at(1, Activity::Prompt("go".into())), at(2, total(100))];
        let timeline = Timeline::of(&events);
        assert_eq!(timeline.turns[0].tokens, Some(100));
        // A reader that later sees a running total of 110 adds the 10 it grew
        // by, not 110, because the timeline says where the session stood.
        assert_eq!(timeline.session_tokens, Some(100));
        let wire = serde_json::to_value(&timeline).unwrap();
        assert_eq!(wire["session_tokens"], 100);
        // A new session starts its running total again.
        let restarted = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(2, total(100)),
            at(
                3,
                Activity::Harness(Event::SessionStarted {
                    session: branchyard_harness::NativeSession::new("s2").unwrap(),
                    forked_from: None,
                }),
            ),
        ]);
        assert_eq!(restarted.session_tokens, None);
    }

    #[test]
    fn a_turn_carries_its_startup_tokens_and_marks_but_no_content() {
        use crate::startup::{Phases, StartupPath};
        use branchyard_harness::{PermissionKey, PermissionRequest, Usage};
        let usage = |cumulative, input, output| {
            Activity::Harness(Event::UsageObserved {
                turn: Some(1),
                usage: Usage {
                    cumulative,
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    ..Usage::default()
                },
            })
        };
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("secret prompt".into())),
            at(
                2,
                Activity::Startup(Box::new(Startup {
                    provider: "local".into(),
                    path: StartupPath::Cold,
                    phases: Phases {
                        queued: Some(0),
                        first_event: Some(40),
                        ..Phases::default()
                    },
                })),
            ),
            at(
                3,
                Activity::Harness(Event::PermissionRequested {
                    turn: Some(1),
                    request: PermissionRequest {
                        key: PermissionKey("k".into()),
                        tool: "write".into(),
                        input: serde_json::json!({"path": "secret.txt"}),
                    },
                }),
            ),
            at(
                4,
                Activity::Decision {
                    tool: "write".into(),
                    allowed: false,
                    message: Some("no".into()),
                    source: crate::DecisionSource::Asked,
                },
            ),
            at(
                5,
                Activity::Steered {
                    id: 1,
                    by: "person".into(),
                    text: "secret steer".into(),
                },
            ),
            at(6, usage(true, 10, 5)),
            at(7, usage(true, 30, 10)),
            at(
                8,
                Activity::Delegation {
                    tool: "spawn".into(),
                    branch: "kid".into(),
                    outcome: "started".into(),
                    refused: false,
                },
            ),
            at(
                9,
                Activity::Delegation {
                    tool: "spawn".into(),
                    branch: String::new(),
                    outcome: "too deep".into(),
                    refused: true,
                },
            ),
            at(10, Activity::Stalled { since_ms: 7 }),
            at(11, Activity::Resumed),
            at(12, Activity::Status(BranchStatus::Ready)),
        ]);
        let turn = &timeline.turns[0];
        assert_eq!(turn.seq, 1);
        assert_eq!(
            turn.startup.as_ref().map(|s| s.phases.total()),
            Some(Some(40))
        );
        assert_eq!(turn.tokens, Some(40), "a running total counts its growth");
        let marks: Vec<_> = turn
            .marks
            .iter()
            .map(|m| (m.seq, m.kind, m.label.as_deref(), m.ok))
            .collect();
        assert_eq!(
            marks,
            [
                (3, MarkKind::Permission, Some("write"), None),
                (4, MarkKind::Decision, Some("write"), Some(false)),
                (5, MarkKind::Steer, None, None),
                (8, MarkKind::Delegation, Some("kid"), Some(true)),
                (9, MarkKind::Delegation, None, Some(false)),
                (10, MarkKind::Stall, None, None),
                (11, MarkKind::Resumed, None, None),
            ]
        );
        let wire = serde_json::to_string(&timeline).unwrap();
        for content in ["secret prompt", "secret.txt", "secret steer", "too deep"] {
            assert!(!wire.contains(content), "{content} is content: {wire}");
        }
    }

    #[test]
    fn a_judge_scores_the_last_turn_and_tool_calls_name_their_events() {
        use crate::fleet::{FleetActivity, JudgeMark};
        let timeline = Timeline::of(&[
            at(1, Activity::Prompt("go".into())),
            at(2, tool(1, "Read")),
            at(3, Activity::Status(BranchStatus::Ready)),
            at(
                4,
                Activity::Fleet(Box::new(FleetActivity::Judged(JudgeMark {
                    score: 87.5,
                    rank: 1,
                    of: 2,
                    picked: true,
                    by: "deterministic".into(),
                    reason: None,
                    check: "passed".into(),
                }))),
            ),
        ]);
        let turn = &timeline.turns[0];
        assert_eq!(turn.tools[0].seq, 2);
        assert_eq!(turn.marks.len(), 1);
        assert_eq!(turn.marks[0].kind, MarkKind::Judged);
        assert_eq!(turn.marks[0].score, Some(87.5));
    }

    #[test]
    fn a_startup_before_any_turn_is_the_first_turns() {
        use crate::startup::{Phases, StartupPath};
        let startup = Startup {
            provider: "local".into(),
            path: StartupPath::Cold,
            phases: Phases::default(),
        };
        let timeline = Timeline::of(&[
            at(1, Activity::Startup(Box::new(startup.clone()))),
            at(2, Activity::Prompt("go".into())),
        ]);
        assert_eq!(timeline.turns[0].startup, Some(startup));
    }

    #[test]
    fn an_empty_log_is_an_empty_timeline() {
        assert_eq!(Timeline::of(&[]), Timeline::default());
    }
}
