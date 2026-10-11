//! A status a branch's harness states about itself, beside the one the
//! protocol gives.
//!
//! A branch's [`BranchStatus`](crate::BranchStatus) comes from the harness
//! protocol and the engine: running, ready, failed. A harness can also say
//! what it is doing in its own words, with the `report_status` tool (or
//! `by report-status`): a [state](ReportedState), a one-line summary and an
//! optional link. The claim is recorded on the branch as
//! [`Activity::StatusReport`] and shown beside the protocol's status in
//! `by log`, `by watch`, `by show` and [`Inspection::reported`]; it never
//! changes that status, and nothing relies on it being true. The one thing
//! it triggers is attention: a `blocked` or `failure` report is escalated to
//! the branch's parent's inbox, as `escalate` would.
//!
//! [`Inspection::reported`]: crate::Inspection::reported

use std::str::FromStr as _;

use serde::{Deserialize, Serialize};

use crate::{Activity, Error, Message, RecordedEvent};

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
}

impl StatusReport {
    /// One line: `status blocked: waiting on the API key (https://...)`.
    pub fn describe(&self) -> String {
        describe(self.state, &self.summary, self.link.as_deref())
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
    /// The field that broke a rule: `state`, `summary` or `link`.
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

/// A whole report as the `report_status` tool takes it, checked: the
/// state, and the summary and link as [`check`] keeps them. The first
/// field to break a rule is named in the [`Refusal`], in the order
/// `state`, `summary`, `link`.
pub fn validate(
    state: &str,
    summary: &str,
    link: Option<&str>,
) -> Result<(ReportedState, String, Option<String>), Refusal> {
    let state = parse_state(state).map_err(|message| Refusal::new("state", message))?;
    let (summary, link) = check_fields(summary, link)?;
    Ok((state, summary, link))
}

/// The latest self-reported status in `events`, oldest first, if the
/// branch ever reported one.
pub fn latest(events: &[RecordedEvent]) -> Option<StatusReport> {
    events.iter().rev().find_map(|event| match &event.activity {
        Activity::StatusReport {
            state,
            summary,
            link,
        } => Some(StatusReport {
            state: *state,
            summary: summary.clone(),
            link: link.clone(),
            at_ms: event.at_ms,
        }),
        _ => None,
    })
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
            validate(state, summary, link).unwrap_err()
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
        let (state, summary, link) = validate(
            " blocked ",
            " needs the API key\n",
            Some(" https://example.com/i/1 "),
        )
        .unwrap();
        assert_eq!(state, ReportedState::Blocked);
        assert_eq!(summary, "needs the API key");
        assert_eq!(link.as_deref(), Some("https://example.com/i/1"));
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
        let latest = latest(&events).unwrap();
        assert_eq!(latest.state, ReportedState::Blocked);
        assert_eq!(latest.summary, "second");
        assert_eq!(latest.at_ms, 3);
        assert_eq!(super::latest(&events[..1]).unwrap().summary, "first");
        assert_eq!(super::latest(&[]), None);
    }

    #[test]
    fn the_wire_forms_are_snake_case_and_omit_an_absent_link() {
        let activity = Activity::StatusReport {
            state: ReportedState::Complete,
            summary: "done".into(),
            link: None,
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
            }
        );
        let report = StatusReport {
            state: ReportedState::Idle,
            summary: "waiting".into(),
            link: None,
            at_ms: 9,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({"state": "idle", "summary": "waiting", "at_ms": 9})
        );
    }
}
