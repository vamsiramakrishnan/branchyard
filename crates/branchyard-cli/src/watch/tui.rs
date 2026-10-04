//! `by watch`'s live dashboard, in the Elm shape: a [`Model`] of what is
//! known, [`update`] folding a [`Msg`] (a key, a fresh [`Snapshot`] of the
//! branches and their new events, or the result of a command) into it and
//! returning the [`Cmd`]s to carry out, and [`view`] drawing it with
//! ratatui. Neither `update` nor `view` does I/O, so both are tested
//! directly, `view` against ratatui's `TestBackend`. [`run`] is the only
//! part that touches the terminal (through crossterm); it hands every
//! command to an [`Effects`], which owns the data source and runs `by`.
//!
//! The keys that act on the selected branch come from the registry in
//! [`super::actions`]: its table drives the key handling, the footer's
//! hints and the `?` sheet.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::mpsc;
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

use super::actions::{self, Action, ActionId, Ask, CopyWhat, PaneKind, Run};
use super::Doing;
use crate::notify;
use crate::render;

/// Recent events kept per branch for the detail pane.
const RECENT: usize = 50;

/// Rows PageUp and PageDown move.
const PAGE: usize = 10;

/// How long a result stays in the status line, in milliseconds of the data
/// source's clock.
const TOAST_MS: u64 = 10_000;

/// Output lines kept in a result pane.
const OUTPUT_LINES: usize = 2_000;

/// A refresh from the data source.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Where the branches come from: a repository path or a server.
    pub label: String,
    pub infos: Vec<BranchInfo>,
    /// Events recorded since the previous snapshot, by branch.
    pub events: Vec<(String, RecordedEvent)>,
    pub now_ms: u64,
    /// The branch `by try` has applied to this checkout, if any.
    pub trying: Option<String>,
    /// `by usage`'s one-line summary of the local logins (local only).
    pub usage: Option<String>,
    /// The maps running or unfinished and their progress (local only).
    pub maps: Option<String>,
    /// Each branch's listening ports, one line each (local only).
    pub ports: std::collections::BTreeMap<String, Vec<String>>,
    /// The task each branch is an attempt of, one line each
    /// (docs/task-repos.md).
    pub tasks: std::collections::BTreeMap<String, String>,
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
    /// Text pasted into the terminal (bracketed paste).
    Paste(String),
    /// The terminal's size, in cells.
    Resize(u16, u16),
    Refreshed(Snapshot),
    /// A background command started; its output goes to `log`.
    Started {
        action: ActionId,
        branch: String,
        log: String,
    },
    /// A command ended: its exit status and its output (for a background
    /// command, the end of its log).
    Done {
        action: ActionId,
        branch: String,
        ok: bool,
        output: String,
    },
    /// A pane's text, or why it could not be loaded.
    Loaded {
        kind: PaneKind,
        branch: String,
        result: Result<String, String>,
    },
}

/// What [`update`] asks [`run`] to do: the effects, kept out of the model.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Quit,
    /// Run `by` with these arguments; see [`actions::Run`].
    Run(Invocation),
    /// Load a pane's text for a branch.
    Load {
        kind: PaneKind,
        branch: String,
    },
    /// Put text on the clipboard through the terminal.
    Copy(String),
    /// Open a branch's worktree in the editor (`by open`); [`run`] leaves
    /// the screen for an editor that takes over the terminal.
    Open(String),
    /// Tell the person a branch needs them or ended; see [`crate::notify`].
    Notify(notify::Notice),
}

/// One run of `by` for an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub action: ActionId,
    pub branch: String,
    /// After `by` and the global flags.
    pub argv: Vec<String>,
    /// Detached, with its output in a log, rather than waited for.
    pub background: bool,
    /// Run in this terminal, which the dashboard leaves to it until it
    /// exits ([`Run::Terminal`]).
    pub terminal: bool,
}

/// What keys do right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Mode {
    #[default]
    Browse,
    /// Typing a filter after `/`.
    Filter,
    /// The `?` sheet is open.
    Help,
    /// Typing the text an action asks for.
    Input(Input),
    /// Asking before an action runs.
    Confirm(Pending),
    /// A scrollable pane over the dashboard.
    Pane(Pane),
}

/// The one-line text box an action such as `s` opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub action: Option<ActionId>,
    pub branch: String,
    pub text: String,
    /// In characters.
    pub cursor: usize,
    /// Lines shown above the box to choose from (`r`'s checkpoints):
    /// `None` while loading, `Err` when they could not be.
    pub list: Option<Result<Vec<String>, String>>,
}

impl Input {
    fn byte(&self, at: usize) -> usize {
        self.text
            .char_indices()
            .nth(at)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.byte(self.cursor);
            self.text.remove(at);
        }
    }
}

/// An action waiting for a yes or no.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub action: ActionId,
    pub branch: String,
    /// What was typed first, for an action that asks both (`r`).
    pub text: String,
    /// The `off` form of a [`Run::Toggle`].
    pub off: bool,
}

/// A diff, a log or a command's output, scrolled by line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub kind: PaneKind,
    pub branch: String,
    pub title: String,
    /// `None` while loading; `Err` says why it could not be.
    pub lines: Option<Result<Vec<String>, String>>,
    /// The first line shown.
    pub scroll: usize,
    /// Keep the end in view as lines arrive.
    pub follow: bool,
}

impl Pane {
    fn loading(kind: PaneKind, branch: &str) -> Pane {
        let title = match kind {
            PaneKind::Diff => format!("diff {branch}"),
            PaneKind::Log => format!("log {branch}"),
            PaneKind::Output => branch.to_owned(),
            PaneKind::Checkpoints => format!("checkpoints {branch}"),
            PaneKind::Compare => format!("compare {branch} with its siblings"),
        };
        Pane {
            kind,
            branch: branch.to_owned(),
            title,
            lines: None,
            scroll: 0,
            follow: kind == PaneKind::Log,
        }
    }

    fn len(&self) -> usize {
        match &self.lines {
            Some(Ok(lines)) => lines.len(),
            _ => 1,
        }
    }
}

/// The status line: the latest result or refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub at_ms: u64,
}

/// What the detail pane shows beyond [`BranchInfo`], from the events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Detail {
    /// The latest events, oldest first, as (milliseconds, line, tone).
    pub recent: VecDeque<(u64, String, Tone)>,
    /// Inbox messages sent to this branch and not yet delivered to it.
    pub unread: BTreeSet<u64>,
    pub tokens: Tokens,
    /// The branch's pull-request steps, folded into merge readiness.
    pub pull_request: Vec<RecordedEvent>,
    /// The checkpoint the branch is at (0 is its base), and the latest
    /// turn with one.
    pub checkpoint: Option<u32>,
    pub last_checkpoint: u32,
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
    /// Watching a server: actions that are local only are refused.
    pub remote: bool,
    pub toast: Option<Toast>,
    /// The terminal's size, for paging a pane.
    pub size: (u16, u16),
    /// What has been notified, so each event is said once.
    pub notified: notify::Tracker,
    /// The branch `by try` has applied to this checkout.
    pub trying: Option<String>,
    /// `by usage`'s summary line, shown under the header.
    pub usage: Option<String>,
    /// The maps' progress, in the header.
    pub maps: Option<String>,
    /// Each branch's listening ports.
    pub ports: std::collections::BTreeMap<String, Vec<String>>,
    /// The task each branch is an attempt of.
    pub tasks: std::collections::BTreeMap<String, String>,
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

    /// The selected branch, if any is shown.
    pub fn selected_info(&self) -> Option<&BranchInfo> {
        let rows = self.visible();
        self.cursor(&rows).map(|at| &self.infos[rows[at].index])
    }

    fn toast(&mut self, tone: Tone, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            tone,
            at_ms: self.now_ms,
        });
    }

    /// Fold in a refresh. Returns a reload of the log pane when its branch
    /// has new events and the pane is following them.
    fn apply(&mut self, snapshot: Snapshot) -> Vec<Cmd> {
        let rows = self.visible();
        let previous = self.cursor(&rows);
        self.label = snapshot.label;
        self.now_ms = snapshot.now_ms;
        self.infos = snapshot.infos;
        self.trying = snapshot.trying;
        self.usage = snapshot.usage;
        self.maps = snapshot.maps;
        self.ports = snapshot.ports;
        self.tasks = snapshot.tasks;
        if self
            .toast
            .as_ref()
            .is_some_and(|t| self.now_ms.saturating_sub(t.at_ms) > TOAST_MS)
        {
            self.toast = None;
        }
        let mut cmds = Vec::new();
        if let Mode::Pane(pane) = &self.mode {
            let fresh = snapshot.events.iter().any(|(b, _)| *b == pane.branch);
            if pane.kind == PaneKind::Log && pane.follow && fresh && pane.lines.is_some() {
                cmds.push(Cmd::Load {
                    kind: PaneKind::Log,
                    branch: pane.branch.clone(),
                });
            }
        }
        for (branch, event) in snapshot.events {
            // The first refresh reads every log from the start, and a
            // server's stream may replay older events: those only prime
            // the tracker, so what it already said is not said again.
            if let Some(notice) = self.notified.observe(&branch, &event.activity) {
                if self.refreshes > 0 && notify::fresh(event.at_ms, self.now_ms) {
                    cmds.push(Cmd::Notify(notice));
                }
            }
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
                Activity::PullRequest(_) => detail.pull_request.push(event.clone()),
                Activity::Checkpoint(checkpoint) => {
                    detail.checkpoint = Some(checkpoint.turn);
                    detail.last_checkpoint = detail.last_checkpoint.max(checkpoint.turn);
                }
                Activity::Rewound { to, .. } => detail.checkpoint = Some(*to),
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
        cmds
    }
}

/// Fold one message into the model, returning what to do about it.
pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    let key = match msg {
        Msg::Key(key) => key,
        Msg::Refreshed(snapshot) => return model.apply(snapshot),
        Msg::Resize(width, height) => {
            model.size = (width, height);
            return Vec::new();
        }
        Msg::Paste(text) => {
            if let Mode::Input(input) = &mut model.mode {
                // One line: a pasted newline would otherwise send early.
                let line: Vec<&str> = text.lines().map(str::trim_end).collect();
                input.insert(&line.join(" "));
            }
            return Vec::new();
        }
        Msg::Started {
            action,
            branch,
            log,
        } => {
            let name = actions::by_id(action).name;
            model.toast(
                Tone::Accent,
                format!("{name} {branch}: started in the background; output in {log}"),
            );
            return Vec::new();
        }
        Msg::Done {
            action,
            branch,
            ok,
            output,
        } => {
            finished(model, actions::by_id(action), &branch, ok, &output);
            return Vec::new();
        }
        Msg::Loaded {
            kind,
            branch,
            result,
        } => {
            if let Mode::Input(input) = &mut model.mode {
                if kind == PaneKind::Checkpoints && input.branch == branch {
                    input.list = Some(result.map(|text| text.lines().map(str::to_owned).collect()));
                }
                return Vec::new();
            }
            if let Mode::Pane(pane) = &mut model.mode {
                if pane.kind == kind && pane.branch == branch {
                    pane.lines = Some(result.map(|text| match text.is_empty() {
                        true => vec![match kind {
                            PaneKind::Diff => "no changes against the base".to_owned(),
                            PaneKind::Compare => "nothing to compare".to_owned(),
                            _ => "nothing recorded yet".to_owned(),
                        }],
                        false => text.lines().map(str::to_owned).collect(),
                    }));
                    if pane.follow {
                        pane.scroll = pane.len().saturating_sub(1);
                    }
                }
            }
            return Vec::new();
        }
    };
    if key == Key::Interrupt {
        return vec![Cmd::Quit];
    }
    match &mut model.mode {
        Mode::Help => {
            // Any key closes the sheet; q still quits.
            model.mode = Mode::Browse;
            if key == Key::Char('q') {
                return vec![Cmd::Quit];
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
        Mode::Input(input) => match key {
            Key::Esc => {
                model.mode = Mode::Browse;
                model.toast(Tone::Dim, "cancelled; nothing was sent");
            }
            Key::Enter => {
                let text = input.text.trim().to_owned();
                if text.is_empty() {
                    model.toast(Tone::Warn, "type something first, or Esc to cancel");
                    return Vec::new();
                }
                let branch = input.branch.clone();
                let Some(action) = input.action.map(actions::by_id) else {
                    model.mode = Mode::Browse;
                    return Vec::new();
                };
                if let Ask::Checkpoint { .. } = action.ask {
                    // A number first, then a yes naming it.
                    match actions::checkpoint_number(&text) {
                        Ok(turn) => {
                            model.mode = Mode::Confirm(Pending {
                                action: action.id,
                                branch,
                                text: turn.to_string(),
                                off: false,
                            });
                        }
                        Err(why) => model.toast(Tone::Warn, why),
                    }
                    return Vec::new();
                }
                model.mode = Mode::Browse;
                return start(model, action, &branch, &text, false);
            }
            Key::Backspace => input.backspace(),
            Key::Left => input.cursor = input.cursor.saturating_sub(1),
            Key::Right => input.cursor = (input.cursor + 1).min(input.text.chars().count()),
            Key::Home => input.cursor = 0,
            Key::End => input.cursor = input.text.chars().count(),
            Key::Char(c) => input.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        },
        Mode::Confirm(pending) => match key {
            Key::Char('y' | 'Y') | Key::Enter => {
                let pending = pending.clone();
                model.mode = Mode::Browse;
                return start(
                    model,
                    actions::by_id(pending.action),
                    &pending.branch,
                    &pending.text,
                    pending.off,
                );
            }
            Key::Char('n' | 'N' | 'q') | Key::Esc => {
                let name = actions::by_id(pending.action).name;
                model.mode = Mode::Browse;
                model.toast(Tone::Dim, format!("{name}: not done"));
            }
            _ => {}
        },
        Mode::Pane(pane) => {
            let page = usize::from(model.size.1.saturating_sub(4)).max(1);
            let last = pane.len().saturating_sub(1);
            match key {
                Key::Char('q') | Key::Esc | Key::Backspace | Key::Left => {
                    model.mode = Mode::Browse;
                    return Vec::new();
                }
                Key::Char('j') | Key::Down => pane.scroll += 1,
                Key::Char('k') | Key::Up => pane.scroll = pane.scroll.saturating_sub(1),
                Key::PageDown | Key::Char(' ') => pane.scroll += page,
                Key::PageUp => pane.scroll = pane.scroll.saturating_sub(page),
                Key::Char('g') | Key::Home => pane.scroll = 0,
                Key::Char('G') | Key::End => pane.scroll = last,
                _ => {}
            }
            pane.scroll = pane.scroll.min(last);
            pane.follow = pane.kind == PaneKind::Log && pane.scroll == last;
        }
        Mode::Browse => match key {
            Key::Char('q') => return vec![Cmd::Quit],
            Key::Esc => {
                // Esc backs out one level: the status line, the filter,
                // then the focus, then the dashboard.
                if model.toast.is_some() {
                    model.toast = None;
                } else if !model.filter.is_empty() {
                    model.filter.clear();
                } else if model.focus.is_some() {
                    model.focus = None;
                } else {
                    return vec![Cmd::Quit];
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
            Key::Enter | Key::Right => {
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
            Key::Char(c) => {
                if let Some(action) = actions::by_key(c) {
                    return trigger(model, action);
                }
            }
            _ => {}
        },
    }
    Vec::new()
}

/// An action's key on the selected branch: refused with the reason, or
/// asking what it needs, or started.
fn trigger(model: &mut Model, action: &'static Action) -> Vec<Cmd> {
    let Some(info) = model.selected_info() else {
        model.toast(Tone::Warn, "no branch selected");
        return Vec::new();
    };
    if let Some(why) = actions::refusal(action, info, model.remote) {
        model.toast(Tone::Warn, why);
        return Vec::new();
    }
    let branch = info.name.clone();
    // A toggle's off form, on the branch being tried.
    let off = matches!(action.run, Run::Toggle { .. })
        && model.trying.as_deref() == Some(branch.as_str());
    match action.ask {
        Ask::Nothing => start(model, action, &branch, "", off),
        Ask::Text { .. } => {
            model.mode = Mode::Input(Input {
                action: Some(action.id),
                branch,
                ..Input::default()
            });
            Vec::new()
        }
        Ask::Checkpoint { .. } => {
            model.mode = Mode::Input(Input {
                action: Some(action.id),
                branch: branch.clone(),
                ..Input::default()
            });
            vec![Cmd::Load {
                kind: PaneKind::Checkpoints,
                branch,
            }]
        }
        Ask::Confirm { .. } => {
            model.mode = Mode::Confirm(Pending {
                action: action.id,
                branch,
                text: String::new(),
                off,
            });
            Vec::new()
        }
    }
}

/// The command that carries out `action` on `branch`, with `text` from its
/// input box.
fn start(model: &mut Model, action: &Action, branch: &str, text: &str, off: bool) -> Vec<Cmd> {
    match action.run {
        Run::Background(_) | Run::Wait(_) | Run::Toggle { .. } | Run::Terminal(_) => {
            let argv = actions::command(action, branch, text, off).unwrap_or_default();
            let shown: Vec<String> = argv
                .iter()
                .filter(|a| *a != "--")
                .map(|a| render::truncate(&crate::args::shell_quote(a), 40))
                .collect();
            model.toast(Tone::Accent, format!("running: by {}", shown.join(" ")));
            vec![Cmd::Run(Invocation {
                action: action.id,
                branch: branch.to_owned(),
                argv,
                background: matches!(action.run, Run::Background(_)),
                terminal: matches!(action.run, Run::Terminal(_)),
            })]
        }
        Run::Pane(kind) => {
            model.mode = Mode::Pane(Pane::loading(kind, branch));
            vec![Cmd::Load {
                kind,
                branch: branch.to_owned(),
            }]
        }
        Run::Copy(what) => {
            let Some(info) = model.info(branch) else {
                return Vec::new();
            };
            let (text, what) = match what {
                CopyWhat::Name => (info.name.clone(), "name"),
                CopyWhat::Path => (info.worktree.display().to_string(), "worktree path"),
            };
            model.toast(
                Tone::Good,
                format!("copied {branch}'s {what} (if the terminal allows OSC 52): {text}"),
            );
            vec![Cmd::Copy(text)]
        }
        Run::Open => {
            model.toast(Tone::Accent, format!("opening {branch}'s worktree"));
            vec![Cmd::Open(branch.to_owned())]
        }
    }
}

/// A command's result: the status line, and for a merge (whose check
/// output matters) or a failure with more to say, a pane with the output.
fn finished(model: &mut Model, action: &Action, branch: &str, ok: bool, output: &str) {
    let last = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(match ok {
            true => "done",
            false => "failed",
        });
    let last = last.strip_prefix("by: ").unwrap_or(last);
    let background = matches!(action.run, Run::Background(_));
    let text = match (ok, background) {
        (true, true) => format!("{} {branch}: finished; {last}", action.name),
        (true, false) => format!("{} {branch}: {last}", action.name),
        (false, _) => format!("{} {branch} failed: {last}", action.name),
    };
    model.toast(if ok { Tone::Good } else { Tone::Bad }, text);
    let lines: Vec<&str> = output.lines().collect();
    // A merge's and a pull request's check output, and a rewind's note on
    // how the conversation continues, are worth reading whole.
    let show = matches!(action.id, ActionId::Merge | ActionId::Pr | ActionId::Rewind)
        || (!ok && lines.len() > 1);
    if show && model.mode == Mode::Browse {
        let mut pane = Pane::loading(
            PaneKind::Output,
            &format!(
                "{} {branch}: {}",
                action.name,
                if ok { "done" } else { "failed" }
            ),
        );
        pane.branch = branch.to_owned();
        let keep = lines.len().saturating_sub(OUTPUT_LINES);
        let mut shown: Vec<String> = lines[keep..].iter().map(|l| (*l).to_owned()).collect();
        if shown.is_empty() {
            shown.push(last.to_owned());
        }
        pane.lines = Some(Ok(shown));
        model.mode = Mode::Pane(pane);
    }
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
        BranchStatus::AwaitingPlanApproval => (
            "?",
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
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

/// The navigation keys; the action keys come from [`actions::ACTIONS`].
const HELP: &[(&str, &str)] = &[
    ("j / ↓, k / ↑", "move"),
    ("PgDn, PgUp", "move ten rows"),
    ("g / Home, G / End", "first, last"),
    ("Enter / →", "focus the branch's subtree (again to leave)"),
    ("Backspace / h / ←", "focus the parent's subtree"),
    ("/", "filter by name, harness, status or prompt"),
    (
        "Esc",
        "clear the status line, the filter, the focus, then quit",
    ),
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
    match &model.mode {
        Mode::Help => draw_help(model, frame, area),
        Mode::Input(input) => draw_input(input, frame, body),
        Mode::Confirm(pending) => draw_confirm(model, pending, frame, body),
        Mode::Pane(pane) => draw_pane(pane, frame, body),
        Mode::Browse | Mode::Filter => {}
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
    if let Some(maps) = &model.maps {
        spans.push(Span::styled(
            format!(" · map {maps}"),
            Style::new().fg(Color::Cyan),
        ));
    }
    if let Some(usage) = &model.usage {
        spans.push(Span::styled(
            format!(" · usage {usage}"),
            Style::new().fg(if usage.contains("full") {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ));
    }
    Line::from(spans)
}

fn footer_line(model: &Model) -> Line<'static> {
    let key = |k: &str| Span::styled(k.to_owned(), Style::new().fg(Color::Black).bg(Color::Gray));
    let text = |t: &str| Span::styled(format!(" {t}  "), Style::new().fg(Color::DarkGray));
    match &model.mode {
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
        Mode::Input(input) => Line::from(vec![
            key("Enter"),
            text(match input.action.map(|id| actions::by_id(id).ask) {
                Some(Ask::Checkpoint { .. }) => "choose",
                _ => "send",
            }),
            key("Esc"),
            text("cancel"),
            key("←/→"),
            text("move"),
        ]),
        Mode::Confirm(_) => Line::from(vec![key("y"), text("yes"), key("n"), text("no")]),
        Mode::Pane(_) => Line::from(vec![
            key("j/k"),
            text("scroll"),
            key("PgDn/PgUp"),
            text("page"),
            key("g/G"),
            text("top, end"),
            key("q"),
            text("close"),
        ]),
        Mode::Browse | Mode::Help => {
            if let Some(toast) = &model.toast {
                return Line::from(Span::styled(
                    toast.text.replace('\n', " "),
                    tone_style(toast.tone),
                ));
            }
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
            ];
            // The actions that apply to the selected branch, from the
            // registry.
            if let Some(info) = model.selected_info() {
                for action in actions::ACTIONS {
                    if actions::refusal(action, info, model.remote).is_none() {
                        spans.push(key(&action.key.to_string()));
                        spans.push(text(action.name));
                    }
                }
            }
            spans.extend([key("?"), text("keys"), key("q"), text("quit")]);
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
    if let Some(detail) = detail.filter(|d| d.last_checkpoint > 0 || d.checkpoint.is_some()) {
        let at = match detail.checkpoint {
            Some(0) => "at the base (0)".to_owned(),
            Some(turn) => format!("at {turn}"),
            None => "not at a recorded one".to_owned(),
        };
        lines.push(Line::from(vec![
            label("checkpt"),
            Span::raw(format!("{at} of {} · r rewinds", detail.last_checkpoint)),
        ]));
    }
    if let Some(readiness) = detail
        .filter(|d| !d.pull_request.is_empty())
        .and_then(|d| crate::pr::readiness(info, &crate::pr::state(info, &d.pull_request)))
    {
        let look = match readiness.verdict {
            "ready" | "merged" => Style::new().fg(Color::Green),
            "closed" => Style::new().fg(Color::DarkGray),
            _ => Style::new().fg(Color::Yellow),
        };
        lines.push(Line::from(vec![
            label("merge"),
            Span::styled(
                crate::pr::readiness_text(&readiness, model.now_ms, render::Style { color: false }),
                look,
            ),
        ]));
    }
    if let Some(task) = model.tasks.get(&info.name) {
        lines.push(Line::from(vec![label("task"), Span::raw(task.clone())]));
    }
    if let Some(ports) = model.ports.get(&info.name).filter(|p| !p.is_empty()) {
        for (i, port) in ports.iter().enumerate() {
            lines.push(Line::from(vec![
                label(if i == 0 { "ports" } else { "" }),
                Span::raw(port.clone()),
            ]));
        }
        lines.push(Line::from(vec![
            label(""),
            Span::styled(
                "b opens one in a browser · K stops them",
                Style::new().fg(Color::DarkGray),
            ),
        ]));
    }
    if model.trying.as_deref() == Some(info.name.as_str()) {
        lines.push(Line::from(vec![
            label("try"),
            Span::styled(
                "applied to this checkout (t restores it)",
                Style::new().fg(Color::Magenta),
            ),
        ]));
    }
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
        let stamp = branchyard_support::time::rfc3339(*at_ms);
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

/// The `?` sheet's lines: navigation, then every action in the registry
/// with what stops it here.
pub fn help_lines(remote: bool) -> Vec<Line<'static>> {
    let row = |keys: String, what: String, note: Option<String>| {
        let mut spans = vec![
            Span::styled(format!("{keys:>19}  "), Style::new().fg(Color::Cyan)),
            Span::raw(what),
        ];
        if let Some(note) = note {
            spans.push(Span::styled(
                format!("  ({note})"),
                Style::new().fg(Color::DarkGray),
            ));
        }
        Line::from(spans)
    };
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_owned(),
            Style::new().add_modifier(Modifier::BOLD),
        ))
    };
    let mut lines = vec![heading("navigate")];
    lines.extend(
        HELP.iter()
            .map(|(keys, what)| row((*keys).to_owned(), (*what).to_owned(), None)),
    );
    lines.push(Line::from(""));
    lines.push(heading("on the selected branch"));
    for action in actions::ACTIONS {
        let note = match action.remote {
            actions::Remote::No(_) if remote => Some("local only".to_owned()),
            _ => None,
        };
        let asks = match action.ask {
            Ask::Confirm { .. } | Ask::Checkpoint { .. } => ", after a yes",
            _ => "",
        };
        lines.push(row(
            action.key.to_string(),
            format!("{}{asks}", action.help),
            note,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("⚠ INTERRUPTED", status_style(&BranchStatus::Interrupted).1),
        Span::raw(" stopped mid-turn (R resumes it); ✉ counts unread inbox messages"),
    ]));
    lines.push(Line::from(Span::styled(
        "any key closes this sheet",
        Style::new().fg(Color::DarkGray),
    )));
    lines
}

fn draw_help(model: &Model, frame: &mut Frame, area: Rect) {
    let lines = help_lines(model.remote);
    let width = 104.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = area.centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" by watch keys ")),
        popup,
    );
}

/// A box of `height` rows across most of `area`, a third of the way down.
fn popup(area: Rect, height: u16) -> Rect {
    let width = (area.width * 4 / 5).clamp(20.min(area.width), 100);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 3;
    Rect::new(x, y, width, height)
}

fn draw_input(input: &Input, frame: &mut Frame, area: Rect) {
    let (title, bottom) = match input.action.map(|id| actions::by_id(id).ask) {
        Some(Ask::Text { title }) => (actions::fill(title, &input.branch), "Enter sends"),
        Some(Ask::Checkpoint { title, .. }) => {
            (actions::fill(title, &input.branch), "Enter chooses")
        }
        _ => (input.branch.clone(), "Enter sends"),
    };
    let area = match input.action.map(|id| actions::by_id(id).ask) {
        Some(Ask::Checkpoint { .. }) => draw_choices(input, frame, area),
        _ => area,
    };
    let area = popup(area, 3);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .title_bottom(format!(" {bottom} · Esc cancels "))
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    // Keep the cursor in view: scroll the text left as it grows.
    let before: String = input.text.chars().take(input.cursor).collect();
    let used = render::width(&before);
    let room = usize::from(inner.width.saturating_sub(1));
    let skip = used.saturating_sub(room);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(input.text.clone())
            .scroll((0, skip as u16))
            .block(block),
        area,
    );
    frame.set_cursor_position((inner.x + (used - skip) as u16, inner.y));
}

/// The list an input box chooses from, drawn in the upper part of `area`;
/// returns the part left for the box.
fn draw_choices(input: &Input, frame: &mut Frame, area: Rect) -> Rect {
    let lines: Vec<Line> = match &input.list {
        None => vec![Line::from(Span::styled(
            "loading…",
            Style::new().fg(Color::DarkGray),
        ))],
        Some(Err(error)) => vec![Line::from(Span::styled(
            error.clone(),
            Style::new().fg(Color::Red),
        ))],
        Some(Ok(lines)) => lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| Line::from(l.clone()))
            .collect(),
    };
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(5).max(3));
    let list = popup(area, height);
    frame.render_widget(Clear, list);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(
                    " checkpoints of {} (* is where it is) ",
                    input.branch
                ))
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        list,
    );
    let below = list.y + list.height;
    Rect::new(
        area.x,
        below,
        area.width,
        (area.y + area.height).saturating_sub(below),
    )
}

fn draw_confirm(model: &Model, pending: &Pending, frame: &mut Frame, area: Rect) {
    let action = actions::by_id(pending.action);
    let question = match (action.ask, action.run) {
        (_, Run::Toggle { off_question, .. }) if pending.off => {
            actions::fill(off_question, &pending.branch)
        }
        (Ask::Confirm { question } | Ask::Checkpoint { question, .. }, _) => {
            actions::fill(question, &pending.branch).replace("{text}", &pending.text)
        }
        _ => format!("{} {}?", action.name, pending.branch),
    };
    let mut lines = vec![Line::from(question), Line::from("")];
    if let Some(info) = model.info(&pending.branch) {
        if let Some(candidate) = &info.candidate {
            lines.push(Line::from(vec![
                Span::styled("candidate ", Style::new().fg(Color::DarkGray)),
                Span::raw(format!(
                    "{} · {} file{}, +{} -{}",
                    candidate.commit.get(..10).unwrap_or(&candidate.commit),
                    candidate.files_changed,
                    if candidate.files_changed == 1 {
                        ""
                    } else {
                        "s"
                    },
                    candidate.insertions,
                    candidate.deletions
                )),
            ]));
        }
        let (glyph, status, look) = status_look(info);
        lines.push(Line::from(vec![
            Span::styled("status    ", Style::new().fg(Color::DarkGray)),
            Span::styled(format!("{glyph} {status}"), look),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Green)),
        Span::raw(format!(" {}   ", action.name)),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" not now"),
    ]));
    let area = popup(area, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(format!(" {} {} ", action.name, pending.branch))
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        area,
    );
}

/// A diff line's color, as `git diff` colors it.
fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        Style::new().add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new()
    }
}

fn draw_pane(pane: &Pane, frame: &mut Frame, area: Rect) {
    let total = pane.len();
    let height = usize::from(area.height.saturating_sub(2)).max(1);
    // The last page ends at the last line rather than past it.
    let top = pane.scroll.min(total.saturating_sub(height));
    let position = match &pane.lines {
        Some(Ok(lines)) if !lines.is_empty() => format!(
            " {}-{} of {} ",
            top + 1,
            (top + height).min(lines.len()),
            lines.len()
        ),
        _ => String::new(),
    };
    let block = Block::bordered()
        .title(format!(" {} ", pane.title))
        .title_bottom(Line::from(position).right_aligned())
        .border_style(Style::new().fg(Color::Cyan));
    let lines: Vec<Line> = match &pane.lines {
        None => vec![Line::from(Span::styled(
            "loading…",
            Style::new().fg(Color::DarkGray),
        ))],
        Some(Err(error)) => vec![Line::from(Span::styled(
            error.clone(),
            Style::new().fg(Color::Red),
        ))],
        Some(Ok(lines)) => lines
            .iter()
            .skip(top)
            .take(height)
            .map(|line| {
                let style = match pane.kind {
                    PaneKind::Diff => diff_style(line),
                    _ => Style::new(),
                };
                Line::from(Span::styled(line.replace('\t', "    "), style))
            })
            .collect(),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// What [`run`] needs from outside the terminal: the data, and a way to
/// carry out commands. Results come back as messages on `done`, so slow
/// work (a merge's check, a turn) never holds up drawing.
pub trait Effects {
    type Error;
    /// The branches and the events recorded since the last call.
    fn refresh(&mut self) -> Result<Snapshot, Self::Error>;
    /// Carry out `cmd` (never [`Cmd::Quit`] or [`Cmd::Open`]).
    fn perform(&mut self, cmd: Cmd, done: &mpsc::Sender<Msg>);
    /// How `by open` would start an editor on `branch`'s worktree, or why
    /// it cannot.
    fn editor(&mut self, branch: &str) -> Result<crate::open::Plan, String>;
}

/// `by open` from the dashboard: a terminal editor gets the screen (raw
/// mode off, the main screen back) until it exits, and the dashboard is
/// redrawn from scratch; a graphical one starts on another thread. The
/// result comes back as [`Msg::Done`].
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
fn open_editor(
    terminal: &mut ratatui::DefaultTerminal,
    plan: Result<crate::open::Plan, String>,
    branch: String,
    done: &mpsc::Sender<Msg>,
) -> std::io::Result<()> {
    use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    let finished = move |result: crate::commands::Outcome, argv: &[String]| Msg::Done {
        action: ActionId::Open,
        branch,
        ok: result.is_ok(),
        output: match result {
            Ok(()) => format!("opened with {}", argv.join(" ")),
            Err(error) => error.to_string(),
        },
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            let _ = done.send(finished(Err(crate::commands::Failure::Message(error)), &[]));
            return Ok(());
        }
    };
    if !plan.terminal {
        let done = done.clone();
        std::thread::spawn(move || {
            let result = crate::open::launch(&plan);
            let _ = done.send(finished(result, &plan.argv));
        });
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    let result = crate::open::launch(&plan);
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    terminal.clear()?;
    let _ = done.send(finished(result, &plan.argv));
    Ok(())
}

/// How long [`run`] waits for a key before looking for results.
const TICK: Duration = Duration::from_millis(100);

/// Run the dashboard until the viewer quits. A refresh error ends the
/// dashboard (after the terminal is restored) and is returned.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
pub fn run<F: Effects>(
    label: String,
    remote: bool,
    interval: Duration,
    mut effects: F,
) -> Result<Result<(), F::Error>, std::io::Error> {
    use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
    use ratatui::crossterm::execute;
    // Raw mode, the alternate screen, and a panic hook that restores both
    // before the panic is reported. `Restore` restores them on every other
    // way out.
    let mut terminal = ratatui::try_init()?;
    struct Restore;
    impl Drop for Restore {
        #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
        fn drop(&mut self) {
            let _ = execute!(std::io::stdout(), DisableBracketedPaste);
            ratatui::restore();
        }
    }
    let _restore = Restore;
    // A paste then arrives whole, so a newline in it does not send early.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let (tx, rx) = mpsc::channel();
    let mut model = Model::new(label);
    model.remote = remote;
    let size = terminal.size()?;
    update(&mut model, Msg::Resize(size.width, size.height));
    terminal.draw(|frame| view(&model, frame))?;
    let mut next_refresh = Instant::now();
    loop {
        let mut msgs = Vec::new();
        if Instant::now() >= next_refresh {
            match effects.refresh() {
                Ok(snapshot) => msgs.push(Msg::Refreshed(snapshot)),
                Err(error) => return Ok(Err(error)),
            }
            next_refresh = Instant::now() + interval;
        }
        msgs.extend(rx.try_iter());
        let wait = next_refresh
            .saturating_duration_since(Instant::now())
            .min(TICK);
        if event::poll(wait)? {
            match event::read()? {
                event::Event::Key(key) => msgs.extend(Key::from_crossterm(key).map(Msg::Key)),
                event::Event::Paste(text) => msgs.push(Msg::Paste(text)),
                event::Event::Resize(width, height) => msgs.push(Msg::Resize(width, height)),
                _ => {}
            }
        }
        if msgs.is_empty() {
            continue;
        }
        for msg in msgs {
            for cmd in update(&mut model, msg) {
                match cmd {
                    Cmd::Quit => return Ok(Ok(())),
                    Cmd::Open(branch) => {
                        let plan = effects.editor(&branch);
                        open_editor(&mut terminal, plan, branch, &tx)?;
                    }
                    // `by review`: the command gets the terminal (for its
                    // editor) until it exits, then the dashboard is redrawn.
                    Cmd::Run(invocation) if invocation.terminal => {
                        use ratatui::crossterm::terminal::{
                            disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
                            LeaveAlternateScreen,
                        };
                        let mut stdout = std::io::stdout();
                        execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen)?;
                        disable_raw_mode()?;
                        effects.perform(Cmd::Run(invocation), &tx);
                        enable_raw_mode()?;
                        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
                        terminal.clear()?;
                    }
                    cmd => effects.perform(cmd, &tx),
                }
            }
        }
        terminal.draw(|frame| view(&model, frame))?;
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
            trying: None,
            usage: None,
            maps: None,
            ports: Default::default(),
            tasks: Default::default(),
        }
    }

    fn model() -> Model {
        let mut model = Model::new("/repo");
        update(&mut model, Msg::Refreshed(snapshot(yard(), Vec::new())));
        model
    }

    /// Whether the loop goes on, from what [`update`] returned.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Flow {
        Continue,
        Quit,
    }

    fn flow(cmds: &[Cmd]) -> Flow {
        match cmds.contains(&Cmd::Quit) {
            true => Flow::Quit,
            false => Flow::Continue,
        }
    }

    fn keys(model: &mut Model, keys: &[Key]) -> Flow {
        let mut last = Flow::Continue;
        for key in keys {
            last = flow(&update(model, Msg::Key(*key)));
        }
        last
    }

    /// The commands the last of `keys` returned.
    fn press(model: &mut Model, keys: &[Key]) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for key in keys {
            cmds = update(model, Msg::Key(*key));
        }
        cmds
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
        assert_eq!((m.mode.clone(), m.filter.as_str()), (Mode::Browse, "blo"));
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
            screen[23].starts_with(
                "j/k move  Enter focus  / filter  S steer  x cancel  f fork  l log  y copy name  \
                 Y copy path  o open  c compare  b browse  K stop ports"
            ),
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

    fn select(model: &mut Model, name: &str) {
        model.selected = Some(name.to_owned());
    }

    fn typed(model: &mut Model, text: &str) {
        for c in text.chars() {
            update(model, Msg::Key(Key::Char(c)));
        }
    }

    fn ready_yard() -> Vec<BranchInfo> {
        let mut infos = yard();
        let mut done = info("done", None, BranchStatus::Ready);
        done.candidate = Some(branchyard::CandidateInfo {
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            files_changed: 3,
            insertions: 10,
            deletions: 2,
        });
        infos.push(done);
        infos
    }

    fn run_of(cmds: &[Cmd]) -> &Invocation {
        match cmds {
            [Cmd::Run(invocation)] => invocation,
            other => panic!("expected one run, got {other:?}"),
        }
    }

    #[test]
    fn resume_is_one_key_on_an_interrupted_branch() {
        let mut m = model();
        select(&mut m, "impl");
        let cmds = press(&mut m, &[Key::Char('R')]);
        let run = run_of(&cmds);
        assert_eq!(run.action, ActionId::Resume);
        assert_eq!(run.argv, ["send", "--", "impl", actions::RESUME_PROMPT]);
        assert!(run.background);
        assert_eq!(m.mode, Mode::Browse);
        assert!(m
            .toast
            .as_ref()
            .unwrap()
            .text
            .starts_with("running: by send impl"));
        // Anywhere else it is refused, with the reason, and runs nothing.
        select(&mut m, "lead");
        assert!(press(&mut m, &[Key::Char('R')]).is_empty());
        let toast = m.toast.clone().unwrap();
        assert_eq!(toast.tone, Tone::Warn);
        assert!(
            toast.text.contains("only an interrupted branch resumes"),
            "{toast:?}"
        );
    }

    #[test]
    fn send_and_steer_read_a_line_of_text_first() {
        let mut m = model();
        select(&mut m, "docs");
        assert!(press(&mut m, &[Key::Char('s')]).is_empty());
        assert!(matches!(&m.mode, Mode::Input(i) if i.branch == "docs"));
        // Keys are text now: q does not quit, s does not nest.
        typed(&mut m, "fix it q");
        update(&mut m, Msg::Paste("s\nand more\n".into()));
        press(&mut m, &[Key::Left, Key::Backspace]);
        let Mode::Input(input) = &m.mode else {
            panic!()
        };
        assert_eq!(input.text, "fix it qs and moe");
        let cmds = press(&mut m, &[Key::Char('r'), Key::Enter]);
        let run = run_of(&cmds);
        assert_eq!(run.argv, ["send", "--", "docs", "fix it qs and more"]);
        assert!(run.background);
        assert_eq!(m.mode, Mode::Browse);

        // Esc cancels; an empty line is not sent.
        press(&mut m, &[Key::Char('s')]);
        assert!(press(&mut m, &[Key::Char(' '), Key::Enter]).is_empty());
        assert!(matches!(m.mode, Mode::Input(_)));
        press(&mut m, &[Key::Esc]);
        assert_eq!(m.mode, Mode::Browse);

        // Steering is for the running turn, and waits for delivery.
        select(&mut m, "lead");
        press(&mut m, &[Key::Char('S')]);
        typed(&mut m, "also add tests");
        let run = run_of(&press(&mut m, &[Key::Enter])).clone();
        assert_eq!(
            run.argv,
            ["send", "--steer", "--", "lead", "also add tests"]
        );
        assert!(!run.background);
        // A turn is running, so s is refused and says what to use.
        press(&mut m, &[Key::Char('s')]);
        assert_eq!(m.mode, Mode::Browse);
        assert!(m.toast.as_ref().unwrap().text.contains("S steers it"));
    }

    #[test]
    fn merge_and_cancel_ask_first_and_merge_shows_the_check_result() {
        let mut m = Model::new("/repo");
        update(&mut m, Msg::Refreshed(snapshot(ready_yard(), Vec::new())));
        select(&mut m, "done");
        assert!(press(&mut m, &[Key::Char('m')]).is_empty());
        assert!(matches!(&m.mode, Mode::Confirm(p) if p.action == ActionId::Merge));
        let buffer = draw(&m, 120, 30);
        assert!(find(&buffer, "Merge done into the checked-out branch?").is_some());
        assert!(find(&buffer, "0123456789 · 3 files, +10 -2").is_some());
        // No: nothing runs.
        assert!(press(&mut m, &[Key::Char('n')]).is_empty());
        assert_eq!(m.mode, Mode::Browse);
        // Yes: the merge runs and is waited for.
        let cmds = press(&mut m, &[Key::Char('m'), Key::Char('y')]);
        let run = run_of(&cmds);
        assert_eq!(run.argv, ["merge", "--", "done"]);
        assert!(!run.background);
        // A failed check: the status line, and the output in a pane.
        update(
            &mut m,
            Msg::Done {
                action: ActionId::Merge,
                branch: "done".into(),
                ok: false,
                output: "by: check failed:\ntest parser ... FAILED\n1 failed\n".into(),
            },
        );
        let toast = m.toast.clone().unwrap();
        assert_eq!(
            (toast.tone, toast.text.as_str()),
            (Tone::Bad, "merge done failed: 1 failed")
        );
        let Mode::Pane(pane) = &m.mode else {
            panic!("{:?}", m.mode)
        };
        assert_eq!(pane.kind, PaneKind::Output);
        assert_eq!(pane.title, "merge done: failed");
        let buffer = draw(&m, 120, 30);
        assert!(find(&buffer, "test parser ... FAILED").is_some());
        press(&mut m, &[Key::Char('q')]);
        // A passing merge shows its result too.
        update(
            &mut m,
            Msg::Done {
                action: ActionId::Merge,
                branch: "done".into(),
                ok: true,
                output: "merged done into main (0123456789..abcdef0123)\n".into(),
            },
        );
        assert!(matches!(&m.mode, Mode::Pane(p) if p.title == "merge done: done"));
        assert_eq!(m.toast.as_ref().unwrap().tone, Tone::Good);

        // Cancel asks too.
        let mut m = model();
        let cmds = press(&mut m, &[Key::Char('x'), Key::Char('y')]);
        assert_eq!(run_of(&cmds).argv, ["cancel", "--", "lead"]);
        press(&mut m, &[Key::Char('x')]);
        assert!(press(&mut m, &[Key::Esc]).is_empty());
        assert!(m.toast.as_ref().unwrap().text.contains("cancel: not done"));
    }

    #[test]
    fn background_results_reach_the_status_line() {
        let mut m = model();
        update(
            &mut m,
            Msg::Started {
                action: ActionId::Send,
                branch: "docs".into(),
                log: "/r/.branchyard/watch/docs-send-1.log".into(),
            },
        );
        assert!(m.toast.as_ref().unwrap().text.contains("docs-send-1.log"));
        update(
            &mut m,
            Msg::Done {
                action: ActionId::Send,
                branch: "docs".into(),
                ok: true,
                output: "…\n✔ docs ready".into(),
            },
        );
        assert_eq!(
            m.toast.as_ref().unwrap().text,
            "send docs: finished; ✔ docs ready"
        );
        assert_eq!(m.mode, Mode::Browse, "a success needs no pane");
        let buffer = draw(&m, 140, 24);
        let (x, y) = find(&buffer, "send docs: finished").unwrap();
        assert_eq!((y, buffer[(x, y)].fg), (23, Color::Green));
        // It fades after a while of refreshes, and Esc clears it first.
        let mut later = snapshot(yard(), Vec::new());
        later.now_ms += TOAST_MS + 1;
        update(&mut m, Msg::Refreshed(later));
        assert_eq!(m.toast, None);
        m.toast_text("x");
        assert_eq!(keys(&mut m, &[Key::Esc]), Flow::Continue);
        assert_eq!(m.toast, None);
    }

    impl Model {
        /// A test's shortcut for a plain status line.
        fn toast_text(&mut self, text: &str) {
            self.toast(Tone::Plain, text);
        }
    }

    #[test]
    fn diff_and_log_open_scrollable_panes() {
        let mut m = Model::new("/repo");
        update(&mut m, Msg::Refreshed(snapshot(ready_yard(), Vec::new())));
        update(&mut m, Msg::Resize(100, 14));
        select(&mut m, "done");
        let cmds = press(&mut m, &[Key::Char('d')]);
        assert_eq!(
            cmds,
            [Cmd::Load {
                kind: PaneKind::Diff,
                branch: "done".into()
            }]
        );
        assert!(find(&draw(&m, 100, 14), "loading…").is_some());
        let diff: String = std::iter::once("diff --git a/x b/x\n+added\n-removed\n".to_owned())
            .chain((0..40).map(|i| format!(" context {i}\n")))
            .collect();
        // A load for another pane is ignored.
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Log,
                branch: "done".into(),
                result: Ok("x".into()),
            },
        );
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Diff,
                branch: "done".into(),
                result: Ok(diff),
            },
        );
        let buffer = draw(&m, 100, 14);
        let (x, y) = find(&buffer, "+added").unwrap();
        assert_eq!(buffer[(x, y)].fg, Color::Green);
        assert!(
            find(&buffer, " 1-10 of 43 ").is_some(),
            "{:#?}",
            lines(&buffer)
        );
        press(&mut m, &[Key::Char('j'), Key::PageDown]);
        let Mode::Pane(pane) = &m.mode else { panic!() };
        assert_eq!(pane.scroll, 11);
        press(&mut m, &[Key::Char('G')]);
        let buffer = draw(&m, 100, 14);
        assert!(find(&buffer, "context 39").is_some());
        assert!(
            find(&buffer, " 34-43 of 43 ").is_some(),
            "{:#?}",
            lines(&buffer)
        );
        assert_eq!(keys(&mut m, &[Key::Char('q')]), Flow::Continue);
        assert_eq!(m.mode, Mode::Browse);

        // The log pane follows new events for its branch.
        let cmds = press(&mut m, &[Key::Char('l')]);
        assert_eq!(
            cmds,
            [Cmd::Load {
                kind: PaneKind::Log,
                branch: "done".into()
            }]
        );
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Log,
                branch: "done".into(),
                result: Ok("one\ntwo\n".into()),
            },
        );
        let refresh = |m: &mut Model, branch: &str| {
            update(
                m,
                Msg::Refreshed(snapshot(
                    ready_yard(),
                    vec![(branch, Activity::Prompt("p".into()))],
                )),
            )
        };
        assert!(refresh(&mut m, "lead").is_empty());
        assert_eq!(
            refresh(&mut m, "done"),
            [Cmd::Load {
                kind: PaneKind::Log,
                branch: "done".into()
            }]
        );
        // Scrolled up, it stops following.
        press(&mut m, &[Key::Up]);
        assert!(refresh(&mut m, "done").is_empty());
        // A failure to load says why.
        press(&mut m, &[Key::Esc]);
        select(&mut m, "done");
        press(&mut m, &[Key::Char('d')]);
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Diff,
                branch: "done".into(),
                result: Err("no such branch".into()),
            },
        );
        assert!(find(&draw(&m, 100, 14), "no such branch").is_some());
    }

    #[test]
    fn copy_keys_and_remote_mode() {
        let mut m = model();
        select(&mut m, "impl");
        assert_eq!(press(&mut m, &[Key::Char('y')]), [Cmd::Copy("impl".into())]);
        assert_eq!(press(&mut m, &[Key::Char('Y')]), [Cmd::Copy("/w".into())]);
        m.remote = true;
        assert!(press(&mut m, &[Key::Char('Y')]).is_empty());
        assert!(m
            .toast
            .as_ref()
            .unwrap()
            .text
            .contains("the worktree is on the server"));
        // What works remotely still does.
        assert_eq!(
            run_of(&press(&mut m, &[Key::Char('R')])).action,
            ActionId::Resume
        );
        // No branch, no action.
        let mut empty = Model::new("/repo");
        update(&mut empty, Msg::Refreshed(snapshot(Vec::new(), Vec::new())));
        assert!(press(&mut empty, &[Key::Char('s')]).is_empty());
        assert_eq!(empty.toast.unwrap().text, "no branch selected");
    }

    fn yes(model: &mut Model, key: char) -> Invocation {
        assert!(
            press(model, &[Key::Char(key)]).is_empty(),
            "{key} asks first"
        );
        assert!(matches!(model.mode, Mode::Confirm(_)), "{:?}", model.mode);
        run_of(&press(model, &[Key::Char('y')])).clone()
    }

    #[test]
    fn pr_open_compare_and_try_run_their_commands() {
        let mut m = Model::new("/repo");
        update(&mut m, Msg::Refreshed(snapshot(ready_yard(), Vec::new())));
        select(&mut m, "done");
        // p: by pr, after a yes, waited for, with its output in a pane.
        press(&mut m, &[Key::Char('p')]);
        let buffer = draw(&m, 120, 30);
        assert!(find(&buffer, "Push done's candidate and open or update").is_some());
        press(&mut m, &[Key::Esc]);
        let pr = yes(&mut m, 'p');
        assert_eq!(pr.argv, ["pr", "--", "done"]);
        assert!(!pr.background);
        update(
            &mut m,
            Msg::Done {
                action: ActionId::Pr,
                branch: "done".into(),
                ok: true,
                output: "check passed\nopened https://github.com/o/r/pull/7\n".into(),
            },
        );
        assert!(matches!(&m.mode, Mode::Pane(p) if p.title == "pr done: done"));
        press(&mut m, &[Key::Char('q')]);
        // P: by pr --watch, in the background.
        let watch = yes(&mut m, 'P');
        assert_eq!(watch.argv, ["pr", "--watch", "--", "done"]);
        assert!(watch.background);
        // v: by review in this terminal (its editor gets the screen), the
        // comments then sent in the background.
        let review = run_of(&press(&mut m, &[Key::Char('v')])).clone();
        assert_eq!(review.argv, ["review", "--detach", "--", "done"]);
        assert!(review.terminal && !review.background);
        assert_eq!(review.action, ActionId::Review);
        // o: the dashboard opens the editor itself.
        assert_eq!(press(&mut m, &[Key::Char('o')]), [Cmd::Open("done".into())]);
        update(
            &mut m,
            Msg::Done {
                action: ActionId::Open,
                branch: "done".into(),
                ok: false,
                output: "no editor: set $VISUAL or $EDITOR".into(),
            },
        );
        assert!(m.toast.as_ref().unwrap().text.contains("no editor"));
        // c: the comparison, in a pane.
        let cmds = press(&mut m, &[Key::Char('c')]);
        assert_eq!(
            cmds,
            [Cmd::Load {
                kind: PaneKind::Compare,
                branch: "done".into()
            }]
        );
        assert!(matches!(&m.mode, Mode::Pane(p) if p.kind == PaneKind::Compare));
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Compare,
                branch: "done".into(),
                result: Ok("BRANCH  STATUS\ndone    ready\n".into()),
            },
        );
        assert!(find(&draw(&m, 100, 24), "done    ready").is_some());
        press(&mut m, &[Key::Char('q')]);
        // t: try, then t again on the tried branch turns it off.
        let on = yes(&mut m, 't');
        assert_eq!(on.argv, ["try", "--", "done"]);
        let mut tried = snapshot(ready_yard(), Vec::new());
        tried.trying = Some("done".into());
        update(&mut m, Msg::Refreshed(tried));
        assert!(find(&draw(&m, 120, 30), "applied to this checkout").is_some());
        press(&mut m, &[Key::Char('t')]);
        assert!(find(&draw(&m, 120, 30), "Turn off the try of done").is_some());
        let off = run_of(&press(&mut m, &[Key::Char('y')])).clone();
        assert_eq!(off.argv, ["try", "--off"]);
        // Not on a running branch, and not against a server.
        select(&mut m, "lead");
        for key in ['p', 'P', 'r'] {
            assert!(press(&mut m, &[Key::Char(key)]).is_empty());
            assert!(m
                .toast
                .as_ref()
                .unwrap()
                .text
                .contains("has a turn running"));
        }
        select(&mut m, "done");
        m.remote = true;
        for key in ['p', 'P', 'o', 'r', 't'] {
            assert!(press(&mut m, &[Key::Char(key)]).is_empty(), "{key}");
            assert!(m.toast.as_ref().unwrap().text.contains("is local only"));
        }
        assert_eq!(
            press(&mut m, &[Key::Char('c')]),
            [Cmd::Load {
                kind: PaneKind::Compare,
                branch: "done".into()
            }]
        );
    }

    #[test]
    fn rewind_chooses_a_checkpoint_from_the_list_then_asks() {
        let mut m = Model::new("/repo");
        update(&mut m, Msg::Refreshed(snapshot(ready_yard(), Vec::new())));
        select(&mut m, "done");
        let cmds = press(&mut m, &[Key::Char('r')]);
        assert_eq!(
            cmds,
            [Cmd::Load {
                kind: PaneKind::Checkpoints,
                branch: "done".into()
            }]
        );
        assert!(matches!(&m.mode, Mode::Input(i) if i.list.is_none()));
        update(
            &mut m,
            Msg::Loaded {
                kind: PaneKind::Checkpoints,
                branch: "done".into(),
                result: Ok("    0  0123456789  base\n  * 1  abcdef0123  2 file(s) +3 -1\n".into()),
            },
        );
        let buffer = draw(&m, 100, 24);
        assert!(find(&buffer, "* 1  abcdef0123").is_some());
        assert!(find(&buffer, " rewind done to checkpoint ").is_some());
        assert!(lines(&buffer)[23].starts_with("Enter choose"));
        // Not a number: said, and the box stays.
        typed(&mut m, "one");
        assert!(press(&mut m, &[Key::Enter]).is_empty());
        assert!(m
            .toast
            .as_ref()
            .unwrap()
            .text
            .contains("is not a checkpoint"));
        assert!(matches!(m.mode, Mode::Input(_)));
        press(&mut m, &[Key::Backspace, Key::Backspace, Key::Backspace]);
        typed(&mut m, "0");
        assert!(press(&mut m, &[Key::Enter]).is_empty());
        assert!(matches!(&m.mode, Mode::Confirm(p) if p.text == "0"));
        let buffer = draw(&m, 120, 30);
        assert!(find(&buffer, "Reset done and its worktree to checkpoint 0?").is_some());
        let run = run_of(&press(&mut m, &[Key::Char('y')])).clone();
        assert_eq!(run.argv, ["rewind", "--yes", "--to", "0", "--", "done"]);
        assert!(!run.background);
        // Esc at the box sends nothing.
        press(&mut m, &[Key::Char('r')]);
        assert!(press(&mut m, &[Key::Esc]).is_empty());
        assert_eq!(m.mode, Mode::Browse);
    }

    #[test]
    fn the_detail_shows_the_checkpoint_and_merge_readiness() {
        use branchyard::{CheckRun, Checkpoint, PullRequestActivity, PullRequestRef};
        let checkpoint = |turn: u32| {
            Activity::Checkpoint(Checkpoint {
                turn,
                commit: "c".repeat(40),
                git_ref: format!("refs/branchyard/done/1/turn-{turn}"),
                after: Some(turn - 1),
                session: None,
                files_changed: 1,
                insertions: 1,
                deletions: 0,
                sandbox: None,
            })
        };
        let pr = |activity: PullRequestActivity| Activity::PullRequest(Box::new(activity));
        let mut m = Model::new("/repo");
        update(
            &mut m,
            Msg::Refreshed(snapshot(
                ready_yard(),
                vec![
                    ("done", checkpoint(1)),
                    ("done", checkpoint(2)),
                    (
                        "done",
                        Activity::Rewound {
                            from: Some(2),
                            to: 1,
                            commit: "c".repeat(40),
                            session: branchyard::SessionContinuity::Fresh {
                                reason: "test".into(),
                            },
                        },
                    ),
                    (
                        "done",
                        pr(PullRequestActivity::Checked(CheckRun {
                            commit: "0123456789abcdef0123456789abcdef01234567".into(),
                            argv: vec!["true".into()],
                            passed: true,
                            timed_out: false,
                            output_tail: String::new(),
                        })),
                    ),
                    (
                        "done",
                        pr(PullRequestActivity::Opened(PullRequestRef {
                            number: 7,
                            url: "https://github.com/o/r/pull/7".into(),
                            head: "by/done".into(),
                            base: None,
                            draft: false,
                        })),
                    ),
                ],
            )),
        );
        select(&mut m, "done");
        let buffer = draw(&m, 160, 40);
        assert!(find(&buffer, "at 1 of 2 · r rewinds").is_some());
        assert!(find(
            &buffer,
            "unknown (by show --refresh asks GitHub) · check passed"
        )
        .is_some());
        assert!(find(&buffer, "PR #7 not observed yet").is_some());
        // A branch without either shows neither.
        select(&mut m, "docs");
        let buffer = draw(&m, 160, 40);
        assert!(find(&buffer, "r rewinds").is_none());
        assert!(find(&buffer, "PR #").is_none());
    }

    #[test]
    fn the_help_sheet_lists_every_action_from_the_registry() {
        let mut m = model();
        keys(&mut m, &[Key::Char('?')]);
        let buffer = draw(&m, 120, 40);
        for action in actions::ACTIONS {
            assert!(find(&buffer, action.help).is_some(), "{}", action.help);
        }
        assert!(find(&buffer, "coming with").is_none());
        assert!(find(&buffer, "local only").is_none());
        m.remote = true;
        let buffer = draw(&m, 120, 40);
        assert!(find(&buffer, "(local only)").is_some());
        // The input box shows its title and the text.
        let mut m = model();
        select(&mut m, "docs");
        press(&mut m, &[Key::Char('f')]);
        typed(&mut m, "try another way");
        let buffer = draw(&m, 100, 24);
        assert!(find(&buffer, " fork docs with the prompt ").is_some());
        assert!(find(&buffer, "try another way").is_some());
        assert!(lines(&buffer)[23].starts_with("Enter send"));
    }

    #[test]
    fn new_events_that_need_you_notify_once_and_history_does_not() {
        let asks = |key: &str| {
            Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request: branchyard::PermissionRequest {
                    key: branchyard::PermissionKey(key.into()),
                    tool: "Bash".into(),
                    input: serde_json::Value::Null,
                },
            })
        };
        // The first refresh is history: nothing notifies.
        let mut m = Model::new("/repo");
        let first = update(
            &mut m,
            Msg::Refreshed(snapshot(
                yard(),
                vec![
                    ("impl", Activity::Status(BranchStatus::Interrupted)),
                    ("lead", asks("old")),
                ],
            )),
        );
        assert!(first.is_empty(), "{first:?}");
        let notices = |cmds: Vec<Cmd>| -> Vec<String> {
            cmds.into_iter()
                .filter_map(|cmd| match cmd {
                    Cmd::Notify(notice) => Some(notice.text),
                    _ => None,
                })
                .collect()
        };
        // Later ones do, once each; activity that needs no one does not.
        let later = update(
            &mut m,
            Msg::Refreshed(snapshot(
                yard(),
                vec![
                    ("lead", asks("old")),
                    ("lead", asks("new")),
                    ("lead", Activity::Prompt("x".into())),
                    ("impl", Activity::Status(BranchStatus::Interrupted)),
                    (
                        "docs",
                        Activity::Status(BranchStatus::Failed {
                            reason: "exit 2".into(),
                        }),
                    ),
                    ("review", Activity::Stalled { since_ms: 1 }),
                ],
            )),
        );
        assert_eq!(
            notices(later),
            [
                "lead asks to use Bash",
                "docs failed: exit 2",
                "review has stalled: no activity from its harness"
            ]
        );
        // An old event arriving late (a server's replay) is not news.
        let mut stale = snapshot(
            yard(),
            vec![("tests", Activity::Status(BranchStatus::Ready))],
        );
        stale.now_ms += notify::FRESH_MS + 60_000;
        assert!(notices(update(&mut m, Msg::Refreshed(stale))).is_empty());
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
