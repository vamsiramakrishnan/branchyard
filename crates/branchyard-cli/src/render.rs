//! Human output: one compact line per event, tables, branch summaries and
//! minimal ANSI color. Everything here is pure so it can be tested without a
//! terminal or an engine.

use std::collections::{HashMap, HashSet};

use branchyard::{
    Activity, BranchInfo, BranchStatus, CandidateInfo, Event, HarnessInfo, RecordedEvent,
    TurnOutcome, Usage,
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::args::shell_quote;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl Tone {
    fn code(self) -> &'static str {
        match self {
            Tone::Bold => "1",
            Tone::Dim => "2",
            Tone::Red => "31",
            Tone::Green => "32",
            Tone::Yellow => "33",
            Tone::Blue => "34",
            Tone::Magenta => "35",
            Tone::Cyan => "36",
        }
    }
}

/// Whether to emit ANSI color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    #[cfg(test)]
    pub const PLAIN: Style = Style { color: false };

    pub fn paint(self, tone: Tone, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{}m{text}\x1b[0m", tone.code())
        } else {
            text.to_owned()
        }
    }
}

/// Branch prefix colors for interleaved output, in order of first appearance.
const BRANCH_TONES: [Tone; 5] = [
    Tone::Cyan,
    Tone::Magenta,
    Tone::Yellow,
    Tone::Green,
    Tone::Blue,
];

/// How many terminal columns `text` takes: two for a wide (East Asian)
/// character, none for a combining mark, one otherwise.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Shorten `text` to `max` terminal columns, marking the cut with an
/// ellipsis. A wide character that would straddle the limit is dropped
/// whole.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let room = max.saturating_sub(1);
    let mut short = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        short.push(c);
    }
    short.push('…');
    short
}

/// `text` followed by spaces up to `columns` terminal columns.
fn pad_right(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

/// A tool invocation's input as one short line: the field a person would
/// recognize (a command, a path) when there is one, else compact JSON.
pub fn compact_input(input: &Value) -> String {
    const KEYS: [&str; 7] = [
        "command",
        "cmd",
        "file_path",
        "path",
        "url",
        "pattern",
        "query",
    ];
    let text = match input {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Object(map) => KEYS
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| input.to_string()),
        other => other.to_string(),
    };
    truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), 80)
}

pub fn usd(value: f64) -> String {
    if value > 0.0 && value < 0.01 {
        format!("${value:.4}")
    } else {
        format!("${value:.2}")
    }
}

pub fn tokens(count: u64) -> String {
    match count {
        0..=999 => count.to_string(),
        1_000..=999_999 => format!("{:.1}k", count as f64 / 1e3),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

fn usage_text(usage: &Usage) -> String {
    let parts: Vec<String> = [
        usage.cost_usd.map(usd),
        usage.input_tokens.map(|n| format!("{} in", tokens(n))),
        usage.output_tokens.map(|n| format!("{} out", tokens(n))),
        usage
            .cached_input_tokens
            .map(|n| format!("{} cached", tokens(n))),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        return "usage reported without figures".into();
    }
    let scope = if usage.cumulative { " (session)" } else { "" };
    format!("usage {}{scope}", parts.join(" · "))
}

fn outcome_line(turn: u64, outcome: &TurnOutcome, style: Style) -> String {
    match outcome {
        TurnOutcome::Completed => style.paint(Tone::Green, &format!("turn {turn} completed")),
        TurnOutcome::Interrupted => style.paint(Tone::Yellow, &format!("turn {turn} interrupted")),
        TurnOutcome::Failed { message } => {
            style.paint(Tone::Red, &format!("turn {turn} failed: {message}"))
        }
        TurnOutcome::LimitReached { limit } => style.paint(
            Tone::Yellow,
            &format!("turn {turn} stopped at limit: {limit}"),
        ),
        TurnOutcome::Refused => style.paint(Tone::Yellow, &format!("turn {turn} refused")),
    }
}

/// One line for a non-text event. Message deltas are text, not lines, and
/// return `None`.
pub fn event_line(event: &Event, style: Style) -> Option<String> {
    let dim = |text: String| style.paint(Tone::Dim, &text);
    Some(match event {
        Event::MessageDelta { .. } => return None,
        Event::Ready => dim("harness ready".into()),
        Event::SessionStarted {
            session,
            forked_from: None,
        } => dim(format!("session {session}")),
        Event::SessionStarted {
            session,
            forked_from: Some(parent),
        } => dim(format!("session {session} forked from {parent}")),
        Event::OpenFailed { reason } => style.paint(Tone::Red, &format!("open failed: {reason}")),
        Event::TurnAccepted { turn, .. } => dim(format!("turn {turn} accepted")),
        Event::ToolStarted { name, .. } => format!("{} {name}", style.paint(Tone::Cyan, "▸")),
        Event::PermissionRequested { request, .. } => style.paint(
            Tone::Yellow,
            format!("asks {}: {}", request.tool, compact_input(&request.input))
                .trim_end_matches([':', ' ']),
        ),
        Event::PermissionWithdrawn { key } => {
            dim(format!("permission request {} withdrawn", key.0))
        }
        Event::UsageObserved { usage, .. } => dim(usage_text(usage)),
        Event::InterruptAcknowledged { turn } => dim(format!("turn {turn} interrupt acknowledged")),
        Event::SteerAccepted { turn, steer } => {
            dim(format!("turn {turn} took steered input {steer}"))
        }
        Event::SteerRejected {
            turn,
            steer,
            reason,
        } => style.paint(
            Tone::Yellow,
            &format!("turn {turn} did not take steered input {steer}: {reason}"),
        ),
        Event::TurnEnded { turn, outcome } => outcome_line(*turn, outcome, style),
        Event::OutcomeUnknown { turn, reason } => {
            style.paint(Tone::Red, &format!("turn {turn} outcome unknown: {reason}"))
        }
        Event::UnsupportedRequest { method } => dim(format!("unsupported request {method}")),
        Event::Warning { message } => style.paint(Tone::Yellow, &format!("warning: {message}")),
        Event::ProtocolViolation { detail } => {
            style.paint(Tone::Red, &format!("protocol violation: {detail}"))
        }
        Event::Unrecognized { kind } => dim(format!("unrecognized {kind}")),
        Event::SessionClosed => dim("session closed".into()),
    })
}

/// The answer a permission request got, as one line.
pub fn decision_line(tool: &str, allowed: bool, message: Option<&str>, style: Style) -> String {
    match (allowed, message) {
        (true, _) => style.paint(Tone::Green, &format!("allowed {tool}")),
        (false, Some(message)) => style.paint(Tone::Red, &format!("denied {tool}: {message}")),
        (false, None) => style.paint(Tone::Red, &format!("denied {tool}")),
    }
}

/// One line for recorded activity. Message deltas are text, not lines, and
/// return `None`.
pub fn activity_line(activity: &Activity, style: Style) -> Option<String> {
    Some(match activity {
        Activity::Harness(event) => return event_line(event, style),
        Activity::Prompt(text) => style.paint(
            Tone::Bold,
            &format!(
                "prompt: {}",
                truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), 100)
            ),
        ),
        Activity::Decision {
            tool,
            allowed,
            message,
            ..
        } => decision_line(tool, *allowed, message.as_deref(), style),
        Activity::Snapshot(candidate) => style.paint(
            Tone::Green,
            &format!(
                "candidate {} ({})",
                short_commit(&candidate.commit),
                candidate_text(Some(candidate))
            ),
        ),
        Activity::Status(status) => {
            let (text, tone) = status_text(status);
            style.paint(tone, &format!("status: {text}"))
        }
        Activity::Warning(message) => style.paint(Tone::Yellow, &format!("warning: {message}")),
        Activity::Delegation {
            tool,
            branch,
            outcome,
            refused,
        } => {
            let tone = if *refused { Tone::Red } else { Tone::Cyan };
            let target = if branch.is_empty() {
                String::new()
            } else {
                format!(" {branch}")
            };
            let verb = if *refused { "refused" } else { "delegated" };
            style.paint(tone, &format!("{verb}: {tool}{target}: {outcome}"))
        }
        Activity::Provisioned {
            auth,
            files,
            env,
            secrets,
            unused_secrets,
        } => {
            let mut parts = Vec::new();
            if let Some(auth) = auth {
                parts.push(format!("auth {auth}"));
            }
            if !files.is_empty() {
                parts.push(format!("wrote {}", files.join(", ")));
            }
            if !env.is_empty() {
                parts.push(format!("set {}", env.join(", ")));
            }
            if !unused_secrets.is_empty() {
                parts.push(format!("unused secrets {}", unused_secrets.join(", ")));
            }
            let exposed: Vec<&str> = secrets
                .iter()
                .filter(|d| d.tool_env)
                .map(|d| d.secret.as_str())
                .collect();
            if !exposed.is_empty() {
                parts.push(format!(
                    "{} in the environment of its tool commands",
                    exposed.join(", ")
                ));
            }
            style.paint(Tone::Dim, &format!("provisioned: {}", parts.join("; ")))
        }
        Activity::Steered { by, text, .. } => style.paint(
            Tone::Bold,
            &format!(
                "steered by {by}: {}",
                truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), 100)
            ),
        ),
        Activity::Recovered { reason, killed } => {
            let killed = match killed.is_empty() {
                true => String::new(),
                false => format!(
                    " (killed {})",
                    killed
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            style.paint(Tone::Yellow, &format!("recovered: {reason}{killed}"))
        }
        Activity::Stalled { .. } => style.paint(
            Tone::Yellow,
            "stalled: no harness activity for its stall window",
        ),
        Activity::Resumed => style.paint(Tone::Cyan, "resumed: activity seen again"),
        Activity::Message(message) => {
            let reply = match message.in_reply_to {
                Some(id) => format!(" (re #{id})"),
                None => String::new(),
            };
            style.paint(
                Tone::Cyan,
                &format!(
                    "message #{} {} {} -> {}{reply}: {}",
                    message.id,
                    message.kind,
                    message.from,
                    message.to,
                    truncate(
                        &message
                            .text
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                        100
                    )
                ),
            )
        }
        Activity::PullRequest(activity) => {
            let (text, tone) = crate::pr::log_line(activity);
            style.paint(tone, &text)
        }
        Activity::MessagesDelivered { ids, via } => {
            let ids = ids
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ");
            let via = match via {
                branchyard::DeliveredVia::TurnStart { boundary } => {
                    format!("at the turn's start ({boundary})")
                }
                branchyard::DeliveredVia::Steer { steer, boundary } => {
                    format!("into the running turn (steered input {steer}, {boundary})")
                }
            };
            style.paint(Tone::Cyan, &format!("delivered {ids} {via}"))
        }
    })
}

/// Turns a stream of events from one or more branches into terminal text.
///
/// Unprefixed, message deltas stream through as they arrive. Prefixed, for
/// several branches at once, deltas are buffered per branch and emitted as
/// whole lines so branches never interleave within a line.
pub struct Renderer {
    style: Style,
    prefixed: bool,
    partial: HashMap<String, String>,
    mid_line: bool,
    tones: HashMap<String, Tone>,
    width: usize,
}

impl Renderer {
    pub fn new(style: Style, prefixed: bool) -> Self {
        Renderer {
            style,
            prefixed,
            partial: HashMap::new(),
            mid_line: false,
            tones: HashMap::new(),
            width: 0,
        }
    }

    pub fn event(&mut self, branch: &str, event: &Event) -> String {
        match event {
            Event::MessageDelta { text, .. } => return self.text(branch, text),
            // Kept in the record for `by log`; noise in live output.
            Event::Unrecognized { .. } => return String::new(),
            _ => {}
        }
        match event_line(event, self.style) {
            Some(line) => self.line(branch, &line),
            None => String::new(),
        }
    }

    /// Live output for one activity. The prompt is not echoed, and status
    /// changes are left to the closing summary.
    pub fn activity(&mut self, branch: &str, activity: &Activity) -> String {
        match activity {
            Activity::Harness(event) => self.event(branch, event),
            Activity::Prompt(_) | Activity::Status(_) => String::new(),
            other => match activity_line(other, self.style) {
                Some(line) => self.line(branch, &line),
                None => String::new(),
            },
        }
    }

    /// Fix prefix colors and width for branches known in advance, so the
    /// first lines already line up.
    pub fn reserve(&mut self, branches: &[String]) {
        for branch in branches {
            let next = BRANCH_TONES[self.tones.len() % BRANCH_TONES.len()];
            self.tones.entry(branch.clone()).or_insert(next);
            self.width = self.width.max(width(branch));
        }
    }

    /// A whole line for `branch`, ending any text in progress first.
    pub fn line(&mut self, branch: &str, line: &str) -> String {
        let mut out = self.end_text(branch);
        out.push_str(&self.prefix(branch));
        out.push_str(line);
        out.push('\n');
        out
    }

    /// End every branch's text in progress, such as before a prompt or at exit.
    pub fn finish(&mut self) -> String {
        let mut branches: Vec<String> = self.partial.keys().cloned().collect();
        branches.sort();
        let mut out: String = branches.iter().map(|b| self.end_text(b)).collect();
        if self.mid_line {
            self.mid_line = false;
            out.push('\n');
        }
        out
    }

    fn text(&mut self, branch: &str, text: &str) -> String {
        if !self.prefixed {
            if !text.is_empty() {
                self.mid_line = !text.ends_with('\n');
            }
            return text.to_owned();
        }
        let buffer = self.partial.entry(branch.to_owned()).or_default();
        buffer.push_str(text);
        let mut lines = Vec::new();
        while let Some(end) = buffer.find('\n') {
            let line: String = buffer.drain(..=end).collect();
            lines.push(line.trim_end_matches('\n').to_owned());
        }
        lines
            .iter()
            .map(|line| format!("{}{line}\n", self.prefix(branch)))
            .collect()
    }

    fn end_text(&mut self, branch: &str) -> String {
        if !self.prefixed {
            if self.mid_line {
                self.mid_line = false;
                return "\n".into();
            }
            return String::new();
        }
        match self.partial.remove(branch) {
            Some(text) if !text.is_empty() => format!("{}{text}\n", self.prefix(branch)),
            _ => String::new(),
        }
    }

    fn prefix(&mut self, branch: &str) -> String {
        if !self.prefixed {
            return String::new();
        }
        let next = BRANCH_TONES[self.tones.len() % BRANCH_TONES.len()];
        let tone = *self.tones.entry(branch.to_owned()).or_insert(next);
        self.width = self.width.max(width(branch));
        let label = format!("{} │", pad_right(branch, self.width));
        format!("{} ", self.style.paint(tone, &label))
    }
}

/// A table column: header, maximum width before truncation, alignment.
pub struct Column {
    pub header: &'static str,
    pub max: usize,
    pub right: bool,
}

pub struct Cell {
    pub text: String,
    pub tone: Option<Tone>,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            tone: None,
        }
    }

    pub fn toned(text: impl Into<String>, tone: Tone) -> Cell {
        Cell {
            text: text.into(),
            tone: Some(tone),
        }
    }
}

/// Columns separated by two spaces, cells truncated to their column's
/// maximum, with no trailing whitespace.
pub fn table(columns: &[Column], rows: &[Vec<Cell>], style: Style) -> String {
    let cells: Vec<Vec<(String, Option<Tone>)>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(columns)
                .map(|(cell, column)| (truncate(&cell.text, column.max), cell.tone))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            cells
                .iter()
                .map(|row| width(&row[i].0))
                .chain([column.header.len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let header: Vec<(String, Option<Tone>)> = columns
        .iter()
        .map(|column| (column.header.to_owned(), Some(Tone::Dim)))
        .collect();
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&cells) {
        let mut line = String::new();
        for (i, ((text, tone), column)) in row.iter().zip(columns).enumerate() {
            let pad = " ".repeat(widths[i].saturating_sub(width(text)));
            let painted = match tone {
                Some(tone) => style.paint(*tone, text),
                None => text.clone(),
            };
            if i > 0 {
                line.push_str("  ");
            }
            if column.right {
                line.push_str(&pad);
                line.push_str(&painted);
            } else {
                line.push_str(&painted);
                if i + 1 < columns.len() {
                    line.push_str(&pad);
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub fn status_text(status: &BranchStatus) -> (String, Tone) {
    match status {
        BranchStatus::Running => ("running".into(), Tone::Cyan),
        BranchStatus::Ready => ("ready".into(), Tone::Green),
        BranchStatus::NoChanges => ("no changes".into(), Tone::Dim),
        BranchStatus::Interrupted => ("interrupted".into(), Tone::Yellow),
        BranchStatus::BudgetExceeded { limit } => (format!("over budget: {limit}"), Tone::Yellow),
        BranchStatus::Failed { reason } => (format!("failed: {reason}"), Tone::Red),
        BranchStatus::Merged { target, .. } => (format!("merged into {target}"), Tone::Blue),
        BranchStatus::Waiting => ("waiting".into(), Tone::Dim),
        BranchStatus::Blocked { reason } => (format!("blocked: {reason}"), Tone::Red),
    }
}

/// [`status_text`], with a `stalled` marker while the branch's running turn
/// has had no harness activity for its stall window.
pub fn branch_status_text(info: &BranchInfo) -> (String, Tone) {
    let (text, tone) = status_text(&info.status);
    match info.stalled {
        true => (format!("{text} (stalled)"), Tone::Yellow),
        false => (text, tone),
    }
}

pub fn candidate_text(candidate: Option<&CandidateInfo>) -> String {
    match candidate {
        None => "-".into(),
        Some(c) => {
            let files = if c.files_changed == 1 {
                "file"
            } else {
                "files"
            };
            format!(
                "{} {files} +{} -{}",
                c.files_changed, c.insertions, c.deletions
            )
        }
    }
}

pub fn cost_text(cost: Option<f64>) -> String {
    cost.map(usd).unwrap_or_else(|| "-".into())
}

pub fn age_text(seconds: u64) -> String {
    match seconds {
        0..=9 => "now".into(),
        10..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// Branches with each fork directly under its parent. A branch whose parent
/// is gone is shown as a root.
pub fn tree_order(infos: &[BranchInfo]) -> Vec<(usize, &BranchInfo)> {
    let names: HashSet<&str> = infos.iter().map(|info| info.name.as_str()).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    fn visit<'a>(
        info: &'a BranchInfo,
        depth: usize,
        infos: &'a [BranchInfo],
        seen: &mut HashSet<&'a str>,
        out: &mut Vec<(usize, &'a BranchInfo)>,
    ) {
        if !seen.insert(&info.name) {
            return;
        }
        out.push((depth, info));
        for child in infos
            .iter()
            .filter(|child| child.parent.as_deref() == Some(&info.name))
        {
            visit(child, depth + 1, infos, seen, out);
        }
    }
    for root in infos.iter().filter(|info| {
        info.parent
            .as_deref()
            .is_none_or(|parent| !names.contains(parent))
    }) {
        visit(root, 0, infos, &mut seen, &mut out);
    }
    out
}

/// `by ls`.
pub fn branch_table(infos: &[BranchInfo], now: u64, style: Style) -> String {
    let columns = [
        Column {
            header: "BRANCH",
            max: 40,
            right: false,
        },
        Column {
            header: "HARNESS",
            max: 20,
            right: false,
        },
        Column {
            header: "STATUS",
            max: 32,
            right: false,
        },
        Column {
            header: "CANDIDATE",
            max: 24,
            right: false,
        },
        Column {
            header: "TURNS",
            max: 6,
            right: true,
        },
        Column {
            header: "COST",
            max: 10,
            right: true,
        },
        Column {
            header: "AGE",
            max: 6,
            right: true,
        },
    ];
    let rows: Vec<Vec<Cell>> = tree_order(infos)
        .into_iter()
        .map(|(depth, info)| {
            let name = match depth {
                0 => info.name.clone(),
                _ => format!("{}└ {}", "  ".repeat(depth - 1), info.name),
            };
            let (status, tone) = branch_status_text(info);
            vec![
                Cell::plain(name),
                Cell::plain(&info.harness),
                Cell::toned(status, tone),
                Cell::plain(candidate_text(info.candidate.as_ref())),
                Cell::plain(info.turns.to_string()),
                Cell::plain(cost_text(info.cost_usd)),
                Cell::plain(age_text(now.saturating_sub(info.created_at))),
            ]
        })
        .collect();
    table(&columns, &rows, style)
}

/// The table closing `by fan`.
pub fn comparison_table(infos: &[&BranchInfo], style: Style) -> String {
    let columns = [
        Column {
            header: "BRANCH",
            max: 40,
            right: false,
        },
        Column {
            header: "STATUS",
            max: 40,
            right: false,
        },
        Column {
            header: "CANDIDATE",
            max: 24,
            right: false,
        },
        Column {
            header: "TURNS",
            max: 6,
            right: true,
        },
        Column {
            header: "COST",
            max: 10,
            right: true,
        },
    ];
    let rows: Vec<Vec<Cell>> = infos
        .iter()
        .map(|info| {
            let (status, tone) = status_text(&info.status);
            vec![
                Cell::plain(&info.name),
                Cell::toned(status, tone),
                Cell::plain(candidate_text(info.candidate.as_ref())),
                Cell::plain(info.turns.to_string()),
                Cell::plain(cost_text(info.cost_usd)),
            ]
        })
        .collect();
    table(&columns, &rows, style)
}

/// `by harnesses`.
pub fn harness_table(harnesses: &[HarnessInfo], style: Style) -> String {
    let columns = [
        Column {
            header: "HARNESS",
            max: 24,
            right: false,
        },
        Column {
            header: "PROFILE",
            max: 32,
            right: false,
        },
        Column {
            header: "DEFAULT",
            max: 7,
            right: false,
        },
        Column {
            header: "ON PATH",
            max: 7,
            right: false,
        },
        Column {
            header: "QUALIFICATION",
            max: 48,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = harnesses
        .iter()
        .map(|h| {
            vec![
                Cell::plain(&h.harness),
                Cell::plain(&h.profile),
                Cell::plain(if h.default { "*" } else { "" }),
                match h.available {
                    true => Cell::toned("yes", Tone::Green),
                    false => Cell::toned("no", Tone::Dim),
                },
                match &h.qualification {
                    Some(q) => Cell::plain(q),
                    None => Cell::toned("unqualified", Tone::Dim),
                },
            ]
        })
        .collect();
    table(&columns, &rows, style)
}

/// Commands that make sense after a branch reaches its status.
pub fn next_commands(info: &BranchInfo) -> Vec<String> {
    let name = shell_quote(&info.name);
    match &info.status {
        BranchStatus::Ready => vec![format!("by diff {name}"), format!("by merge {name}")],
        BranchStatus::NoChanges => vec![
            format!("by send {name} \"<prompt>\""),
            format!("by rm {name}"),
        ],
        BranchStatus::Running => vec![format!("by log {name}")],
        BranchStatus::Merged { .. } => vec![format!("by rm {name}")],
        BranchStatus::Waiting | BranchStatus::Blocked { .. } => {
            let parent = info.parent.as_deref().map(shell_quote).unwrap_or_default();
            vec![format!("by graph show {parent}"), format!("by rm {name}")]
        }
        BranchStatus::Interrupted
        | BranchStatus::BudgetExceeded { .. }
        | BranchStatus::Failed { .. } => {
            let mut next = vec![format!("by log {name}")];
            if info.candidate.is_some() {
                next.push(format!("by diff {name}"));
            }
            next.push(format!("by rm {name}"));
            next
        }
    }
}

pub(crate) fn key_values(pairs: &[(&str, String)], style: Style) -> String {
    let width = pairs.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (key, value) in pairs {
        let mut lines = value.lines();
        let first = lines.next().unwrap_or("");
        let label = style.paint(Tone::Dim, &format!("{key:<width$}"));
        out.push_str(format!("{label}  {first}").trim_end());
        out.push('\n');
        for line in lines {
            out.push_str(format!("{:width$}  {line}", "").trim_end());
            out.push('\n');
        }
    }
    out
}

fn candidate_detail(info: &BranchInfo) -> String {
    match &info.candidate {
        Some(c) => format!("{} at {}", candidate_text(Some(c)), short_commit(&c.commit)),
        None => "none".into(),
    }
}

fn cost_detail(info: &BranchInfo) -> String {
    let turns = if info.turns == 1 { "turn" } else { "turns" };
    match info.cost_usd {
        Some(cost) => format!("{} over {} {turns}", usd(cost), info.turns),
        None => format!("not reported, {} {turns}", info.turns),
    }
}

/// The summary closing `by run`, `by send` and `by fork`.
pub fn summary(info: &BranchInfo, style: Style) -> String {
    let (status, tone) = status_text(&info.status);
    let mut pairs = vec![
        ("branch", format!("{} ({})", info.name, info.git_branch)),
        ("status", style.paint(tone, &status)),
        ("candidate", candidate_detail(info)),
        ("cost", cost_detail(info)),
    ];
    let next = next_commands(info);
    if !next.is_empty() {
        pairs.push(("next", next.join("\n")));
    }
    key_values(&pairs, style)
}

/// `by inspect`.
pub fn inspection(i: &branchyard::Inspection, style: Style) -> String {
    let (status, tone) = status_text(&i.status);
    let (status, tone) = match i.stalled {
        true => (format!("{status} (stalled)"), Tone::Yellow),
        false => (status, tone),
    };
    let money = |v: Option<f64>| v.map_or("unknown".into(), usd);
    let mut pairs = vec![
        ("branch", i.name.clone()),
        ("status", style.paint(tone, &status)),
        ("harness", format!("{} ({})", i.harness, i.profile)),
        ("parent", i.parent.clone().unwrap_or_else(|| "none".into())),
        (
            "children",
            match i.children.is_empty() {
                true => "none".into(),
                false => i.children.join(", "),
            },
        ),
        ("candidate", candidate_text(i.candidate.as_ref())),
        ("turns", i.turns.to_string()),
        ("cost", money(i.cost_usd)),
        ("subtree cost", usd(i.subtree_cost_usd)),
    ];
    if let Some(max) = i.max_usd {
        pairs.push((
            "budget",
            format!("{} of {} left", money(i.remaining_usd), usd(max)),
        ));
    }
    if let Some(envelope) = &i.envelope {
        let harnesses = match envelope.harnesses.is_empty() {
            true => "its own".to_owned(),
            false => envelope.harnesses.join(", "),
        };
        pairs.push((
            "envelope",
            format!(
                "depth {}, {} children, harnesses: {harnesses}",
                envelope.max_depth, envelope.max_children
            ),
        ));
    }
    if let Some(seat) = &i.seat {
        let spawns = match i.seats.is_empty() {
            true => "none".to_owned(),
            false => i.seats.join(", "),
        };
        pairs.push(("seat", format!("{seat}; spawns seats: {spawns}")));
    }
    if !i.depends_on.is_empty() {
        pairs.push(("depends on", dependencies_text(&i.depends_on)));
    }
    if !i.bindings.is_empty() {
        pairs.push(("bindings", bindings_text(&i.bindings)));
    }
    if i.graph_revision > 0 {
        pairs.push(("graph revision", i.graph_revision.to_string()));
    }
    if !i.last_message.is_empty() {
        pairs.push(("last message", i.last_message.trim_end().to_owned()));
    }
    key_values(&pairs, style)
}

fn dependencies_text(dependencies: &[branchyard::Dependency]) -> String {
    dependencies
        .iter()
        .map(|d| match d.after {
            branchyard::After::Settled => d.prerequisite.clone(),
            branchyard::After::Integrated => format!("{} (integrated)", d.prerequisite),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn bindings_text(bindings: &[branchyard::Binding]) -> String {
    bindings
        .iter()
        .map(|b| format!("{} ({})", b.scratch, b.access))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `by graph show`: each child, its status and what it waits for.
pub fn graph(g: &branchyard::Graph, style: Style) -> String {
    let mut out = format!("{}'s graph, revision {}\n", g.branch, g.revision);
    if g.children.is_empty() {
        out.push_str("  no children\n");
        return out;
    }
    let width = g.children.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for child in &g.children {
        let (status, tone) = status_text(&child.status);
        let waits: Vec<branchyard::Dependency> = g
            .dependencies
            .iter()
            .filter(|d| d.dependent == child.name)
            .cloned()
            .collect();
        let mut line = format!(
            "  {:width$}  {}",
            child.name,
            style.paint(tone, &status),
            width = width
        );
        if !waits.is_empty() {
            line.push_str(&format!("  after {}", dependencies_text(&waits)));
        }
        if !child.bindings.is_empty() {
            line.push_str(&format!("  binds {}", bindings_text(&child.bindings)));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `by graph apply`: what the proposal created, and the revision.
pub fn graph_applied(a: &branchyard::GraphApplied, style: Style) -> String {
    let mut out = format!("{}'s graph is at revision {}\n", a.branch, a.revision);
    for spawned in &a.spawned {
        let (status, tone) = status_text(&spawned.status);
        out.push_str(&format!(
            "  {} {}\n",
            spawned.name,
            style.paint(tone, &status)
        ));
    }
    if !a.dependencies.is_empty() {
        out.push_str(&format!(
            "  {} dependenc{}\n",
            a.dependencies.len(),
            if a.dependencies.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    out
}

/// `by show`.
/// With `extra` `(key, value)` lines at the end, aligned with the rest.
pub fn details(info: &BranchInfo, now: u64, style: Style, extra: Vec<(&str, String)>) -> String {
    let (status, tone) = branch_status_text(info);
    let mut pairs = vec![
        ("branch", info.name.clone()),
        ("git branch", info.git_branch.clone()),
        ("harness", format!("{} ({})", info.harness, info.profile)),
        ("status", style.paint(tone, &status)),
        ("prompt", info.prompt.clone()),
    ];
    if let Some(parent) = &info.parent {
        pairs.push(("forked from", parent.clone()));
    }
    if let Some(superseded_by) = &info.superseded_by {
        pairs.push(("reincarnated as", superseded_by.clone()));
    }
    pairs.extend([
        ("base", short_commit(&info.base).to_owned()),
        ("candidate", candidate_detail(info)),
        ("cost", cost_detail(info)),
        (
            "session",
            info.session.clone().unwrap_or_else(|| "none".into()),
        ),
        ("worktree", info.worktree.display().to_string()),
        (
            "created",
            format!("{} ago", age_text(now.saturating_sub(info.created_at))),
        ),
    ]);
    if let BranchStatus::Merged { commit, .. } = &info.status {
        pairs.push(("merge commit", short_commit(commit).to_owned()));
    }
    pairs.extend(extra);
    key_values(&pairs, style)
}

/// UTC timestamp with milliseconds, such as `2026-09-26T12:34:56.789Z`.
pub fn timestamp(at_ms: u64) -> String {
    let (seconds, millis) = (at_ms / 1000, at_ms % 1000);
    let (days, rem) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        rem / 60 % 60,
        rem % 60
    )
}

/// `by log`: one timestamped line per activity, with consecutive message
/// deltas joined into the text they spell.
pub fn log_text(events: &[RecordedEvent], style: Style) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < events.len() {
        let stamp = style.paint(Tone::Dim, &timestamp(events[i].at_ms));
        let indent = " ".repeat(24);
        if let Activity::Harness(Event::MessageDelta { .. }) = events[i].activity {
            let mut text = String::new();
            while let Some(RecordedEvent {
                activity: Activity::Harness(Event::MessageDelta { text: delta, .. }),
                ..
            }) = events.get(i)
            {
                text.push_str(delta);
                i += 1;
            }
            for (n, line) in text.trim_end_matches('\n').lines().enumerate() {
                let lead = if n == 0 { stamp.as_str() } else { &indent };
                out.push_str(format!("{lead}  {line}").trim_end());
                out.push('\n');
            }
            continue;
        }
        if let Some(line) = activity_line(&events[i].activity, style) {
            out.push_str(&format!("{stamp}  {line}\n"));
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_count_terminal_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(truncate("日本語のテキスト", 7), "日本語…");
        assert_eq!(width(&truncate("日本語のテキスト", 8)), 7);
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
        let columns = [
            Column {
                header: "NAME",
                max: 20,
                right: false,
            },
            Column {
                header: "N",
                max: 5,
                right: true,
            },
        ];
        let rows = vec![
            vec![Cell::plain("日本"), Cell::plain("1")],
            vec![Cell::plain("abcdef"), Cell::plain("22")],
        ];
        let text = table(&columns, &rows, Style::PLAIN);
        let ends: Vec<usize> = text.lines().map(width).collect();
        assert_eq!(ends, [10, 10, 10], "{text}");
    }
    use branchyard::{NativeSession, PermissionKey, PermissionRequest};
    use serde_json::json;
    use std::path::PathBuf;

    const PLAIN: Style = Style::PLAIN;

    fn request(tool: &str, input: Value) -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("k1".into()),
            tool: tool.into(),
            input,
        }
    }

    pub(crate) fn info(name: &str, parent: Option<&str>) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: PathBuf::from(format!("/repo/.branchyard/worktrees/{name}")),
            prompt: "Fix the flaky test".into(),
            harness: "claude-code".into(),
            profile: "claude-code-stream-json".into(),
            session: None,
            parent: parent.map(str::to_owned),
            children: Vec::new(),
            depth: 0,
            base: "0123456789abcdef".into(),
            candidate: None,
            status: BranchStatus::Ready,
            turns: 1,
            cost_usd: None,
            created_at: 10_000,
            stalled: false,
            superseded_by: None,
        }
    }

    fn line(event: Event) -> String {
        event_line(&event, PLAIN).unwrap()
    }

    #[test]
    fn every_event_renders_as_one_line() {
        let session = NativeSession::new("s-2").unwrap();
        let parent = NativeSession::new("s-1").unwrap();
        let cases = [
            (Event::Ready, "harness ready"),
            (
                Event::SessionStarted {
                    session: session.clone(),
                    forked_from: None,
                },
                "session s-2",
            ),
            (
                Event::SessionStarted {
                    session,
                    forked_from: Some(parent),
                },
                "session s-2 forked from s-1",
            ),
            (
                Event::OpenFailed {
                    reason: "not logged in".into(),
                },
                "open failed: not logged in",
            ),
            (
                Event::TurnAccepted {
                    turn: 1,
                    native: None,
                },
                "turn 1 accepted",
            ),
            (
                Event::ToolStarted {
                    turn: 1,
                    call_id: "c".into(),
                    name: "Bash".into(),
                },
                "▸ Bash",
            ),
            (
                Event::PermissionRequested {
                    turn: Some(1),
                    request: request("Bash", json!({"command": "cargo test\n  -q"})),
                },
                "asks Bash: cargo test -q",
            ),
            (
                Event::PermissionRequested {
                    turn: None,
                    request: request("Think", Value::Null),
                },
                "asks Think",
            ),
            (
                Event::PermissionWithdrawn {
                    key: PermissionKey("k9".into()),
                },
                "permission request k9 withdrawn",
            ),
            (
                Event::UsageObserved {
                    turn: Some(1),
                    usage: Usage {
                        cumulative: true,
                        input_tokens: Some(12_345),
                        output_tokens: Some(678),
                        cached_input_tokens: Some(2_000_000),
                        cost_usd: Some(0.0421),
                    },
                },
                "usage $0.04 · 12.3k in · 678 out · 2.0M cached (session)",
            ),
            (
                Event::UsageObserved {
                    turn: None,
                    usage: Usage::default(),
                },
                "usage reported without figures",
            ),
            (
                Event::InterruptAcknowledged { turn: 2 },
                "turn 2 interrupt acknowledged",
            ),
            (
                Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Completed,
                },
                "turn 1 completed",
            ),
            (
                Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Interrupted,
                },
                "turn 1 interrupted",
            ),
            (
                Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Failed {
                        message: "boom".into(),
                    },
                },
                "turn 1 failed: boom",
            ),
            (
                Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::LimitReached {
                        limit: "max turns".into(),
                    },
                },
                "turn 1 stopped at limit: max turns",
            ),
            (
                Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Refused,
                },
                "turn 1 refused",
            ),
            (
                Event::OutcomeUnknown {
                    turn: 3,
                    reason: "closed".into(),
                },
                "turn 3 outcome unknown: closed",
            ),
            (
                Event::UnsupportedRequest {
                    method: "fs/read".into(),
                },
                "unsupported request fs/read",
            ),
            (
                Event::Warning {
                    message: "retrying".into(),
                },
                "warning: retrying",
            ),
            (
                Event::ProtocolViolation {
                    detail: "bad frame".into(),
                },
                "protocol violation: bad frame",
            ),
            (
                Event::Unrecognized {
                    kind: "system/hook".into(),
                },
                "unrecognized system/hook",
            ),
            (Event::SessionClosed, "session closed"),
        ];
        for (event, expected) in cases {
            assert_eq!(line(event), expected);
        }
        assert_eq!(
            event_line(
                &Event::MessageDelta {
                    turn: 1,
                    text: "hi".into()
                },
                PLAIN
            ),
            None
        );
    }

    #[test]
    fn color_wraps_only_when_enabled() {
        let color = Style { color: true };
        assert_eq!(color.paint(Tone::Red, "x"), "\x1b[31mx\x1b[0m");
        assert_eq!(color.paint(Tone::Red, ""), "");
        assert_eq!(PLAIN.paint(Tone::Red, "x"), "x");
        let tool = Event::ToolStarted {
            turn: 1,
            call_id: "c".into(),
            name: "Edit".into(),
        };
        assert_eq!(event_line(&tool, color).unwrap(), "\x1b[36m▸\x1b[0m Edit");
    }

    #[test]
    fn decisions_render() {
        assert_eq!(decision_line("Bash", true, None, PLAIN), "allowed Bash");
        assert_eq!(
            decision_line("Bash", false, Some("Denied by Branchyard policy."), PLAIN),
            "denied Bash: Denied by Branchyard policy."
        );
        assert_eq!(decision_line("Bash", false, None, PLAIN), "denied Bash");
    }

    #[test]
    fn engine_activity_renders_as_lines() {
        let decision = Activity::Decision {
            tool: "Bash".into(),
            allowed: false,
            message: Some("no".into()),
            source: branchyard::DecisionSource::Default,
        };
        let cases = [
            (decision, "denied Bash: no"),
            (
                Activity::Prompt("fix  the\ntest".into()),
                "prompt: fix the test",
            ),
            (
                Activity::Snapshot(CandidateInfo {
                    commit: "0123456789abcdef".into(),
                    files_changed: 2,
                    insertions: 3,
                    deletions: 1,
                }),
                "candidate 0123456789 (2 files +3 -1)",
            ),
            (Activity::Status(BranchStatus::Ready), "status: ready"),
            (
                Activity::Warning("processes outlived the harness".into()),
                "warning: processes outlived the harness",
            ),
            (Activity::Harness(Event::Ready), "harness ready"),
            (
                Activity::Delegation {
                    tool: "spawn".into(),
                    branch: "kid".into(),
                    outcome: "started".into(),
                    refused: false,
                },
                "delegated: spawn kid: started",
            ),
            (
                Activity::Delegation {
                    tool: "spawn".into(),
                    branch: String::new(),
                    outcome: "denied: over budget".into(),
                    refused: true,
                },
                "refused: spawn: denied: over budget",
            ),
        ];
        for (activity, expected) in cases {
            assert_eq!(activity_line(&activity, PLAIN).as_deref(), Some(expected));
        }
        let mut r = Renderer::new(PLAIN, false);
        assert_eq!(r.activity("b", &Activity::Prompt("x".into())), "");
        assert_eq!(r.activity("b", &Activity::Status(BranchStatus::Ready)), "");
        assert_eq!(
            r.activity("b", &Activity::Warning("w".into())),
            "warning: w\n"
        );
    }

    #[test]
    fn compact_input_prefers_recognizable_fields() {
        assert_eq!(
            compact_input(&json!({"file_path": "src/lib.rs", "content": "x"})),
            "src/lib.rs"
        );
        assert_eq!(compact_input(&json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(compact_input(&json!("ls")), "ls");
        let long = compact_input(&json!({ "command": "x".repeat(200) }));
        assert_eq!(long.chars().count(), 80);
        assert!(long.ends_with('…'));
    }

    fn delta(text: &str) -> Event {
        Event::MessageDelta {
            turn: 1,
            text: text.into(),
        }
    }

    #[test]
    fn unprefixed_text_streams_and_breaks_before_lines() {
        let mut r = Renderer::new(PLAIN, false);
        let mut out = r.event("a", &delta("Hel"));
        out += &r.event("a", &delta("lo"));
        out += &r.event("a", &Event::SessionClosed);
        out += &r.event("a", &delta("done\n"));
        out += &r.event("a", &Event::SessionClosed);
        out += &r.event("a", &delta("tail"));
        out += &r.finish();
        assert_eq!(out, "Hello\nsession closed\ndone\nsession closed\ntail\n");
        assert_eq!(r.finish(), "");
    }

    #[test]
    fn prefixed_text_is_emitted_in_whole_lines_per_branch() {
        let mut r = Renderer::new(PLAIN, true);
        let mut out = r.event("x-codex", &delta("one "));
        out += &r.event("x-claude-code", &delta("alpha\nbe"));
        out += &r.event("x-codex", &delta("two\n"));
        out += &r.event("x-claude-code", &Event::Ready);
        out += &r.event("x-codex", &delta("left"));
        out += &r.finish();
        assert_eq!(
            out,
            "x-claude-code │ alpha\n\
             x-codex       │ one two\n\
             x-claude-code │ be\n\
             x-claude-code │ harness ready\n\
             x-codex       │ left\n"
        );
    }

    #[test]
    fn reserved_branches_line_up_from_the_first_line() {
        let mut r = Renderer::new(PLAIN, true);
        r.reserve(&["a".into(), "longer-name".into()]);
        assert_eq!(r.line("a", "x"), "a           │ x\n");
        let mut color = Renderer::new(Style { color: true }, true);
        color.reserve(&["b".into(), "a".into()]);
        assert!(color.line("a", "x").starts_with("\x1b[35m"));
    }

    #[test]
    fn branches_get_distinct_colors() {
        let mut r = Renderer::new(Style { color: true }, true);
        let a = r.line("a", "x");
        let b = r.line("b", "x");
        assert!(a.starts_with("\x1b[36m"), "{a:?}");
        assert!(b.starts_with("\x1b[35m"), "{b:?}");
        assert_eq!(r.line("a", "x"), a);
    }

    #[test]
    fn tables_align_truncate_and_trim() {
        let columns = [
            Column {
                header: "NAME",
                max: 6,
                right: false,
            },
            Column {
                header: "N",
                max: 5,
                right: true,
            },
            Column {
                header: "NOTE",
                max: 10,
                right: false,
            },
        ];
        let rows = vec![
            vec![Cell::plain("short"), Cell::plain("1"), Cell::plain("")],
            vec![
                Cell::plain("much-too-long"),
                Cell::plain("123"),
                Cell::plain("ok"),
            ],
        ];
        assert_eq!(
            table(&columns, &rows, PLAIN),
            "NAME      N  NOTE\n\
             short     1\n\
             much-…  123  ok\n"
        );
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
    }

    #[test]
    fn colored_tables_pad_by_visible_width() {
        let columns = [
            Column {
                header: "S",
                max: 10,
                right: false,
            },
            Column {
                header: "X",
                max: 10,
                right: false,
            },
        ];
        let rows = vec![vec![Cell::toned("ok", Tone::Green), Cell::plain("y")]];
        let text = table(&columns, &rows, Style { color: true });
        assert_eq!(text.lines().nth(1).unwrap(), "\x1b[32mok\x1b[0m  y");
    }

    #[test]
    fn ls_shows_forks_under_their_parents() {
        let mut root = info("flaky", None);
        root.candidate = Some(CandidateInfo {
            commit: "abc".into(),
            files_changed: 3,
            insertions: 12,
            deletions: 4,
        });
        root.cost_usd = Some(0.42);
        let mut other = info("docs", None);
        other.status = BranchStatus::Failed {
            reason: "harness exited".into(),
        };
        other.created_at = 10_000 - 7_200;
        let fork = info("flaky-alt", Some("flaky"));
        let grandchild = info("flaky-alt-2", Some("flaky-alt"));
        let orphan = info("orphan", Some("removed"));
        let infos = [root, other, grandchild, fork, orphan];
        let order: Vec<(usize, &str)> = tree_order(&infos)
            .into_iter()
            .map(|(d, i)| (d, i.name.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                (0, "flaky"),
                (1, "flaky-alt"),
                (2, "flaky-alt-2"),
                (0, "docs"),
                (0, "orphan")
            ]
        );
        assert_eq!(
            branch_table(&infos, 10_030, PLAIN),
            "BRANCH           HARNESS      STATUS                  CANDIDATE       TURNS   COST  AGE\n\
             flaky            claude-code  ready                   3 files +12 -4      1  $0.42  30s\n\
             └ flaky-alt      claude-code  ready                   -                   1      -  30s\n\
             \x20 └ flaky-alt-2  claude-code  ready                   -                   1      -  30s\n\
             docs             claude-code  failed: harness exited  -                   1      -   2h\n\
             orphan           claude-code  ready                   -                   1      -  30s\n"
        );
    }

    #[test]
    fn status_candidate_cost_and_age_texts() {
        let merged = BranchStatus::Merged {
            target: "main".into(),
            commit: "c".into(),
        };
        assert_eq!(status_text(&merged).0, "merged into main");
        assert_eq!(
            status_text(&BranchStatus::BudgetExceeded {
                limit: "$2.00".into()
            })
            .0,
            "over budget: $2.00"
        );
        assert_eq!(status_text(&BranchStatus::NoChanges).0, "no changes");
        let one = CandidateInfo {
            commit: "c".into(),
            files_changed: 1,
            insertions: 0,
            deletions: 2,
        };
        assert_eq!(candidate_text(Some(&one)), "1 file +0 -2");
        assert_eq!(cost_text(Some(0.004)), "$0.0040");
        assert_eq!(cost_text(Some(12.0)), "$12.00");
        assert_eq!(cost_text(None), "-");
        assert_eq!(
            [5, 42, 600, 7_200, 200_000].map(age_text),
            ["now", "42s", "10m", "2h", "2d"]
        );
    }

    #[test]
    fn harness_table_marks_default_and_qualification() {
        let harnesses = [
            HarnessInfo {
                harness: "claude-code".into(),
                profile: "claude-code-stream-json".into(),
                default: true,
                available: true,
                qualification: Some("9/9 on Claude Code 2.1.283".into()),
            },
            HarnessInfo {
                harness: "codex".into(),
                profile: "codex-app-server".into(),
                default: false,
                available: false,
                qualification: None,
            },
        ];
        assert_eq!(
            harness_table(&harnesses, PLAIN),
            "HARNESS      PROFILE                  DEFAULT  ON PATH  QUALIFICATION\n\
             claude-code  claude-code-stream-json  *        yes      9/9 on Claude Code 2.1.283\n\
             codex        codex-app-server                  no       unqualified\n"
        );
    }

    #[test]
    fn summary_ends_with_next_commands() {
        let mut ready = info("fix it", None);
        ready.candidate = Some(CandidateInfo {
            commit: "0123456789abcdef".into(),
            files_changed: 2,
            insertions: 5,
            deletions: 1,
        });
        ready.cost_usd = Some(0.5);
        ready.turns = 2;
        assert_eq!(
            summary(&ready, PLAIN),
            "branch     fix it (by/fix it)\n\
             status     ready\n\
             candidate  2 files +5 -1 at 0123456789\n\
             cost       $0.50 over 2 turns\n\
             next       by diff 'fix it'\n\
             \x20          by merge 'fix it'\n"
        );
        let mut failed = info("f", None);
        failed.status = BranchStatus::Failed { reason: "x".into() };
        assert_eq!(next_commands(&failed), ["by log f", "by rm f"]);
        let mut none = info("n", None);
        none.status = BranchStatus::NoChanges;
        assert_eq!(next_commands(&none), ["by send n \"<prompt>\"", "by rm n"]);
    }

    #[test]
    fn details_include_fork_parent_and_session() {
        let mut fork = info("alt", Some("flaky"));
        fork.session = Some("s-9".into());
        let text = details(&fork, 10_000 + 120, PLAIN, Vec::new());
        assert!(text.contains("forked from  flaky\n"), "{text}");
        assert!(text.contains("session      s-9\n"));
        assert!(text.contains("base         0123456789\n"));
        assert!(text.contains("created      2m ago\n"));
        assert!(text.contains("cost         not reported, 1 turn\n"));
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(951_827_696_007), "2000-02-29T12:34:56.007Z");
        assert_eq!(timestamp(1_790_380_800_000), "2026-09-26T00:00:00.000Z");
    }

    #[test]
    fn log_joins_message_deltas() {
        let at = |at_ms, event| RecordedEvent {
            at_ms,
            activity: Activity::Harness(event),
        };
        let events = [
            at(0, Event::Ready),
            at(1, delta("Looking at ")),
            at(2, delta("the test.\nFound it.\n")),
            at(3, Event::SessionClosed),
            RecordedEvent {
                at_ms: 4,
                activity: Activity::Decision {
                    tool: "Bash".into(),
                    allowed: true,
                    message: None,
                    source: branchyard::DecisionSource::Asked,
                },
            },
        ];
        assert_eq!(
            log_text(&events, PLAIN),
            "1970-01-01T00:00:00.000Z  harness ready\n\
             1970-01-01T00:00:00.001Z  Looking at the test.\n\
             \x20                         Found it.\n\
             1970-01-01T00:00:00.003Z  session closed\n\
             1970-01-01T00:00:00.004Z  allowed Bash\n"
        );
    }
}
