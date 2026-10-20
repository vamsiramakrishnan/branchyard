//! A status a branch's harness states about itself, beside the one the
//! protocol gives.
//!
//! A branch's [`BranchStatus`](crate::BranchStatus) comes from the harness
//! protocol and the engine: running, ready, failed. A harness can also say
//! what it is doing in its own words, with the `report_status` tool (or
//! `by report-status`): a [state](ReportedState), a one-line summary, an
//! optional link and an optional time-to-live. The claim is recorded on the branch as
//! [`Activity::StatusReport`] and shown beside the protocol's status in
//! `by log`, `by watch`, `by show` and [`Inspection::reported`]; it never
//! changes that status, and nothing relies on it being true. The one thing
//! it triggers is attention: a `blocked` or `failure` report is escalated to
//! the branch's parent's inbox, as `escalate` would.
//!
//! A report goes stale, judged when it is read and never stored: when the
//! branch ends after it, when a later turn finishes without a new one, or
//! when its time-to-live passes ([`judge`]). A stale report is still shown,
//! marked so, and no longer stands for what the branch is doing.
//!
//! [`Inspection::reported`]: crate::Inspection::reported

use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use crate::{Activity, BranchStatus, Error, Message, RecordedEvent};

/// The longest summary, in characters.
pub const SUMMARY_MAX: usize = 280;

/// The longest link, in bytes.
pub const LINK_MAX: usize = 2048;

/// What a harness says it is doing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::EnumIter,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReportedState {
    /// At work on the task.
    Working,
    /// Not working on anything, and not stuck.
    Idle,
    /// Cannot go on without something it names: an answer, a credential, a
    /// decision. Escalated to the parent.
    Blocked,
    /// Believes the task is done.
    Complete,
    /// Gave up, or cannot do the task. Escalated to the parent.
    Failure,
}

impl ReportedState {
    /// Every state's name, in the order the tool lists them.
    pub const NAMES: [&'static str; 5] = ["working", "idle", "blocked", "complete", "failure"];

    /// Whether the state asks for someone's attention: `blocked` and
    /// `failure` are escalated to the parent's inbox and noticed by a
    /// person watching.
    pub fn needs_attention(self) -> bool {
        matches!(self, ReportedState::Blocked | ReportedState::Failure)
    }

    /// Whether the summary says what the branch is doing now, so a
    /// dashboard shows it in place of the last tool: `working` and
    /// `blocked`.
    pub fn is_doing(self) -> bool {
        matches!(self, ReportedState::Working | ReportedState::Blocked)
    }

    /// The state `text` names, or a refusal that lists the ones there are.
    pub fn parse(text: &str) -> Result<ReportedState, Error> {
        parse_state(text).map_err(Error::Denied)
    }
}

/// [`ReportedState::parse`], with the refusal as its message.
fn parse_state(text: &str) -> Result<ReportedState, String> {
    ReportedState::from_str(text.trim()).map_err(|_| {
        format!(
            "state must be one of {}, not {text:?}",
            ReportedState::NAMES.join(", ")
        )
    })
}

/// The shortest time-to-live a report may carry, in seconds.
pub const TTL_MIN_SECONDS: u64 = 1;

/// The longest time-to-live a report may carry, in seconds: seven days.
pub const TTL_MAX_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Why a report no longer says what the branch is doing. Judged when the
/// report is read, from the branch's events and the time; see
/// [`judge`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StaleBecause {
    /// The branch ended since: a `failed`, `merged` or `discarded` status
    /// was recorded after the report.
    Ended,
    /// A later turn of the branch started and finished without a new
    /// report.
    LaterTurn,
    /// Its `ttl_seconds` have passed.
    Expired,
}

/// A branch's own report of its status, as `report_status` returns it and
/// `Inspection::reported` shows the latest.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// What the harness says it is doing.
    pub state: ReportedState,
    /// One line of at most 280 characters.
    pub summary: String,
    /// Where to read more, an `http` or `https` URL. Omitted when none was
    /// given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Milliseconds since the Unix epoch when the report was recorded.
    pub at_ms: u64,
    /// How long the report holds, in seconds, as the harness gave it.
    /// Omitted when none was given: the report then holds until a later
    /// turn finishes or the branch ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// When the report expires, `at_ms` plus `ttl_seconds`. Omitted without
    /// a `ttl_seconds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Whether the report no longer says what the branch is doing, judged
    /// when it was read. Omitted while it is current.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Why it is stale. Omitted while it is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_because: Option<StaleBecause>,
}

impl StatusReport {
    /// A current report as recorded at `at_ms`, its expiry worked out from
    /// `ttl_seconds`.
    pub fn new(
        state: ReportedState,
        summary: String,
        link: Option<String>,
        ttl_seconds: Option<u64>,
        at_ms: u64,
    ) -> StatusReport {
        StatusReport {
            state,
            summary,
            link,
            at_ms,
            ttl_seconds,
            expires_at_ms: ttl_seconds.map(|s| at_ms.saturating_add(s.saturating_mul(1000))),
            stale: false,
            stale_because: None,
        }
    }

    /// The report `event` records, current, if it is one.
    pub fn of(event: &RecordedEvent) -> Option<StatusReport> {
        match &event.activity {
            Activity::StatusReport {
                state,
                summary,
                link,
                ttl_seconds,
            } => Some(StatusReport::new(
                *state,
                summary.clone(),
                link.clone(),
                *ttl_seconds,
                event.at_ms,
            )),
            _ => None,
        }
    }

    /// One line: `status blocked: waiting on the API key (https://...)`.
    pub fn describe(&self) -> String {
        describe(self.state, &self.summary, self.link.as_deref())
    }

    /// Whether its time-to-live has passed at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }

    /// Mark it stale for `because`, unless it is stale already.
    pub fn mark_stale(&mut self, because: StaleBecause) {
        if !self.stale {
            self.stale = true;
            self.stale_because = Some(because);
        }
    }

    /// The report as it stands at `now_ms`: marked [`StaleBecause::Expired`]
    /// when its time-to-live has passed and nothing else made it stale.
    pub fn at(mut self, now_ms: u64) -> StatusReport {
        if self.expired(now_ms) {
            self.mark_stale(StaleBecause::Expired);
        }
        self
    }

    /// Whether a dashboard may show its summary as what the branch is
    /// doing now: it is current and its state is `working` or `blocked`.
    pub fn is_doing(&self) -> bool {
        !self.stale && self.state.is_doing()
    }
}

/// What `report_status` returns: the report as recorded, and the message to
/// the parent it raised when the state asks for attention.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    /// The report, as recorded on the branch.
    pub report: StatusReport,
    /// The escalation sent to the parent's inbox for a `blocked` or
    /// `failure` report. Omitted for the other states, for a branch with no
    /// parent, and for a report that repeats the one before it exactly (the
    /// parent was told then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Message>,
}

/// `state`, `summary` and `link` as one line, whitespace collapsed:
/// `status working: reading the failing test`, with the link in
/// parentheses when there is one.
pub fn describe(state: ReportedState, summary: &str, link: Option<&str>) -> String {
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    match link {
        Some(link) => format!("status {state}: {summary} ({link})"),
        None => format!("status {state}: {summary}"),
    }
}

/// A report field the engine refused, and why: what a surface that
/// answers with the field (an HTTP `400` with `detail.field`) needs where
/// the tool's own refusal, [`Error::Denied`], carries only the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The field that broke a rule: `state`, `summary`, `link` or
    /// `ttl_seconds`.
    pub field: &'static str,
    /// The rule, in the words [`Error::Denied`] carries for the tool.
    pub message: String,
}

impl Refusal {
    fn new(field: &'static str, message: String) -> Refusal {
        Refusal { field, message }
    }
}

/// A report's summary and link as the engine keeps them, or why it refuses
/// them: a summary that is blank or longer than [`SUMMARY_MAX`] characters,
/// a link that is not an `http` or `https` URL of at most [`LINK_MAX`]
/// bytes. A blank link counts as none.
pub fn check(summary: &str, link: Option<&str>) -> Result<(String, Option<String>), Error> {
    check_fields(summary, link).map_err(|refusal| Error::Denied(refusal.message))
}

/// [`check`], naming the field it refused.
pub fn check_fields(
    summary: &str,
    link: Option<&str>,
) -> Result<(String, Option<String>), Refusal> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(Refusal::new(
            "summary",
            "a status report needs a summary".into(),
        ));
    }
    let length = summary.chars().count();
    if length > SUMMARY_MAX {
        return Err(Refusal::new(
            "summary",
            format!(
                "the summary is {length} characters; keep it to one line of at most \
                 {SUMMARY_MAX}"
            ),
        ));
    }
    let link = match link.map(str::trim).filter(|l| !l.is_empty()) {
        None => None,
        Some(link) if link.len() > LINK_MAX => {
            return Err(Refusal::new(
                "link",
                format!("the link is {} bytes; at most {LINK_MAX}", link.len()),
            ))
        }
        Some(link) => match url::Url::parse(link) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") && parsed.has_host() => {
                Some(link.to_owned())
            }
            _ => {
                return Err(Refusal::new(
                    "link",
                    format!("the link must be an http or https URL, not {link:?}"),
                ))
            }
        },
    };
    Ok((summary.to_owned(), link))
}

/// A report's time-to-live as the engine keeps it, in whole seconds, or
/// why it refuses it: a number that is not a whole number of seconds from
/// [`TTL_MIN_SECONDS`] to [`TTL_MAX_SECONDS`]. None means the report has
/// no time-to-live.
pub fn check_ttl(seconds: Option<f64>) -> Result<Option<u64>, Error> {
    check_ttl_field(seconds).map_err(|refusal| Error::Denied(refusal.message))
}

/// [`check_ttl`], naming the field (`ttl_seconds`) it refused.
pub fn check_ttl_field(seconds: Option<f64>) -> Result<Option<u64>, Refusal> {
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    let range = TTL_MIN_SECONDS as f64..=TTL_MAX_SECONDS as f64;
    if !seconds.is_finite() || seconds.fract() != 0.0 || !range.contains(&seconds) {
        return Err(Refusal::new(
            "ttl_seconds",
            format!(
                "ttl_seconds must be a whole number of seconds from {TTL_MIN_SECONDS} to \
                 {TTL_MAX_SECONDS} (7 days), not {seconds}"
            ),
        ));
    }
    Ok(Some(seconds as u64))
}

/// A whole report as the `report_status` tool takes it, checked: the
/// state, the summary and link as [`check`] keeps them, and the
/// time-to-live as [`check_ttl`] does. The first field to break a rule is
/// named in the [`Refusal`], in the order `state`, `summary`, `link`,
/// `ttl_seconds`.
pub fn validate(
    state: &str,
    summary: &str,
    link: Option<&str>,
    ttl_seconds: Option<f64>,
) -> Result<Checked, Refusal> {
    let state = parse_state(state).map_err(|message| Refusal::new("state", message))?;
    let (summary, link) = check_fields(summary, link)?;
    let ttl_seconds = check_ttl_field(ttl_seconds)?;
    Ok(Checked {
        state,
        summary,
        link,
        ttl_seconds,
    })
}

/// A report's fields as [`validate`] keeps them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The state.
    pub state: ReportedState,
    /// The summary, trimmed.
    pub summary: String,
    /// The link, or none for a blank one.
    pub link: Option<String>,
    /// The time-to-live in seconds, if one was given.
    pub ttl_seconds: Option<u64>,
}

/// The latest self-reported status in `events`, oldest first, if the
/// branch ever reported one, judged stale or current at `now_ms` by
/// [`judge`].
pub fn latest(events: &[RecordedEvent], now_ms: u64) -> Option<StatusReport> {
    let at = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::StatusReport { .. }))?;
    let report = StatusReport::of(&events[at])?;
    Some(judge(report, &events[at + 1..], now_ms))
}

/// Whether `report` still says what the branch is doing, given the events
/// recorded `after` it and the time `now_ms`. It is stale, in this order
/// of reasons, when:
///
/// 1. [`StaleBecause::Ended`]: a `failed`, `merged` or `discarded` status
///    was recorded after it;
/// 2. [`StaleBecause::LaterTurn`]: a prompt was recorded after it (a
///    later turn started), and after that prompt a status other than
///    `running` or `waiting` (that turn finished);
/// 3. [`StaleBecause::Expired`]: it has a time-to-live and `now_ms` is at
///    or past `expires_at_ms`.
///
/// A turn still running after the report leaves it current until that
/// turn finishes. Nothing is stored: the judgement is made each time a
/// report is read.
pub fn judge(mut report: StatusReport, after: &[RecordedEvent], now_ms: u64) -> StatusReport {
    let ended = after.iter().any(|e| {
        matches!(
            e.activity,
            Activity::Status(
                BranchStatus::Failed { .. }
                    | BranchStatus::Merged { .. }
                    | BranchStatus::Discarded { .. }
            )
        )
    });
    let later_turn = after
        .iter()
        .position(|e| matches!(e.activity, Activity::Prompt(_)))
        .is_some_and(|start| {
            after[start..].iter().any(|e| {
                matches!(&e.activity, Activity::Status(s)
                    if !matches!(s, BranchStatus::Running | BranchStatus::Waiting))
            })
        });
    if ended {
        report.mark_stale(StaleBecause::Ended);
    } else if later_turn {
        report.mark_stale(StaleBecause::LaterTurn);
    }
    report.at(now_ms)
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::*;

    fn event(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn report(at_ms: u64, state: ReportedState, summary: &str) -> RecordedEvent {
        event(
            at_ms,
            Activity::StatusReport {
                state,
                summary: summary.into(),
                link: None,
                ttl_seconds: None,
            },
        )
    }

    #[test]
    fn the_names_are_the_states_in_the_order_the_tool_lists_them() {
        let states: Vec<&str> = ReportedState::iter().map(<&str>::from).collect();
        assert_eq!(states, ReportedState::NAMES);
        for name in ReportedState::NAMES {
            let state = ReportedState::parse(name).unwrap();
            assert_eq!(state.to_string(), name);
            assert_eq!(serde_json::to_value(state).unwrap(), name);
        }
    }

    #[test]
    fn a_bad_state_is_refused_naming_the_good_ones() {
        let Err(Error::Denied(message)) = ReportedState::parse("done") else {
            panic!("done is not a state");
        };
        assert_eq!(
            message,
            "state must be one of working, idle, blocked, complete, failure, not \"done\""
        );
        assert!(ReportedState::parse("Working").is_err(), "case matters");
        assert_eq!(ReportedState::parse(" idle ").unwrap(), ReportedState::Idle);
    }

    #[test]
    fn only_blocked_and_failure_ask_for_attention() {
        let attention: Vec<_> = ReportedState::iter()
            .filter(|s| s.needs_attention())
            .collect();
        assert_eq!(attention, [ReportedState::Blocked, ReportedState::Failure]);
        let doing: Vec<_> = ReportedState::iter().filter(|s| s.is_doing()).collect();
        assert_eq!(doing, [ReportedState::Working, ReportedState::Blocked]);
    }

    #[test]
    fn a_summary_is_trimmed_and_limited_in_characters() {
        let (summary, link) = check("  reading the failing test\n", None).unwrap();
        assert_eq!(summary, "reading the failing test");
        assert_eq!(link, None);
        // 280 characters of three bytes each are within the limit.
        assert!(check(&"é".repeat(SUMMARY_MAX), None).is_ok());
        let Err(Error::Denied(message)) = check(&"x".repeat(SUMMARY_MAX + 1), None) else {
            panic!("281 characters are too many");
        };
        assert!(message.contains("281 characters"), "{message}");
        assert!(check("  \n ", None).is_err(), "blank");
    }

    #[test]
    fn a_link_is_an_http_url() {
        let link = |text: &str| check("s", Some(text)).map(|(_, link)| link);
        assert_eq!(
            link("https://example.com/pull/7").unwrap().as_deref(),
            Some("https://example.com/pull/7")
        );
        assert_eq!(
            link("http://localhost:3000/").unwrap().as_deref(),
            Some("http://localhost:3000/")
        );
        assert_eq!(link("  ").unwrap(), None, "blank means none");
        for bad in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "example.com/pull/7",
            "https://",
            "not a url",
        ] {
            assert!(link(bad).is_err(), "{bad} was accepted");
        }
        let long = format!("https://example.com/{}", "a".repeat(LINK_MAX));
        assert!(link(&long).is_err());
    }

    /// A refusal names the field that broke a rule, in the order state,
    /// summary, link, and says what the tool's `denied` says.
    #[test]
    fn a_refusal_names_its_field() {
        let refused = |state: &str, summary: &str, link: Option<&str>| {
            validate(state, summary, link, None).unwrap_err()
        };
        let state = refused("done", "x", None);
        assert_eq!(state.field, "state");
        let Err(Error::Denied(tool)) = ReportedState::parse("done") else {
            panic!("done is not a state");
        };
        assert_eq!(state.message, tool);
        let summary = refused("working", &"x".repeat(SUMMARY_MAX + 1), None);
        assert_eq!(summary.field, "summary");
        assert!(summary.message.contains("281 characters"), "{summary:?}");
        assert_eq!(refused("working", " ", None).field, "summary");
        let link = refused("working", "x", Some("ftp://example.com/"));
        assert_eq!(link.field, "link");
        assert!(link.message.contains("http or https"), "{link:?}");
        // The first to fail is named.
        assert_eq!(
            refused("done", "", Some("nope")).field,
            "state",
            "state is checked first"
        );
        assert_eq!(refused("idle", "", Some("nope")).field, "summary");
        let Checked {
            state,
            summary,
            link,
            ttl_seconds,
        } = validate(
            " blocked ",
            " needs the API key\n",
            Some(" https://example.com/i/1 "),
            Some(600.0),
        )
        .unwrap();
        assert_eq!(state, ReportedState::Blocked);
        assert_eq!(summary, "needs the API key");
        assert_eq!(link.as_deref(), Some("https://example.com/i/1"));
        assert_eq!(ttl_seconds, Some(600));
        let ttl = validate("idle", "x", Some("https://e.com/"), Some(0.0)).unwrap_err();
        assert_eq!(ttl.field, "ttl_seconds");
        assert_eq!(
            validate("idle", "", None, Some(0.0)).unwrap_err().field,
            "summary",
            "the ttl is checked last"
        );
        // The tool's refusal is the same text.
        let Err(Error::Denied(tool)) = check(&"x".repeat(SUMMARY_MAX + 1), None) else {
            panic!("too long");
        };
        assert_eq!(tool, summary_message(SUMMARY_MAX + 1));
    }

    fn summary_message(length: usize) -> String {
        format!("the summary is {length} characters; keep it to one line of at most {SUMMARY_MAX}")
    }

    #[test]
    fn a_report_reads_as_one_line() {
        assert_eq!(
            describe(ReportedState::Working, "reading\nthe  failing test", None),
            "status working: reading the failing test"
        );
        assert_eq!(
            describe(
                ReportedState::Blocked,
                "needs a key",
                Some("https://example.com/i/1")
            ),
            "status blocked: needs a key (https://example.com/i/1)"
        );
    }

    #[test]
    fn the_latest_report_wins_and_carries_its_time() {
        let events = vec![
            report(1, ReportedState::Working, "first"),
            event(2, Activity::Warning("w".into())),
            report(3, ReportedState::Blocked, "second"),
            event(4, Activity::Prompt("again".into())),
        ];
        let latest = latest(&events, 5).unwrap();
        assert_eq!(latest.state, ReportedState::Blocked);
        assert_eq!(latest.summary, "second");
        assert_eq!(latest.at_ms, 3);
        assert!(
            !latest.stale,
            "a turn that has not finished leaves it current"
        );
        assert_eq!(super::latest(&events[..1], 5).unwrap().summary, "first");
        assert_eq!(super::latest(&[], 5), None);
    }

    #[test]
    fn the_wire_forms_are_snake_case_and_omit_an_absent_link() {
        let activity = Activity::StatusReport {
            state: ReportedState::Complete,
            summary: "done".into(),
            link: None,
            ttl_seconds: None,
        };
        assert_eq!(
            serde_json::to_value(&activity).unwrap(),
            serde_json::json!({"status_report": {"state": "complete", "summary": "done"}})
        );
        let with_link: Activity = serde_json::from_value(serde_json::json!({
            "status_report": {"state": "failure", "summary": "x", "link": "https://e.com/"}
        }))
        .unwrap();
        assert_eq!(
            with_link,
            Activity::StatusReport {
                state: ReportedState::Failure,
                summary: "x".into(),
                link: Some("https://e.com/".into()),
                ttl_seconds: None,
            }
        );
        let report = StatusReport::new(ReportedState::Idle, "waiting".into(), None, None, 9);
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({"state": "idle", "summary": "waiting", "at_ms": 9})
        );
        // A time-to-live is kept on the activity, and the report says when
        // it expires and, once read stale, why.
        let timed = Activity::StatusReport {
            state: ReportedState::Blocked,
            summary: "needs a key".into(),
            link: None,
            ttl_seconds: Some(600),
        };
        assert_eq!(
            serde_json::to_value(&timed).unwrap(),
            serde_json::json!({"status_report": {
                "state": "blocked", "summary": "needs a key", "ttl_seconds": 600
            }})
        );
        let read = latest(&[event(1_000, timed)], 601_000).unwrap();
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::json!({
                "state": "blocked", "summary": "needs a key", "at_ms": 1000,
                "ttl_seconds": 600, "expires_at_ms": 601_000,
                "stale": true, "stale_because": "expired"
            })
        );
        let back: StatusReport =
            serde_json::from_value(serde_json::to_value(&read).unwrap()).unwrap();
        assert_eq!(back, read);
    }

    #[test]
    fn a_ttl_is_whole_seconds_from_one_to_seven_days() {
        assert_eq!(check_ttl(None).unwrap(), None);
        assert_eq!(check_ttl(Some(1.0)).unwrap(), Some(1));
        assert_eq!(
            check_ttl(Some(TTL_MAX_SECONDS as f64)).unwrap(),
            Some(604_800)
        );
        for bad in [0.0, -5.0, 1.5, 604_801.0, f64::NAN, f64::INFINITY] {
            let Err(Error::Denied(message)) = check_ttl(Some(bad)) else {
                panic!("{bad} was accepted");
            };
            assert!(
                message.starts_with("ttl_seconds must be a whole number"),
                "{message}"
            );
        }
    }

    fn timed(at_ms: u64, state: ReportedState, ttl_seconds: u64) -> RecordedEvent {
        event(
            at_ms,
            Activity::StatusReport {
                state,
                summary: "s".into(),
                link: None,
                ttl_seconds: Some(ttl_seconds),
            },
        )
    }

    /// A report expires at `at_ms + ttl`, not a millisecond before.
    #[test]
    fn a_report_expires_when_its_ttl_has_passed() {
        let events = [timed(10_000, ReportedState::Blocked, 60)];
        let current = latest(&events, 69_999).unwrap();
        assert_eq!(current.expires_at_ms, Some(70_000));
        assert!(!current.stale);
        assert!(current.is_doing());
        let expired = latest(&events, 70_000).unwrap();
        assert!(expired.stale);
        assert_eq!(expired.stale_because, Some(StaleBecause::Expired));
        assert!(
            !expired.is_doing(),
            "a stale report is not what it is doing"
        );
        // Without a ttl, time alone never makes it stale.
        let forever = latest(&[report(1, ReportedState::Working, "x")], u64::MAX).unwrap();
        assert!(!forever.stale);
        assert_eq!(forever.expires_at_ms, None);
    }

    /// A later turn that finished without a report, or the branch ending,
    /// makes it stale; a turn still running, or the same turn ending, does
    /// not.
    #[test]
    fn a_later_finished_turn_or_an_end_makes_it_stale() {
        let failed = || {
            Activity::Status(BranchStatus::Failed {
                reason: "exit 1".into(),
            })
        };
        let base = || {
            vec![
                event(1, Activity::Prompt("go".into())),
                report(2, ReportedState::Blocked, "needs a key"),
            ]
        };
        let judged = |extra: Vec<Activity>| {
            let mut events = base();
            events.extend(extra.into_iter().map(|a| event(9, a)));
            latest(&events, 10).unwrap().stale_because
        };
        // Its own turn ending leaves it current.
        assert_eq!(judged(vec![Activity::Status(BranchStatus::Ready)]), None);
        // A later turn running leaves it current.
        assert_eq!(
            judged(vec![
                Activity::Status(BranchStatus::Ready),
                Activity::Prompt("again".into()),
                Activity::Status(BranchStatus::Running),
            ]),
            None
        );
        // ...until it finishes.
        assert_eq!(
            judged(vec![
                Activity::Status(BranchStatus::Ready),
                Activity::Prompt("again".into()),
                Activity::Status(BranchStatus::Running),
                Activity::Status(BranchStatus::NoChanges),
            ]),
            Some(StaleBecause::LaterTurn)
        );
        // Ending says so first, in its own turn or a later one.
        assert_eq!(judged(vec![failed()]), Some(StaleBecause::Ended));
        assert_eq!(
            judged(vec![Activity::Prompt("again".into()), failed()]),
            Some(StaleBecause::Ended)
        );
        assert_eq!(
            judged(vec![Activity::Status(BranchStatus::Discarded {
                reason: "r".into()
            })]),
            Some(StaleBecause::Ended)
        );
        // A report made after the end is current again.
        let mut events = base();
        events.push(event(3, failed()));
        events.push(report(4, ReportedState::Idle, "looked again"));
        assert!(!latest(&events, 5).unwrap().stale);
        // An event-based reason wins over expiry.
        let events = [
            timed(1, ReportedState::Working, 1),
            event(2, Activity::Prompt("again".into())),
            event(3, Activity::Status(BranchStatus::Ready)),
        ];
        assert_eq!(
            latest(&events, 1_000_000).unwrap().stale_because,
            Some(StaleBecause::LaterTurn)
        );
    }
}
