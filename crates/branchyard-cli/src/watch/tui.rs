//! `by watch`'s live dashboard, in the Elm shape: a [`Model`] of what is
//! known, [`update`] folding a [`Msg`] (a key, or a fresh [`Snapshot`] of
//! the branches and their new events) into it, and [`view`] drawing it with
//! ratatui. Neither `update` nor `view` does I/O, so both are tested
//! directly, `view` against ratatui's `TestBackend`. [`run`] is the only
//! part that touches the terminal (through crossterm) or the data source.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use branchyard::{Activity, BranchInfo, BranchStatus, Event, RecordedEvent};
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;

use super::Doing;
use crate::render;

/// Recent events kept per branch for the detail pane.
const RECENT: usize = 50;

/// Rows PageUp and PageDown move.
const PAGE: usize = 10;

/// A refresh from the data source.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Where the branches come from: a repository path or a server.
    pub label: String,
    pub infos: Vec<BranchInfo>,
    /// Events recorded since the previous snapshot, by branch.
    pub events: Vec<(String, RecordedEvent)>,
    pub now_ms: u64,
}

/// A key, as the dashboard reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    /// Ctrl-C or Ctrl-D: quit from anywhere.
    Interrupt,
}

impl Key {
    /// A crossterm key press; `None` for releases, repeats of nothing we
    /// bind, and keys we do not bind.
    pub fn from_crossterm(key: KeyEvent) -> Option<Key> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        Some(match key.code {
            KeyCode::Char('c' | 'd') if control => Key::Interrupt,
            KeyCode::Char(c) if !control => Key::Char(c),
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Backspace => Key::Backspace,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum Msg {
    Key(Key),
    Refreshed(Snapshot),
}

/// Whether the loop goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// What keys do right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Browse,
    /// Typing a filter after `/`.
    Filter,
    /// The `?` sheet is open.
    Help,
}

/// What the detail pane shows beyond [`BranchInfo`], from the events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detail {
    /// The latest events, oldest first, as (milliseconds, line, tone).
    pub recent: VecDeque<(u64, String, Tone)>,
    /// Inbox messages sent to this branch and not yet delivered to it.
    pub unread: BTreeSet<u64>,
    pub tokens: Tokens,
}

/// Token counts the harness reported, summed over turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    /// The last cumulative (whole-session) report, which replaces rather
    /// than adds to what came before it.
    session: (u64, u64, u64),
}

impl Tokens {
    fn apply(&mut self, usage: &branchyard::Usage) {
        let now = (
            usage.input_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
            usage.cached_input_tokens.unwrap_or(0),
        );
        let delta = match usage.cumulative {
            true => {
                let before = self.session;
                self.session = now;
                (
                    now.0.saturating_sub(before.0),
                    now.1.saturating_sub(before.1),
                    now.2.saturating_sub(before.2),
                )
            }
            false => now,
        };
        self.input += delta.0;
        self.output += delta.1;
        self.cached += delta.2;
    }

    pub fn any(&self) -> bool {
        self.input + self.output + self.cached > 0
    }
}

/// How an event line is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Good,
    Warn,
    Bad,
    Accent,
}

/// Everything the dashboard knows.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub label: String,
    pub infos: Vec<BranchInfo>,
    pub doing: HashMap<String, Doing>,
    pub detail: HashMap<String, Detail>,
    /// The selected branch, by name, so it stays put as rows come and go.
    pub selected: Option<String>,
    pub mode: Mode,
    pub filter: String,
    /// The branch whose subtree alone is shown, after Enter.
    pub focus: Option<String>,
    pub now_ms: u64,
    /// Refreshes applied; 0 until the first data arrives.
    pub refreshes: u64,
}

/// One row of the tree as shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Visible {
    pub index: usize,
    pub depth: usize,
    /// Kept only as an ancestor of a row the filter matches.
    pub context: bool,
}

impl Model {
    pub fn new(label: impl Into<String>) -> Model {
        Model {
            label: label.into(),
            ..Model::default()
        }
    }

    fn info(&self, name: &str) -> Option<&BranchInfo> {
        self.infos.iter().find(|info| info.name == name)
    }

    /// The rows to show: the tree (or the focused subtree), narrowed by
    /// the filter to matching branches and their ancestors.
    pub fn visible(&self) -> Vec<Visible> {
        let order: Vec<(usize, &BranchInfo)> = render::tree_order(&self.infos);
        let index_of: HashMap<&str, usize> = self
            .infos
            .iter()
            .enumerate()
            .map(|(i, info)| (info.name.as_str(), i))
            .collect();
        let mut rows: Vec<(usize, &BranchInfo)> = match &self.focus {
            None => order,
            Some(focus) => {
                let start = order.iter().position(|(_, info)| &info.name == focus);
                match start {
                    None => order,
                    Some(start) => {
                        let base = order[start].0;
                        let end = order[start + 1..]
                            .iter()
                            .position(|(depth, _)| *depth <= base)
                            .map_or(order.len(), |i| start + 1 + i);
                        order[start..end]
                            .iter()
                            .map(|(depth, info)| (depth - base, *info))
                            .collect()
                    }
                }
            }
        };
        let needle = self.filter.trim().to_lowercase();
        if needle.is_empty() {
            return rows
                .into_iter()
                .map(|(depth, info)| Visible {
                    index: index_of[info.name.as_str()],
                    depth,
                    context: false,
                })
                .collect();
        }
        let matches = |info: &BranchInfo| {
            let (status, _) = render::status_text(&info.status);
            [&info.name, &info.harness, &status, &info.prompt]
                .iter()
                .any(|field| field.to_lowercase().contains(&needle))
        };
        let matched: HashSet<&str> = rows
            .iter()
            .filter(|(_, info)| matches(info))
            .map(|(_, info)| info.name.as_str())
            .collect();
        // Keep each match's ancestors (as context), so the tree still reads.
        let mut keep: HashSet<&str> = matched.clone();
        for name in &matched {
            let mut up = self.info(name).and_then(|info| info.parent.as_deref());
            while let Some(parent) = up {
                if !keep.insert(parent) {
                    break;
                }
                up = self.info(parent).and_then(|info| info.parent.as_deref());
            }
        }
        rows.retain(|(_, info)| keep.contains(info.name.as_str()));
        rows.into_iter()
            .map(|(depth, info)| Visible {
                index: index_of[info.name.as_str()],
                depth,
                context: !matched.contains(info.name.as_str()),
            })
            .collect()
    }

    /// The selected row's position among `rows`.
    pub fn cursor(&self, rows: &[Visible]) -> Option<usize> {
        let name = self.selected.as_deref()?;
        rows.iter()
            .position(|row| self.infos[row.index].name == name)
    }

    /// Keep the selection on a shown branch: the same one if it is still
    /// shown, else the row at the same position, else the first.
    fn settle(&mut self, previous: Option<usize>) {
        let rows = self.visible();
        if self.cursor(&rows).is_some() {
            return;
        }
        let at = previous.unwrap_or(0).min(rows.len().saturating_sub(1));
        self.selected = rows.get(at).map(|row| self.infos[row.index].name.clone());
    }

    fn step(&mut self, by: isize) {
        let rows = self.visible();
        if rows.is_empty() {
            return;
        }
        let at = self.cursor(&rows).unwrap_or(0) as isize;
        let next = (at + by).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = Some(self.infos[rows[next].index].name.clone());
    }

    fn apply(&mut self, snapshot: Snapshot) {
        let rows = self.visible();
        let previous = self.cursor(&rows);
        self.label = snapshot.label;
        self.now_ms = snapshot.now_ms;
        self.infos = snapshot.infos;
        for (branch, event) in snapshot.events {
            self.doing.entry(branch.clone()).or_default().apply(&event);
            let detail = self.detail.entry(branch.clone()).or_default();
            match &event.activity {
                Activity::Harness(Event::UsageObserved { usage, .. }) => detail.tokens.apply(usage),
                Activity::Message(message) if message.to == branch && !message.delivered => {
                    detail.unread.insert(message.id);
                }
                Activity::Message(message) if message.to == branch => {
                    detail.unread.remove(&message.id);
                }
                Activity::MessagesDelivered { ids, .. } => {
                    for id in ids {
                        detail.unread.remove(id);
                    }
                }
                _ => {}
            }
            if let Some(line) =
                render::activity_line(&event.activity, render::Style { color: false })
            {
                detail
                    .recent
                    .push_back((event.at_ms, line, tone_of(&event.activity)));
                while detail.recent.len() > RECENT {
                    detail.recent.pop_front();
                }
            }
        }
        let names: HashSet<String> = self.infos.iter().map(|i| i.name.clone()).collect();
        self.doing.retain(|name, _| names.contains(name));
        self.detail.retain(|name, _| names.contains(name));
        if self.focus.as_ref().is_some_and(|f| !names.contains(f)) {
            self.focus = None;
        }
        self.refreshes += 1;
        self.settle(previous);
    }
}

/// Fold one message into the model.
pub fn update(model: &mut Model, msg: Msg) -> Flow {
    let key = match msg {
        Msg::Refreshed(snapshot) => {
            model.apply(snapshot);
            return Flow::Continue;
        }
        Msg::Key(key) => key,
    };
    if key == Key::Interrupt {
        return Flow::Quit;
    }
    match model.mode {
        Mode::Help => {
            // Any key closes the sheet; q still quits.
            model.mode = Mode::Browse;
            if key == Key::Char('q') {
                return Flow::Quit;
            }
        }
        Mode::Filter => {
            let rows = model.visible();
            let previous = model.cursor(&rows);
            match key {
                Key::Enter => model.mode = Mode::Browse,
                Key::Esc => {
                    model.filter.clear();
                    model.mode = Mode::Browse;
                }
                Key::Backspace => {
                    model.filter.pop();
                }
                Key::Char(c) => model.filter.push(c),
                Key::Up => model.step(-1),
                Key::Down => model.step(1),
                _ => {}
            }
            model.settle(previous);
        }
        Mode::Browse => match key {
            Key::Char('q') => return Flow::Quit,
            Key::Esc => {
                // Esc backs out one level: the filter, then the focus, then
                // the dashboard.
                if !model.filter.is_empty() {
                    model.filter.clear();
                } else if model.focus.is_some() {
                    model.focus = None;
                } else {
                    return Flow::Quit;
                }
                model.settle(None);
            }
            Key::Char('?') => model.mode = Mode::Help,
            Key::Char('/') => model.mode = Mode::Filter,
            Key::Char('j') | Key::Down => model.step(1),
            Key::Char('k') | Key::Up => model.step(-1),
            Key::PageDown => model.step(PAGE as isize),
            Key::PageUp => model.step(-(PAGE as isize)),
            Key::Char('g') | Key::Home => model.step(isize::MIN / 2),
            Key::Char('G') | Key::End => model.step(isize::MAX / 2),
            Key::Enter | Key::Char('l') | Key::Right => {
                if let Some(name) = model.selected.clone() {
                    model.focus = match model.focus.as_deref() == Some(name.as_str()) {
                        // Enter on the focused branch goes back out.
                        true => None,
                        false => Some(name),
                    };
                }
            }
            Key::Backspace | Key::Char('h') | Key::Left => {
                // Out one level: focus the focused branch's parent.
                if let Some(focus) = model.focus.clone() {
                    model.focus = model.info(&focus).and_then(|info| info.parent.clone());
                    model.selected = Some(focus);
                }
            }
            _ => {}
        },
    }
    Flow::Continue
}

fn tone_of(activity: &Activity) -> Tone {
    use branchyard::TurnOutcome;
    match activity {
        Activity::Status(BranchStatus::Interrupted) => Tone::Warn,
        Activity::Status(BranchStatus::Failed { .. } | BranchStatus::Blocked { .. }) => Tone::Bad,
        Activity::Status(BranchStatus::Ready | BranchStatus::Merged { .. }) => Tone::Good,
        Activity::Status(_) => Tone::Plain,
        Activity::Prompt(_) | Activity::Message(_) | Activity::Steered { .. } => Tone::Accent,
        Activity::Decision { allowed: true, .. } | Activity::Snapshot(_) => Tone::Good,
        Activity::Decision { allowed: false, .. } => Tone::Bad,
        Activity::Warning(_) | Activity::Stalled { .. } | Activity::Recovered { .. } => Tone::Warn,
        Activity::Delegation { refused: true, .. } => Tone::Bad,
        Activity::Harness(event) => match event {
            Event::TurnEnded { outcome, .. } => match outcome {
                TurnOutcome::Completed => Tone::Good,
                TurnOutcome::Failed { .. } => Tone::Bad,
                _ => Tone::Warn,
            },
            Event::PermissionRequested { .. } | Event::Warning { .. } => Tone::Warn,
            Event::OpenFailed { .. }
            | Event::OutcomeUnknown { .. }
            | Event::ProtocolViolation { .. } => Tone::Bad,
            Event::ToolStarted { .. } => Tone::Plain,
            _ => Tone::Dim,
        },
        _ => Tone::Dim,
    }
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::new(),
        Tone::Dim => Style::new().fg(Color::DarkGray),
        Tone::Good => Style::new().fg(Color::Green),
        Tone::Warn => Style::new().fg(Color::Yellow),
        Tone::Bad => Style::new().fg(Color::Red),
        Tone::Accent => Style::new().fg(Color::Cyan),
    }
}

/// A status as a glyph, a label and a style. Interrupted is drawn black on
/// yellow so it cannot be missed: an interrupted branch holds work nobody
/// is finishing.
pub fn status_look(info: &BranchInfo) -> (&'static str, String, Style) {
    let (text, _) = render::status_text(&info.status);
    let (glyph, style) = status_style(&info.status);
    let text = match (&info.status, info.stalled) {
        (BranchStatus::Interrupted, _) => "INTERRUPTED".to_owned(),
        (_, true) => format!("{text} (stalled)"),
        _ => text,
    };
    (glyph, text, style)
}

fn status_style(status: &BranchStatus) -> (&'static str, Style) {
    match status {
        BranchStatus::Running => ("●", Style::new().fg(Color::Cyan)),
        BranchStatus::Waiting => ("◌", Style::new().fg(Color::DarkGray)),
        BranchStatus::Blocked { .. } => ("■", Style::new().fg(Color::Magenta)),
        BranchStatus::Ready => ("✔", Style::new().fg(Color::Green)),
        BranchStatus::NoChanges => ("○", Style::new().fg(Color::DarkGray)),
        BranchStatus::Interrupted => (
            "⚠",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        BranchStatus::BudgetExceeded { .. } => ("$", Style::new().fg(Color::Yellow)),
        BranchStatus::Failed { .. } => (
            "✖",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        BranchStatus::Merged { .. } => ("◆", Style::new().fg(Color::Blue)),
    }
}

/// Tree guides for rows at `depths` (a pre-order walk): `├─ ` and `└─ `
/// before each non-root, `│ ` where an ancestor has siblings still to come.
pub fn guides(depths: &[usize]) -> Vec<String> {
    let last: Vec<bool> = (0..depths.len())
        .map(|i| {
            depths[i + 1..]
                .iter()
                .take_while(|d| **d >= depths[i])
                .all(|d| *d != depths[i])
        })
        .collect();
    let mut open: Vec<bool> = Vec::new();
    depths
        .iter()
        .zip(&last)
        .map(|(&depth, &last)| {
            if depth == 0 {
                open.clear();
                return String::new();
            }
            open.truncate(depth - 1);
            open.resize(depth - 1, false);
            let mut text: String = open.iter().map(|o| if *o { "│ " } else { "  " }).collect();
            text.push_str(if last { "└─ " } else { "├─ " });
            open.push(!last);
            text
        })
        .collect()
}

const HELP: &[(&str, &str)] = &[
    ("j / ↓, k / ↑", "move"),
    ("PgDn, PgUp", "move ten rows"),
    ("g / Home, G / End", "first, last"),
    (
        "Enter / l / →",
        "focus the branch's subtree (again to leave)",
    ),
    ("Backspace / h / ←", "focus the parent's subtree"),
    ("/", "filter by name, harness, status or prompt"),
    ("Esc", "clear the filter, then unfocus, then quit"),
    ("?", "this sheet"),
    ("q, Ctrl-C", "quit"),
];

/// Draw the whole dashboard.
pub fn view(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header_line(model)), header);
    frame.render_widget(Paragraph::new(footer_line(model)), footer);

    let rows = model.visible();
    let cursor = model.cursor(&rows);
    let selected = cursor.map(|at| &model.infos[rows[at].index]);
    // Side by side when there is room, else the detail below the tree, else
    // the tree alone.
    let (tree, detail) = if body.width >= 110 {
        let [tree, detail] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(body);
        (tree, Some(detail))
    } else if body.height >= 18 {
        // The tree takes what its rows need (borders and header too), up
        // to 55%; the detail gets the rest.
        let needed = rows.len().max(1) as u16 + 3;
        let [tree, detail] = Layout::vertical([
            Constraint::Length(needed.min(body.height * 55 / 100)),
            Constraint::Fill(1),
        ])
        .areas(body);
        (tree, Some(detail))
    } else {
        (body, None)
    };
    draw_tree(model, &rows, cursor, frame, tree);
    if let Some(area) = detail {
        draw_detail(model, selected, frame, area);
    }
    if model.mode == Mode::Help {
        draw_help(frame, area);
    }
}

fn header_line(model: &Model) -> Line<'static> {
    let infos = &model.infos;
    let count = |f: fn(&BranchStatus) -> bool| infos.iter().filter(|i| f(&i.status)).count();
    let running = count(|s| matches!(s, BranchStatus::Running));
    let interrupted = count(|s| matches!(s, BranchStatus::Interrupted));
    let failed = count(|s| matches!(s, BranchStatus::Failed { .. }));
    let blocked = count(|s| matches!(s, BranchStatus::Blocked { .. }));
    let cost: f64 = infos.iter().filter_map(|i| i.cost_usd).sum();
    let mut spans = vec![
        Span::styled("by watch", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}  ", model.label),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw(format!(
            "{} {}",
            infos.len(),
            if infos.len() == 1 {
                "branch"
            } else {
                "branches"
            }
        )),
        Span::styled(
            format!(" · {running} running"),
            Style::new().fg(Color::Cyan),
        ),
    ];
    if interrupted > 0 {
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!(" ⚠ {interrupted} interrupted "),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if failed > 0 {
        spans.push(Span::styled(
            format!(" · {failed} failed"),
            Style::new().fg(Color::Red),
        ));
    }
    if blocked > 0 {
        spans.push(Span::styled(
            format!(" · {blocked} blocked"),
            Style::new().fg(Color::Magenta),
        ));
    }
    if infos.iter().any(|i| i.cost_usd.is_some()) {
        spans.push(Span::raw(format!(" · {} reported", render::usd(cost))));
    }
    Line::from(spans)
}

fn footer_line(model: &Model) -> Line<'static> {
    let key = |k: &str| Span::styled(k.to_owned(), Style::new().fg(Color::Black).bg(Color::Gray));
    let text = |t: &str| Span::styled(format!(" {t}  "), Style::new().fg(Color::DarkGray));
    match model.mode {
        Mode::Filter => Line::from(vec![
            Span::styled("/", Style::new().fg(Color::Yellow)),
            Span::raw(model.filter.clone()),
            Span::styled("▏", Style::new().fg(Color::Yellow)),
            text(""),
            key("Enter"),
            text("keep"),
            key("Esc"),
            text("clear"),
        ]),
        _ => {
            let mut spans = vec![
                key("j/k"),
                text("move"),
                key("Enter"),
                text(if model.focus.is_some() {
                    "unfocus"
                } else {
                    "focus"
                }),
                key("/"),
                text("filter"),
                key("?"),
                text("help"),
                key("q"),
                text("quit"),
            ];
            if let Some(focus) = &model.focus {
                spans.push(Span::styled(
                    format!("focus: {focus}  "),
                    Style::new().fg(Color::Cyan),
                ));
            }
            if !model.filter.is_empty() {
                spans.push(Span::styled(
                    format!("filter: {}", model.filter),
                    Style::new().fg(Color::Yellow),
                ));
            }
            Line::from(spans)
        }
    }
}

fn draw_tree(
    model: &Model,
    rows: &[Visible],
    cursor: Option<usize>,
    frame: &mut Frame,
    area: Rect,
) {
    let title = match &model.focus {
        Some(focus) => format!(" branches under {focus} "),
        None => " branches ".to_owned(),
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::DarkGray));
    if rows.is_empty() {
        let text = match (model.refreshes, model.infos.is_empty()) {
            (0, _) => "loading…",
            (_, true) => "no branches yet; start one with: by run \"<prompt>\"",
            (_, false) => "no branch matches the filter",
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::new().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }
    let depths: Vec<usize> = rows.iter().map(|row| row.depth).collect();
    let guides = guides(&depths);
    let now_s = model.now_ms / 1000;
    // Columns drop from the right as the tree narrows: activity, then age
    // and turns, then harness.
    let inner = area.width.saturating_sub(2);
    let wide = inner >= 72;
    let medium = inner >= 52;
    let table_rows: Vec<Row> = rows
        .iter()
        .zip(&guides)
        .map(|(row, guide)| {
            let info = &model.infos[row.index];
            let (glyph, status, look) = status_look(info);
            let name_style = match (row.context, &info.status) {
                (true, _) => Style::new().fg(Color::DarkGray),
                (false, BranchStatus::Interrupted) => {
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                }
                _ => Style::new(),
            };
            let unread = model
                .detail
                .get(&info.name)
                .map_or(0, |detail| detail.unread.len());
            let mut name = vec![
                Span::styled(guide.clone(), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("{glyph} "), look),
                Span::styled(info.name.clone(), name_style),
            ];
            if unread > 0 {
                name.push(Span::styled(
                    format!(" ✉{unread}"),
                    Style::new().fg(Color::Cyan),
                ));
            }
            let mut cells = vec![
                Cell::from(Line::from(name)),
                Cell::from(Span::styled(status, look)),
            ];
            if medium {
                cells.push(Cell::from(info.harness.clone()));
            }
            if wide {
                cells.push(Cell::from(
                    Line::from(info.turns.to_string()).right_aligned(),
                ));
            }
            cells.push(Cell::from(
                Line::from(render::cost_text(info.cost_usd)).right_aligned(),
            ));
            if wide {
                cells.push(Cell::from(
                    Line::from(render::age_text(now_s.saturating_sub(info.created_at)))
                        .right_aligned(),
                ));
                let activity = model
                    .doing
                    .get(&info.name)
                    .map(Doing::summary)
                    .unwrap_or_default();
                cells.push(Cell::from(Span::styled(
                    activity,
                    Style::new().fg(Color::DarkGray),
                )));
            }
            Row::new(cells)
        })
        .collect();
    let name_width = rows
        .iter()
        .zip(&guides)
        .map(|(row, guide)| {
            let info = &model.infos[row.index];
            let unread = model
                .detail
                .get(&info.name)
                .map_or(0, |detail| detail.unread.len());
            guide.chars().count()
                + 2
                + info.name.chars().count()
                + if unread > 0 {
                    3 + unread.to_string().len()
                } else {
                    0
                }
        })
        .max()
        .unwrap_or(6)
        .clamp(6, 40) as u16;
    let mut widths = vec![Constraint::Length(name_width), Constraint::Length(14)];
    let mut header = vec!["BRANCH", "STATUS"];
    if medium {
        widths.push(Constraint::Length(12));
        header.push("HARNESS");
    }
    if wide {
        widths.push(Constraint::Length(5));
        header.push("TURNS");
    }
    widths.push(Constraint::Length(8));
    header.push("    COST");
    if wide {
        widths.push(Constraint::Length(4));
        header.push(" AGE");
        widths.push(Constraint::Fill(1));
        header.push("ACTIVITY");
    }
    let table = Table::new(table_rows, widths)
        .header(Row::new(header).style(Style::new().fg(Color::DarkGray)))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always)
        .block(block);
    let mut state = TableState::default().with_selected(cursor);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_detail(model: &Model, info: Option<&BranchInfo>, frame: &mut Frame, area: Rect) {
    let block = Block::bordered().border_style(Style::new().fg(Color::DarkGray));
    let Some(info) = info else {
        frame.render_widget(block.title(" detail "), area);
        return;
    };
    let (glyph, status, look) = status_look(info);
    let block = block.title(Line::from(vec![
        Span::raw(" "),
        Span::styled(info.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ]));
    let detail = model.detail.get(&info.name);
    let label = |text: &str| Span::styled(format!("{text:<9}"), Style::new().fg(Color::DarkGray));
    let mut lines = vec![Line::from(vec![
        label("status"),
        Span::styled(format!("{glyph} {status}"), look),
    ])];
    let next = render::next_commands(info).join(" · ");
    lines.push(Line::from(vec![
        label("next"),
        Span::styled(
            next,
            match info.status {
                // An interrupted branch holds work nobody is finishing.
                BranchStatus::Interrupted => Style::new().fg(Color::Yellow),
                _ => Style::new(),
            },
        ),
    ]));
    lines.push(Line::from(vec![
        label("harness"),
        Span::raw(format!("{} ({})", info.harness, info.profile)),
    ]));
    if let Some(parent) = &info.parent {
        lines.push(Line::from(vec![label("parent"), Span::raw(parent.clone())]));
    }
    let mut cost = format!(
        "{} · {} turn{}",
        render::cost_text(info.cost_usd),
        info.turns,
        if info.turns == 1 { "" } else { "s" }
    );
    if let Some(tokens) = detail.map(|d| d.tokens).filter(Tokens::any) {
        cost.push_str(&format!(
            " · {} in, {} out",
            render::tokens(tokens.input),
            render::tokens(tokens.output)
        ));
        if tokens.cached > 0 {
            cost.push_str(&format!(", {} cached", render::tokens(tokens.cached)));
        }
    }
    lines.push(Line::from(vec![label("cost"), Span::raw(cost)]));
    let unread = detail.map_or(0, |d| d.unread.len());
    lines.push(Line::from(vec![
        label("inbox"),
        match unread {
            0 => Span::styled("no unread messages", Style::new().fg(Color::DarkGray)),
            n => Span::styled(
                format!("{n} unread message{}", if n == 1 { "" } else { "s" }),
                Style::new().fg(Color::Cyan),
            ),
        },
    ]));
    if !info.children.is_empty() {
        let mut spans = vec![label("children")];
        for (i, child) in info.children.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            match model.info(child) {
                Some(child) => {
                    let (glyph, _, look) = status_look(child);
                    spans.push(Span::styled(format!("{glyph} "), look));
                    spans.push(Span::raw(child.name.clone()));
                }
                None => spans.push(Span::styled(
                    child.clone(),
                    Style::new().fg(Color::DarkGray),
                )),
            }
        }
        lines.push(Line::from(spans));
    }
    let prompt = info.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    lines.push(Line::from(vec![
        label("prompt"),
        Span::raw(render::truncate(&prompt, 240)),
    ]));
    if let Some(summary) = model
        .doing
        .get(&info.name)
        .map(Doing::summary)
        .filter(|s| !s.is_empty())
    {
        lines.push(Line::from(vec![label("now"), Span::raw(summary)]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "latest events",
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )));
    // As many of the latest events as fit, newest last.
    let room = area.height.saturating_sub(2) as usize;
    let fixed = lines.len();
    let recent: Vec<&(u64, String, Tone)> = detail
        .map(|d| d.recent.iter().collect())
        .unwrap_or_default();
    if recent.is_empty() {
        lines.push(Line::from(Span::styled(
            "none yet",
            Style::new().fg(Color::DarkGray),
        )));
    }
    let shown = room.saturating_sub(fixed).max(1).min(recent.len());
    for (at_ms, text, tone) in &recent[recent.len() - shown..] {
        let stamp = render::timestamp(*at_ms);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", stamp.get(11..19).unwrap_or(&stamp)),
                Style::new().fg(Color::DarkGray),
            ),
            Span::styled(text.clone(), tone_style(*tone)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 76.min(area.width);
    let height = (HELP.len() as u16 + 6).min(area.height);
    let popup = area.centered(Constraint::Length(width), Constraint::Length(height));
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:>19}  "), Style::new().fg(Color::Cyan)),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("⚠ INTERRUPTED", status_style(&BranchStatus::Interrupted).1),
        Span::raw(" stopped mid-turn; ✉ counts unread inbox messages"),
    ]));
    lines.push(Line::from(Span::styled(
        "any key closes this sheet",
        Style::new().fg(Color::DarkGray),
    )));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" by watch keys ")),
        popup,
    );
}

/// Run the dashboard until the viewer quits. `refresh` fetches a snapshot;
/// its error ends the dashboard (after the terminal is restored) and is
/// returned.
pub fn run<E>(
    label: String,
    interval: Duration,
    mut refresh: impl FnMut() -> Result<Snapshot, E>,
) -> Result<Result<(), E>, std::io::Error> {
    // Raw mode, the alternate screen, and a panic hook that restores both
    // before the panic is reported. `Restore` restores them on every other
    // way out.
    let mut terminal = ratatui::try_init()?;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ratatui::restore();
        }
    }
    let _restore = Restore;
    let mut model = Model::new(label);
    terminal.draw(|frame| view(&model, frame))?;
    let mut next_refresh = Instant::now();
    loop {
        if Instant::now() >= next_refresh {
            match refresh() {
                Ok(snapshot) => {
                    update(&mut model, Msg::Refreshed(snapshot));
                }
                Err(error) => return Ok(Err(error)),
            }
            next_refresh = Instant::now() + interval;
            terminal.draw(|frame| view(&model, frame))?;
        }
        let wait = next_refresh.saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            match event::read()? {
                event::Event::Key(key) => {
                    if let Some(key) = Key::from_crossterm(key) {
                        if update(&mut model, Msg::Key(key)) == Flow::Quit {
                            return Ok(Ok(()));
                        }
                    }
                }
                // Drawing autoresizes to the new size.
                event::Event::Resize(..) => {}
                _ => continue,
            }
            terminal.draw(|frame| view(&model, frame))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{DeliveredVia, Message, MessageKind, Usage};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn info(name: &str, parent: Option<&str>, status: BranchStatus) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: PathBuf::from("/w"),
            prompt: format!("work on {name}"),
            harness: "codex".into(),
            profile: "codex-app-server".into(),
            session: None,
            parent: parent.map(Into::into),
            children: Vec::new(),
            depth: 0,
            base: "b".into(),
            candidate: None,
            status,
            turns: 2,
            cost_usd: Some(0.5),
            created_at: 1_000,
            stalled: false,
            superseded_by: None,
        }
    }

    fn yard() -> Vec<BranchInfo> {
        let mut lead = info("lead", None, BranchStatus::Running);
        lead.children = vec!["impl".into(), "review".into()];
        vec![
            lead,
            info("impl", Some("lead"), BranchStatus::Interrupted),
            info("review", Some("lead"), BranchStatus::Waiting),
            info("docs", None, BranchStatus::Failed { reason: "x".into() }),
            info(
                "tests",
                Some("impl"),
                BranchStatus::Blocked {
                    reason: "impl".into(),
                },
            ),
        ]
    }

    fn snapshot(infos: Vec<BranchInfo>, events: Vec<(&str, Activity)>) -> Snapshot {
        Snapshot {
            label: "/repo".into(),
            infos,
            events: events
                .into_iter()
                .enumerate()
                .map(|(i, (branch, activity))| {
                    (
                        branch.to_owned(),
                        RecordedEvent {
                            at_ms: 1_130_000 + i as u64 * 1000,
                            activity,
                        },
                    )
                })
                .collect(),
            now_ms: 1_130_000,
        }
    }

    fn model() -> Model {
        let mut model = Model::new("/repo");
        update(&mut model, Msg::Refreshed(snapshot(yard(), Vec::new())));
        model
    }

    fn keys(model: &mut Model, keys: &[Key]) -> Flow {
        let mut flow = Flow::Continue;
        for key in keys {
            flow = update(model, Msg::Key(*key));
        }
        flow
    }

    fn draw(model: &Model, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view(model, frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn lines(buffer: &Buffer) -> Vec<String> {
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Where `needle` starts in the drawn screen, as (x, y) in cells.
    fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let area = buffer.area;
        for y in 0..area.height {
            let cells: Vec<&str> = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
            for x in 0..cells.len() {
                let mut text = String::new();
                for cell in &cells[x..] {
                    text.push_str(cell);
                    if text.len() >= needle.len() {
                        break;
                    }
                }
                if text.starts_with(needle) {
                    return Some((x as u16, y));
                }
            }
        }
        None
    }

    fn selected(model: &Model) -> &str {
        model.selected.as_deref().unwrap()
    }

    #[test]
    fn guides_draw_the_tree() {
        assert_eq!(
            guides(&[0, 1, 2, 1, 0, 1]),
            ["", "├─ ", "│ └─ ", "└─ ", "", "└─ "]
        );
        assert_eq!(guides(&[0, 1, 2, 2]), ["", "└─ ", "  ├─ ", "  └─ "]);
    }

    #[test]
    fn keys_move_filter_focus_and_quit() {
        let mut m = model();
        // Tree order: lead, impl, tests, review, docs.
        assert_eq!(selected(&m), "lead");
        keys(&mut m, &[Key::Char('j'), Key::Down]);
        assert_eq!(selected(&m), "tests");
        keys(&mut m, &[Key::Char('G')]);
        assert_eq!(selected(&m), "docs");
        keys(&mut m, &[Key::Up, Key::Char('g')]);
        assert_eq!(selected(&m), "lead");
        keys(&mut m, &[Key::PageDown]);
        assert_eq!(selected(&m), "docs");

        // Focus: only the subtree, and back out.
        keys(&mut m, &[Key::Home, Key::Down, Key::Enter]);
        assert_eq!(m.focus.as_deref(), Some("impl"));
        let names: Vec<&str> = m
            .visible()
            .iter()
            .map(|r| m.infos[r.index].name.as_str())
            .collect();
        assert_eq!(names, ["impl", "tests"]);
        assert_eq!(m.visible()[1].depth, 1);
        keys(&mut m, &[Key::Backspace]);
        assert_eq!(m.focus.as_deref(), Some("lead"));
        assert_eq!(selected(&m), "impl");
        keys(&mut m, &[Key::Esc]);
        assert_eq!(m.focus, None);

        // Filter: matches plus their ancestors as context.
        keys(
            &mut m,
            &[
                Key::Char('/'),
                Key::Char('b'),
                Key::Char('l'),
                Key::Char('o'),
            ],
        );
        assert_eq!(m.mode, Mode::Filter);
        let rows = m.visible();
        let shown: Vec<(&str, bool)> = rows
            .iter()
            .map(|r| (m.infos[r.index].name.as_str(), r.context))
            .collect();
        assert_eq!(shown, [("lead", true), ("impl", true), ("tests", false)]);
        // q is text while filtering.
        assert_eq!(keys(&mut m, &[Key::Char('q')]), Flow::Continue);
        assert_eq!(m.filter, "bloq");
        keys(&mut m, &[Key::Backspace, Key::Enter]);
        assert_eq!((m.mode, m.filter.as_str()), (Mode::Browse, "blo"));
        // Esc clears the filter first, then quits.
        assert_eq!(keys(&mut m, &[Key::Esc]), Flow::Continue);
        assert!(m.filter.is_empty());
        assert_eq!(keys(&mut m, &[Key::Esc]), Flow::Quit);

        let mut m = model();
        keys(&mut m, &[Key::Char('?')]);
        assert_eq!(m.mode, Mode::Help);
        assert_eq!(keys(&mut m, &[Key::Char('x')]), Flow::Continue);
        assert_eq!(m.mode, Mode::Browse);
        assert_eq!(keys(&mut m, &[Key::Char('q')]), Flow::Quit);
        assert_eq!(keys(&mut model(), &[Key::Interrupt]), Flow::Quit);
    }

    #[test]
    fn the_selection_follows_its_branch_across_refreshes() {
        let mut m = model();
        keys(&mut m, &[Key::End]);
        assert_eq!(selected(&m), "docs");
        let mut infos = yard();
        infos.insert(0, info("new", None, BranchStatus::Running));
        update(&mut m, Msg::Refreshed(snapshot(infos, Vec::new())));
        assert_eq!(selected(&m), "docs");
        // Gone: the row at the same position is selected instead.
        let infos: Vec<BranchInfo> = yard().into_iter().filter(|i| i.name != "docs").collect();
        update(&mut m, Msg::Refreshed(snapshot(infos, Vec::new())));
        assert_eq!(selected(&m), "review");
    }

    #[test]
    fn events_feed_the_detail_pane() {
        let mut m = model();
        let message = |id, to: &str, delivered| {
            Activity::Message(Message {
                id,
                from: "lead".into(),
                to: to.into(),
                kind: MessageKind::Report,
                text: "hi".into(),
                in_reply_to: None,
                at_ms: 0,
                delivered,
            })
        };
        let usage = |cumulative, input, output| {
            Activity::Harness(Event::UsageObserved {
                turn: Some(1),
                usage: Usage {
                    cumulative,
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    cached_input_tokens: None,
                    cost_usd: None,
                },
            })
        };
        update(
            &mut m,
            Msg::Refreshed(snapshot(
                yard(),
                vec![
                    ("impl", message(1, "impl", false)),
                    ("impl", message(2, "impl", false)),
                    ("lead", message(1, "impl", false)),
                    (
                        "impl",
                        Activity::MessagesDelivered {
                            ids: vec![1],
                            via: DeliveredVia::TurnStart {
                                boundary: "turn_start".into(),
                            },
                        },
                    ),
                    ("impl", usage(false, 100, 10)),
                    ("impl", usage(false, 50, 5)),
                    ("lead", usage(true, 1_000, 100)),
                    ("lead", usage(true, 1_500, 120)),
                    ("impl", Activity::Status(BranchStatus::Interrupted)),
                    ("gone", Activity::Prompt("x".into())),
                ],
            )),
        );
        let impl_ = &m.detail["impl"];
        assert_eq!(impl_.unread, BTreeSet::from([2]));
        assert_eq!((impl_.tokens.input, impl_.tokens.output), (150, 15));
        let lead = &m.detail["lead"];
        assert!(lead.unread.is_empty(), "a message lead sent is not lead's");
        assert_eq!((lead.tokens.input, lead.tokens.output), (1_500, 120));
        let last = impl_.recent.back().unwrap();
        assert_eq!(last.2, Tone::Warn);
        assert!(!m.detail.contains_key("gone"));
    }

    #[test]
    fn the_dashboard_lays_out_tree_detail_header_and_footer() {
        let mut m = model();
        update(
            &mut m,
            Msg::Refreshed(snapshot(
                yard(),
                vec![
                    ("lead", Activity::Prompt("Split the parser".into())),
                    (
                        "lead",
                        Activity::Harness(Event::MessageDelta {
                            turn: 1,
                            text: "Reading the grammar".into(),
                        }),
                    ),
                ],
            )),
        );
        let buffer = draw(&m, 140, 24);
        let screen = lines(&buffer);
        assert!(
            screen[0].starts_with("by watch · /repo  5 branches · 1 running"),
            "{screen:#?}"
        );
        assert!(screen[0].contains("⚠ 1 interrupted"), "{}", screen[0]);
        assert!(
            screen[0].contains("1 failed · 1 blocked · $2.50 reported"),
            "{}",
            screen[0]
        );
        assert!(
            screen[23].starts_with("j/k move  Enter focus  / filter  ? help  q quit"),
            "{}",
            screen[23]
        );
        // The tree, with guides, statuses and the selection marker.
        let tree: Vec<&str> = screen[3..8].iter().map(|l| l.as_str()).collect();
        assert!(tree[0].starts_with("│▶ ● lead"), "{screen:#?}");
        assert!(tree[1].starts_with("│  ├─ ⚠ impl"), "{screen:#?}");
        assert!(tree[2].starts_with("│  │ └─ ■ tests"), "{screen:#?}");
        assert!(tree[3].starts_with("│  └─ ◌ review"), "{screen:#?}");
        assert!(tree[4].starts_with("│  ✖ docs"), "{screen:#?}");
        assert!(screen[3].contains("Reading the gram"), "{}", screen[3]);
        // The detail pane beside it, for the selected branch.
        let (x, _) = find(&buffer, " lead ").unwrap();
        assert!(x > 70, "detail pane is on the right: {x}");
        assert!(find(&buffer, "codex (codex-app-server)").is_some());
        assert!(find(&buffer, "children ◌ review").is_none());
        assert!(find(&buffer, "⚠ impl, ◌ review").is_some(), "{screen:#?}");
        assert!(
            find(&buffer, "prompt: Split the parser").is_some(),
            "{screen:#?}"
        );
        assert!(find(&buffer, "no unread messages").is_some());
    }

    #[test]
    fn interrupted_branches_stand_out() {
        let m = model();
        let buffer = draw(&m, 140, 24);
        let (x, y) = find(&buffer, "INTERRUPTED").unwrap();
        let cell = &buffer[(x, y)];
        assert_eq!((cell.fg, cell.bg), (Color::Black, Color::Yellow));
        assert!(cell.modifier.contains(Modifier::BOLD));
        let (x, y) = find(&buffer, "impl").unwrap();
        assert_eq!(buffer[(x, y)].fg, Color::Yellow);
        let (x, y) = find(&buffer, "⚠ 1 interrupted").unwrap();
        assert_eq!(buffer[(x, y)].bg, Color::Yellow);
        // Selected, its detail names what to do next, in the same color.
        let mut m = m;
        keys(&mut m, &[Key::Down]);
        let buffer = draw(&m, 140, 24);
        let (x, y) = find(&buffer, "by log impl · by rm impl").unwrap();
        assert_eq!(buffer[(x, y)].fg, Color::Yellow);
    }

    #[test]
    fn narrow_and_short_terminals_drop_columns_then_the_detail() {
        let m = model();
        // Narrow: detail below the tree, fewer columns.
        let buffer = draw(&m, 60, 30);
        let screen = lines(&buffer);
        assert!(
            !screen.iter().any(|l| l.contains("ACTIVITY")),
            "{screen:#?}"
        );
        assert!(screen.iter().any(|l| l.contains("HARNESS")), "{screen:#?}");
        let tree_row = find(&buffer, "BRANCH").unwrap().1;
        let detail_row = find(&buffer, " lead ").unwrap().1;
        assert!(detail_row > tree_row, "{screen:#?}");
        // Short: the tree alone.
        let buffer = draw(&m, 60, 10);
        assert!(find(&buffer, "latest events").is_none());
        assert!(find(&buffer, "lead").is_some());
        for line in lines(&buffer) {
            assert!(line.chars().count() <= 60);
        }
    }

    #[test]
    fn help_filter_and_empty_states_draw() {
        let mut m = model();
        keys(&mut m, &[Key::Char('?')]);
        let buffer = draw(&m, 100, 24);
        assert!(find(&buffer, "by watch keys").is_some());
        assert!(find(&buffer, "filter by name, harness, status or prompt").is_some());

        let mut m = model();
        keys(&mut m, &[Key::Char('/'), Key::Char('z'), Key::Char('z')]);
        let buffer = draw(&m, 100, 24);
        assert!(find(&buffer, "no branch matches the filter").is_some());
        assert!(
            lines(&buffer)[23].starts_with("/zz▏"),
            "{:?}",
            lines(&buffer)[23]
        );

        let buffer = draw(&Model::new("/repo"), 100, 24);
        assert!(find(&buffer, "loading…").is_some());
        let mut m = Model::new("/repo");
        update(&mut m, Msg::Refreshed(snapshot(Vec::new(), Vec::new())));
        let buffer = draw(&m, 100, 24);
        assert!(find(&buffer, "no branches yet").is_some());
    }

    #[test]
    fn crossterm_keys_map_to_dashboard_keys() {
        let press = |code, modifiers| Key::from_crossterm(KeyEvent::new(code, modifiers));
        assert_eq!(
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Key::Interrupt)
        );
        assert_eq!(
            press(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(Key::Char('j'))
        );
        assert_eq!(
            press(KeyCode::Char('G'), KeyModifiers::SHIFT),
            Some(Key::Char('G'))
        );
        assert_eq!(press(KeyCode::F(1), KeyModifiers::NONE), None);
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(Key::from_crossterm(release), None);
    }
}
