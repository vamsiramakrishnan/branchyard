//! `by watch`: a live tree of every branch with its status, harness,
//! current activity, cost, turns and age.
//!
//! On a terminal it is a ratatui dashboard ([`tui`]): the tree, a detail
//! pane for the selected branch, a filter, subtree focus, a help sheet, and
//! keys that act on the selected branch (send, steer, resume, cancel,
//! merge, fork, diff, log, copy; see [`actions`]), each running the `by`
//! command it names; `q`, Esc or Ctrl-C exits and restores the terminal,
//! as a panic does.
//! Otherwise it appends one line per change, and `--once` prints one
//! frame. Activity comes from the repository's event feed from a cursor:
//! locally through [`Yard::events_since`], remotely from the server's
//! event stream.

mod actions;
mod tui;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use branchyard::{Activity, BranchInfo, Event, RecordedEvent, Yard};
use branchyard_client::api::FeedEntry;
use branchyard_client::Repo;

use crate::commands::{self, Env, Failure, Outcome, Target};
use crate::notify;
use crate::render::{self, Style, Tone};

/// What a branch is doing now, from its recent events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doing {
    /// Tool started most recently in the current turn.
    pub tool: Option<String>,
    /// A permission request not yet answered.
    pub asking: Option<String>,
    /// Text of the current message, tail only.
    text: String,
    pub last_at_ms: u64,
}

/// Feed events read per call while catching up.
const FEED_BATCH: usize = 1000;

/// Text kept per branch for the snippet.
const TEXT_TAIL: usize = 512;

impl Doing {
    pub fn apply(&mut self, event: &RecordedEvent) {
        self.last_at_ms = self.last_at_ms.max(event.at_ms);
        match &event.activity {
            Activity::Prompt(_) | Activity::Harness(Event::TurnAccepted { .. }) => {
                self.tool = None;
                self.asking = None;
                self.text.clear();
            }
            Activity::Harness(Event::ToolStarted { name, .. }) => {
                self.tool = Some(name.clone());
                self.text.clear();
            }
            Activity::Harness(Event::MessageDelta { text, .. }) => {
                self.text.push_str(text);
                if self.text.len() > TEXT_TAIL * 2 {
                    let cut = self.text.len() - TEXT_TAIL;
                    let cut = (cut..self.text.len())
                        .find(|i| self.text.is_char_boundary(*i))
                        .unwrap_or(self.text.len());
                    self.text.drain(..cut);
                }
            }
            Activity::Harness(Event::PermissionRequested { request, .. }) => {
                self.asking = Some(request.tool.clone());
            }
            Activity::Harness(Event::PermissionWithdrawn { .. }) | Activity::Decision { .. } => {
                self.asking = None;
            }
            // A gateway call is what the harness is doing, as a tool is.
            Activity::ConnectorCall(call) => {
                self.tool = Some(format!(
                    "{} {} ({})",
                    call.connector, call.operation, call.decision
                ));
            }
            _ => {}
        }
    }

    /// The last line of the current message, whitespace collapsed.
    pub fn snippet(&self) -> String {
        self.text
            .lines()
            .rev()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .find(|line| !line.is_empty())
            .unwrap_or_default()
    }

    /// One line: a pending permission request, the tool, the snippet.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(tool) = &self.asking {
            parts.push(format!("asks {tool}"));
        }
        if let Some(tool) = &self.tool {
            parts.push(format!("▸ {tool}"));
        }
        let snippet = self.snippet();
        if !snippet.is_empty() {
            parts.push(snippet);
        }
        parts.join(" · ")
    }
}

struct Column {
    header: &'static str,
    max: usize,
    right: bool,
}

const COLUMNS: [Column; 6] = [
    Column {
        header: "BRANCH",
        max: 36,
        right: false,
    },
    Column {
        header: "HARNESS",
        max: 14,
        right: false,
    },
    Column {
        header: "STATUS",
        max: 26,
        right: false,
    },
    Column {
        header: "TURNS",
        max: 5,
        right: true,
    },
    Column {
        header: "COST",
        max: 9,
        right: true,
    },
    Column {
        header: "AGE",
        max: 4,
        right: true,
    },
];

/// Narrowest activity column worth showing.
const MIN_ACTIVITY: usize = 12;

fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(render::width(text)));
    match right {
        true => format!("{fill}{text}"),
        false => format!("{text}{fill}"),
    }
}

/// The tree as lines no wider than `width`: a column header, then one row
/// per branch with forks indented under their parents. Status is colored;
/// every cell is cut to fit before coloring.
pub fn rows(
    infos: &[BranchInfo],
    doing: &HashMap<String, Doing>,
    now_s: u64,
    width: usize,
    style: Style,
) -> Vec<String> {
    let cells: Vec<(Vec<String>, Tone, String)> = render::tree_order(infos)
        .into_iter()
        .map(|(depth, info)| {
            let name = match depth {
                0 => info.name.clone(),
                _ => format!("{}└ {}", "  ".repeat(depth - 1), info.name),
            };
            let (status, tone) = render::status_text(&info.status);
            let activity = doing
                .get(&info.name)
                .map(Doing::summary)
                .unwrap_or_default();
            let fixed = vec![
                name,
                info.harness.clone(),
                status,
                info.turns.to_string(),
                render::cost_text(info.cost_usd),
                render::age_text(now_s.saturating_sub(info.created_at)),
            ];
            let fixed = fixed
                .into_iter()
                .zip(&COLUMNS)
                .map(|(text, column)| render::truncate(&text, column.max))
                .collect();
            (fixed, tone, activity)
        })
        .collect();
    let widths: Vec<usize> = COLUMNS
        .iter()
        .enumerate()
        .map(|(i, column)| {
            cells
                .iter()
                .map(|(fixed, _, _)| render::width(&fixed[i]))
                .chain([column.header.len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let used: usize = widths.iter().sum::<usize>() + 2 * widths.len();
    let activity_width = width.saturating_sub(used);
    let show_activity = activity_width >= MIN_ACTIVITY;
    let line = |fixed: &[String], tone: Option<Tone>, activity: &str| {
        let mut out = String::new();
        let mut visible = 0;
        for (i, (text, column)) in fixed.iter().zip(&COLUMNS).enumerate() {
            let sep = if i > 0 { "  " } else { "" };
            let padded = pad(text, widths[i], column.right);
            // Cut the fixed part too, on a very narrow terminal.
            let room = width.saturating_sub(visible + sep.len());
            if room == 0 {
                break;
            }
            let padded = render::truncate(&padded, room);
            visible += sep.len() + render::width(&padded);
            out.push_str(sep);
            match (i, tone) {
                (2, Some(tone)) => out.push_str(&style.paint(tone, &padded)),
                _ => out.push_str(&padded),
            }
        }
        if show_activity && !activity.is_empty() {
            out.push_str("  ");
            out.push_str(&render::truncate(activity, activity_width));
        }
        out.trim_end().to_owned()
    };
    let header: Vec<String> = COLUMNS.iter().map(|c| c.header.to_owned()).collect();
    let mut header_line = line(&header, None, if show_activity { "ACTIVITY" } else { "" });
    if style.color {
        header_line = style.paint(Tone::Dim, &header_line);
    }
    let mut out = vec![header_line];
    for (fixed, tone, activity) in &cells {
        out.push(line(fixed, Some(*tone), activity));
    }
    out
}

/// A whole screen: title, blank line, the tree, and a total.
pub fn frame(
    title: &str,
    infos: &[BranchInfo],
    doing: &HashMap<String, Doing>,
    now_s: u64,
    width: usize,
    style: Style,
) -> Vec<String> {
    let mut lines = vec![render::truncate(title, width), String::new()];
    if infos.is_empty() {
        lines.push(render::truncate(
            "no branches yet; start one with: by run \"<prompt>\"",
            width,
        ));
        return lines;
    }
    lines.extend(rows(infos, doing, now_s, width, style));
    let running = infos
        .iter()
        .filter(|i| i.status == branchyard::BranchStatus::Running)
        .count();
    let cost: f64 = infos.iter().filter_map(|i| i.cost_usd).sum();
    let reported = infos.iter().any(|i| i.cost_usd.is_some());
    let mut total = format!(
        "{} {}, {running} running",
        infos.len(),
        if infos.len() == 1 {
            "branch"
        } else {
            "branches"
        }
    );
    if reported {
        total.push_str(&format!(", {} reported", render::usd(cost)));
    }
    lines.push(String::new());
    lines.push(style.paint(Tone::Dim, &render::truncate(&total, width)));
    lines
}

/// Log lines for what changed since `previous` (branch name to its last
/// logged text), updating it. Removed branches are logged once.
pub fn changes(
    previous: &mut HashMap<String, String>,
    infos: &[BranchInfo],
    doing: &HashMap<String, Doing>,
    now_ms: u64,
) -> Vec<String> {
    let stamp = render::timestamp(now_ms);
    let mut out = Vec::new();
    for info in infos {
        let (status, _) = render::status_text(&info.status);
        let activity = doing
            .get(&info.name)
            .map(Doing::summary)
            .unwrap_or_default();
        let mut text = format!(
            "{status}  turns {}  cost {}",
            info.turns,
            render::cost_text(info.cost_usd)
        );
        if !activity.is_empty() {
            text.push_str(&format!("  {}", render::truncate(&activity, 100)));
        }
        if previous.get(&info.name) != Some(&text) {
            out.push(format!("{stamp}  {}  {text}", info.name));
            previous.insert(info.name.clone(), text);
        }
    }
    let present: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    let gone: Vec<String> = previous
        .keys()
        .filter(|name| !present.contains(name.as_str()))
        .cloned()
        .collect();
    for name in gone {
        previous.remove(&name);
        out.push(format!("{stamp}  {name}  removed"));
    }
    out
}

/// Where branches and their events come from.
enum Source {
    Local {
        yard: Yard,
        /// Feed position of the last event read.
        cursor: u64,
    },
    Remote {
        label: String,
        repo: Repo,
        events: Receiver<Result<FeedEntry, branchyard_client::Error>>,
        lost: Option<String>,
    },
}

impl Source {
    fn label(&self) -> String {
        match self {
            Source::Local { yard, .. } => yard.root().display().to_string(),
            Source::Remote { label, lost, .. } => match lost {
                None => label.clone(),
                Some(error) => format!("{label} (event stream lost: {error})"),
            },
        }
    }

    /// `by diff`'s text for a branch.
    fn diff(&self, branch: &str) -> Result<String, String> {
        match self {
            Source::Local { yard, .. } => yard
                .branch(branch)
                .and_then(|branch| branch.diff())
                .map_err(|e| e.to_string()),
            Source::Remote { repo, .. } => repo.diff(branch).map_err(|e| e.to_string()),
        }
    }

    /// `by log`'s text for a branch, without color.
    fn log(&self, branch: &str) -> Result<String, String> {
        let events = match self {
            Source::Local { yard, .. } => yard
                .branch(branch)
                .and_then(|branch| branch.events())
                .map_err(|e| e.to_string())?,
            Source::Remote { repo, .. } => {
                repo.events(branch, 0).map_err(|e| e.to_string())?.events
            }
        };
        Ok(render::log_text(&events, Style { color: false }))
    }

    /// `by show`'s checkpoint list for a branch, without color: what `r`
    /// chooses from. Local only, as `by rewind` is.
    fn checkpoints(&self, branch: &str) -> Result<String, String> {
        match self {
            Source::Local { yard, .. } => yard
                .branch(branch)
                .and_then(|branch| branch.checkpoints())
                .map(|list| {
                    crate::attempts::checkpoint_lines(&list, Style { color: false })
                        .lines()
                        .skip_while(|l| l.trim().is_empty() || l.trim() == "checkpoints")
                        .map(|l| format!("{l}\n"))
                        .collect()
                })
                .map_err(|e| e.to_string()),
            Source::Remote { .. } => Err("by rewind is local only".into()),
        }
    }

    /// `by compare`'s table for a branch and its siblings, without color.
    fn compare(&self, branch: &str) -> Result<String, String> {
        let infos = self.branches().map_err(|e| e.to_string())?;
        let names = actions::siblings(&infos, branch);
        let attempts = match self {
            Source::Local { yard, .. } => yard.compare(&names, false).map_err(|e| e.to_string()),
            Source::Remote { repo, .. } => {
                crate::attempts::remote_attempts(repo, &names).map_err(|e| e.to_string())
            }
        }?;
        let mut text = crate::attempts::compare_table(&attempts, Style { color: false });
        if names.len() == 1 {
            text.push_str(&format!(
                "\n{branch} has no siblings: no other child of its parent, and no other branch \
                 of its by fan\n"
            ));
        } else {
            text.push_str(&format!(
                "\npick one with: by compare {} --pick BRANCH\n",
                names.join(" ")
            ));
        }
        Ok(text)
    }

    /// The branch `by try` has applied here (never remotely).
    fn trying(&self) -> Option<String> {
        match self {
            Source::Local { yard, .. } => yard.try_recorded().ok().flatten().map(|s| s.branch),
            Source::Remote { .. } => None,
        }
    }

    /// How `by open` would open a branch's worktree.
    fn editor(&self, branch: &str) -> Result<crate::open::Plan, String> {
        let Source::Local { yard, .. } = self else {
            return Err("by open is local only: the worktree is on the server".into());
        };
        let info = yard
            .branch(branch)
            .map_err(|e| e.to_string())?
            .info()
            .clone();
        if !info.worktree.is_dir() {
            return Err(format!(
                "{branch} has no worktree at {}",
                info.worktree.display()
            ));
        }
        crate::open::plan(&info.worktree, None, &|name| std::env::var(name).ok())
            .map_err(|e| e.to_string())
    }

    fn branches(&self) -> Result<Vec<BranchInfo>, Failure> {
        Ok(match self {
            Source::Local { yard, .. } => {
                // The gateway's newest calls, as connector_call events.
                let _ = yard.ingest_connector_audit();
                yard.branches()?
            }
            Source::Remote { repo, .. } => repo.branches()?,
        })
    }

    /// Events recorded since the last call, by branch.
    fn events(&mut self, infos: &[BranchInfo]) -> Vec<(String, RecordedEvent)> {
        let mut out = Vec::new();
        match self {
            Source::Local { yard, cursor } => {
                let names: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();
                // An unreadable store just shows no activity this round.
                while let Ok(page) = yard.events_since(*cursor, FEED_BATCH) {
                    if page.events.is_empty() {
                        break;
                    }
                    *cursor = page.next_cursor;
                    out.extend(
                        page.events
                            .into_iter()
                            .filter(|e| names.contains(e.branch.as_str()))
                            .map(|e| (e.branch, e.event)),
                    );
                }
            }
            Source::Remote { events, lost, .. } => loop {
                match events.try_recv() {
                    Ok(Ok(entry)) => out.push((
                        entry.branch,
                        RecordedEvent {
                            at_ms: entry.at_ms,
                            activity: entry.activity,
                        },
                    )),
                    Ok(Err(error)) => *lost = Some(error.to_string()),
                    Err(_) => break,
                }
            },
        }
        out
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bytes from `reader`, on a thread, until it ends.
fn keys(mut reader: impl Read + Send + 'static) -> Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while let Ok(1) = reader.read(&mut byte) {
            if tx.send(byte[0]).is_err() {
                return;
            }
        }
    });
    rx
}

fn quits(key: u8) -> bool {
    // q, Q, Ctrl-C, Ctrl-D.
    matches!(key, b'q' | b'Q' | 3 | 4)
}

/// Wait up to `interval` for a quitting key. True to quit.
fn wait(keys: Option<&Receiver<u8>>, interval: Duration) -> bool {
    let Some(keys) = keys else {
        std::thread::sleep(interval);
        return false;
    };
    let deadline = std::time::Instant::now() + interval;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match keys.recv_timeout(left) {
            Ok(key) if quits(key) => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            // Input ended: keep watching until killed.
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(left);
                return false;
            }
        }
    }
}

/// The width `--once` draws to: the terminal's, else `COLUMNS` (as a
/// pager or script sets it), else 120.
fn once_width() -> usize {
    let terminal = io::stdout()
        .is_terminal()
        .then(|| ratatui::crossterm::terminal::size().ok())
        .flatten()
        .map(|(columns, _)| usize::from(columns))
        .filter(|c| *c > 0);
    terminal
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|c| c.parse().ok())
                .filter(|c| *c > 0)
        })
        .unwrap_or(120)
}

pub fn run(env: &Env, target: &Target, interval: Duration, once: bool) -> Outcome {
    let mut source = match target {
        Target::Local => Source::Local {
            yard: commands::open()?,
            cursor: 0,
        },
        Target::Remote(remote) => {
            let (tx, rx) = mpsc::channel();
            let stream = remote.repo.stream(Some(0));
            std::thread::spawn(move || {
                for item in stream {
                    if tx.send(item).is_err() {
                        return;
                    }
                }
            });
            Source::Remote {
                label: remote.label(),
                repo: remote.repo.clone(),
                events: rx,
                lost: None,
            }
        }
    };
    let style = Style { color: env.color };
    let mut doing: HashMap<String, Doing> = HashMap::new();
    if once {
        // Remote history arrives on the stream; give it a moment.
        if matches!(source, Source::Remote { .. }) {
            std::thread::sleep(Duration::from_millis(300));
        }
        let infos = update(&mut source, &mut doing, |_, _| {})?;
        let title = format!("by watch · {}", source.label());
        let lines = frame(&title, &infos, &doing, now_ms() / 1000, once_width(), style);
        return commands::print(&(lines.join("\n") + "\n"));
    }
    if !env.stdout_tty {
        let input = keys(io::stdin());
        let mut previous = HashMap::new();
        // Escapes on stderr if that is still a terminal; the first round
        // is history and only primes it.
        let notifier = env.notifier();
        let mut primed = false;
        loop {
            let now = now_ms();
            let infos = update(&mut source, &mut doing, |branch, event| {
                if let Some(notifier) = notifier.as_ref().filter(|_| primed) {
                    if notify::fresh(event.at_ms, now) {
                        notifier.observe(branch, &event.activity);
                    }
                }
            })?;
            primed = true;
            let lines = changes(&mut previous, &infos, &doing, now_ms());
            if !lines.is_empty() {
                commands::print(&(lines.join("\n") + "\n"))?;
            }
            if wait(Some(&input), interval) {
                return Ok(());
            }
        }
    }
    let label = source.label();
    let cockpit = Cockpit::new(source, target, env.notify);
    let remote = matches!(target, Target::Remote(_));
    match tui::run(label, remote, interval, cockpit) {
        Ok(outcome) => outcome,
        Err(error) => Err(error.into()),
    }
}

/// The dashboard's effects: its data source, the `by` its actions run,
/// and how it notifies.
struct Cockpit {
    source: Source,
    runner: Runner,
    notify: notify::Settings,
    /// `by usage`'s header line and listening ports, refreshed less often
    /// than the branches (they read files and `/proc`), locally only.
    extras: Extras,
}

/// What the dashboard shows beyond the branches, with when each was read.
#[derive(Default)]
struct Extras {
    usage: Option<String>,
    usage_at: Option<std::time::Instant>,
    ports: std::collections::BTreeMap<String, Vec<String>>,
    ports_at: Option<std::time::Instant>,
}

/// How often the usage meters are read again.
const USAGE_EVERY: Duration = Duration::from_secs(60);
/// How often the listening ports are scanned again.
const PORTS_EVERY: Duration = Duration::from_secs(5);

impl Extras {
    fn refresh(&mut self, source: &Source) {
        let Source::Local { yard, .. } = source else {
            return;
        };
        let stale = |at: Option<std::time::Instant>, every: Duration| {
            at.is_none_or(|at| at.elapsed() >= every)
        };
        if stale(self.usage_at, USAGE_EVERY) {
            let cwd = yard.root().to_path_buf();
            let vars = |name: &str| std::env::var(name).ok();
            let config = crate::defaults::config_at(&cwd, &vars)
                .ok()
                .flatten()
                .map(|c| c.usage)
                .unwrap_or_default();
            let logins = crate::usage::meter(&config, &vars, crate::usage::now_ms());
            self.usage = crate::usage::header(&logins);
            self.usage_at = Some(std::time::Instant::now());
        }
        if stale(self.ports_at, PORTS_EVERY) {
            self.ports = crate::ports::of_yard(yard)
                .into_iter()
                .map(|(branch, listeners)| (branch, crate::ports::lines(&listeners)))
                .collect();
            self.ports_at = Some(std::time::Instant::now());
        }
    }
}

/// Runs `by` for the dashboard's actions.
struct Runner {
    /// This executable, which the actions run with `globals` before their
    /// own arguments.
    by: Option<PathBuf>,
    globals: Vec<String>,
    /// Where a local yard's commands run.
    root: Option<PathBuf>,
    /// Where background commands write their output.
    logs: PathBuf,
}

/// Lines of a background command's log shown when it ends.
const LOG_TAIL: usize = 40;

impl Cockpit {
    fn new(source: Source, target: &Target, notify: notify::Settings) -> Cockpit {
        let (mut globals, root, logs) = match (&source, target) {
            (Source::Local { yard, .. }, _) => (
                Vec::new(),
                Some(yard.root().to_path_buf()),
                yard.root().join(".branchyard").join("watch"),
            ),
            (Source::Remote { .. }, Target::Remote(remote)) => (
                remote.args.clone(),
                None,
                std::env::temp_dir().join("branchyard-watch"),
            ),
            (Source::Remote { .. }, Target::Local) => (
                Vec::new(),
                None,
                std::env::temp_dir().join("branchyard-watch"),
            ),
        };
        // The dashboard notifies; the commands it starts do not, too.
        globals.push(notify::Settings::CHILD_FLAG.to_owned());
        Cockpit {
            source,
            notify,
            extras: Extras::default(),
            runner: Runner {
                by: std::env::current_exe().ok(),
                globals,
                root,
                logs,
            },
        }
    }
}

impl Runner {
    fn command(&self, invocation: &tui::Invocation) -> Result<Command, String> {
        let by = self
            .by
            .as_ref()
            .ok_or("cannot find the by executable to run")?;
        let mut command = Command::new(by);
        command
            .args(&self.globals)
            .args(&invocation.argv)
            .stdin(Stdio::null())
            .env("NO_COLOR", "1");
        if let Some(root) = &self.root {
            command.current_dir(root);
        }
        Ok(command)
    }

    /// Run `invocation`, reporting on `done` from another thread.
    fn run(&self, invocation: tui::Invocation, done: &Sender<tui::Msg>) {
        let finished = |ok: bool, output: String| tui::Msg::Done {
            action: invocation.action,
            branch: invocation.branch.clone(),
            ok,
            output,
        };
        let mut command = match self.command(&invocation) {
            Ok(command) => command,
            Err(error) => {
                let _ = done.send(finished(false, error));
                return;
            }
        };
        if invocation.terminal {
            // The dashboard has left the screen; the command has the
            // terminal until it exits.
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .env_remove("NO_COLOR");
            let msg = match command.status() {
                Ok(status) => finished(
                    status.success(),
                    match status.success() {
                        true => "done".to_owned(),
                        false => format!("by exited with {status}"),
                    },
                ),
                Err(error) => finished(false, format!("could not start by: {error}")),
            };
            let _ = done.send(msg);
            return;
        }
        let done = done.clone();
        if !invocation.background {
            std::thread::spawn(move || {
                let msg = match command.output() {
                    Ok(out) => {
                        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                        text.push_str(&String::from_utf8_lossy(&out.stderr));
                        tui::Msg::Done {
                            action: invocation.action,
                            branch: invocation.branch,
                            ok: out.status.success(),
                            output: text,
                        }
                    }
                    Err(error) => tui::Msg::Done {
                        action: invocation.action,
                        branch: invocation.branch,
                        ok: false,
                        output: format!("could not run by: {error}"),
                    },
                };
                let _ = done.send(msg);
            });
            return;
        }
        let log = self.logs.join(format!(
            "{}-{}-{}.log",
            invocation
                .branch
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                })
                .collect::<String>(),
            actions::by_id(invocation.action).name.replace(' ', "-"),
            now_ms()
        ));
        let started = std::fs::create_dir_all(&self.logs)
            .and_then(|()| File::create(&log))
            .and_then(|file| Ok((file.try_clone()?, file)))
            .and_then(|(out, err)| {
                command.stdout(out).stderr(err);
                // Its own process group: the terminal's signals, and the
                // dashboard quitting, leave it running.
                #[cfg(unix)]
                std::os::unix::process::CommandExt::process_group(&mut command, 0);
                command.spawn()
            });
        let mut child = match started {
            Ok(child) => child,
            Err(error) => {
                let _ = done.send(finished(false, format!("could not start by: {error}")));
                return;
            }
        };
        let _ = done.send(tui::Msg::Started {
            action: invocation.action,
            branch: invocation.branch.clone(),
            log: log.display().to_string(),
        });
        std::thread::spawn(move || {
            let ok = child.wait().is_ok_and(|status| status.success());
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(LOG_TAIL)..].join("\n");
            let _ = done.send(tui::Msg::Done {
                action: invocation.action,
                branch: invocation.branch,
                ok,
                output: tail,
            });
        });
    }
}

impl tui::Effects for Cockpit {
    type Error = Failure;

    fn editor(&mut self, branch: &str) -> Result<crate::open::Plan, String> {
        self.source.editor(branch)
    }

    fn refresh(&mut self) -> Result<tui::Snapshot, Failure> {
        let infos = self.source.branches()?;
        let events = self.source.events(&infos);
        self.extras.refresh(&self.source);
        Ok(tui::Snapshot {
            label: self.source.label(),
            infos,
            events,
            now_ms: now_ms(),
            trying: self.source.trying(),
            usage: self.extras.usage.clone(),
            ports: self.extras.ports.clone(),
        })
    }

    fn perform(&mut self, cmd: tui::Cmd, done: &Sender<tui::Msg>) {
        match cmd {
            tui::Cmd::Quit => {}
            tui::Cmd::Run(invocation) => self.runner.run(invocation, done),
            tui::Cmd::Load { kind, branch } => {
                let result = match kind {
                    actions::PaneKind::Diff => self.source.diff(&branch),
                    actions::PaneKind::Log => self.source.log(&branch),
                    actions::PaneKind::Checkpoints => self.source.checkpoints(&branch),
                    actions::PaneKind::Compare => self.source.compare(&branch),
                    actions::PaneKind::Output => Err("nothing to load".into()),
                };
                let _ = done.send(tui::Msg::Loaded {
                    kind,
                    branch,
                    result,
                });
            }
            tui::Cmd::Notify(notice) => {
                // The dashboard's own terminal.
                let mut out: Option<Box<dyn Write + Send>> = Some(Box::new(io::stdout()));
                notify::show(&self.notify, &notice, &mut out);
            }
            // The dashboard opens editors itself; see `tui::run`.
            tui::Cmd::Open(_) => {}
            tui::Cmd::Copy(text) => {
                let mut stdout = io::stdout();
                let _ =
                    stdout.write_all(osc52(&text, std::env::var_os("TMUX").is_some()).as_bytes());
                let _ = stdout.flush();
            }
        }
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, as OSC 52 takes it.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Wrap an escape sequence so tmux passes it to the terminal outside it.
pub fn tmux_passthrough(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// The OSC 52 sequence that sets the clipboard to `text`.
fn osc52(text: &str, tmux: bool) -> String {
    let sequence = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    match tmux {
        true => tmux_passthrough(&sequence),
        false => sequence,
    }
}

/// Refresh the branch list and apply new events.
fn update(
    source: &mut Source,
    doing: &mut HashMap<String, Doing>,
    mut each: impl FnMut(&str, &RecordedEvent),
) -> Result<Vec<BranchInfo>, Failure> {
    let infos = source.branches()?;
    for (branch, event) in source.events(&infos) {
        each(&branch, &event);
        doing.entry(branch).or_default().apply(&event);
    }
    let names: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    doing.retain(|name, _| names.contains(name.as_str()));
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{BranchStatus, PermissionKey, PermissionRequest};
    use std::path::PathBuf;

    fn info(name: &str, parent: Option<&str>, status: BranchStatus) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: PathBuf::from("/w"),
            prompt: "p".into(),
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

    fn at(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn delta(text: &str) -> Activity {
        Activity::Harness(Event::MessageDelta {
            turn: 1,
            text: text.into(),
        })
    }

    #[test]
    fn activity_tracks_tool_permission_and_last_line() {
        let mut doing = Doing::default();
        doing.apply(&at(1, Activity::Prompt("go".into())));
        doing.apply(&at(2, delta("Reading the parser\nnow   running")));
        doing.apply(&at(3, delta(" tests\n\n")));
        assert_eq!(doing.summary(), "now running tests");
        doing.apply(&at(
            4,
            Activity::Harness(Event::ToolStarted {
                turn: 1,
                call_id: "c".into(),
                name: "Bash".into(),
            }),
        ));
        doing.apply(&at(
            5,
            Activity::Harness(Event::PermissionRequested {
                turn: Some(1),
                request: PermissionRequest {
                    key: PermissionKey("k".into()),
                    tool: "Bash".into(),
                    input: serde_json::Value::Null,
                },
            }),
        ));
        assert_eq!(doing.summary(), "asks Bash · ▸ Bash");
        doing.apply(&at(
            6,
            Activity::Decision {
                tool: "Bash".into(),
                allowed: true,
                message: None,
                source: branchyard::DecisionSource::Default,
            },
        ));
        assert_eq!(doing.summary(), "▸ Bash");
        assert_eq!(doing.last_at_ms, 6);
        doing.apply(&at(7, delta(&"é".repeat(2_000))));
        assert!(doing.text.len() <= TEXT_TAIL * 2 + 2);
    }

    #[test]
    fn the_tree_indents_forks_and_fits_the_width() {
        let infos = vec![
            info("parser", None, BranchStatus::Running),
            info("parser-alt", Some("parser"), BranchStatus::Ready),
            info("docs", None, BranchStatus::Failed { reason: "x".into() }),
        ];
        let mut doing = HashMap::new();
        let mut busy = Doing::default();
        busy.apply(&at(
            1,
            delta("Rewriting the tokenizer to handle nested quotes"),
        ));
        doing.insert("parser".to_owned(), busy);
        let lines = rows(&infos, &doing, 1_130, 90, Style::PLAIN);
        assert_eq!(
            lines,
            [
                "BRANCH        HARNESS  STATUS     TURNS   COST  AGE  ACTIVITY",
                "parser        codex    running        2  $0.50   2m  Rewriting the tokenizer to handle ne…",
                "└ parser-alt  codex    ready          2  $0.50   2m",
                "docs          codex    failed: x      2  $0.50   2m",
            ]
        );
        for line in rows(&infos, &doing, 1_130, 40, Style::PLAIN) {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
        let narrow = rows(&infos, &doing, 1_130, 50, Style::PLAIN);
        assert!(!narrow[0].contains("ACTIVITY"), "{narrow:?}");
    }

    #[test]
    fn color_does_not_change_the_layout() {
        let infos = vec![info("a", None, BranchStatus::Ready)];
        let plain = rows(&infos, &HashMap::new(), 1_000, 80, Style::PLAIN);
        let color = rows(&infos, &HashMap::new(), 1_000, 80, Style { color: true });
        let strip = |s: &str| {
            let mut out = String::new();
            let mut escape = false;
            for c in s.chars() {
                match (escape, c) {
                    (false, '\x1b') => escape = true,
                    (true, 'm') => escape = false,
                    (true, _) => {}
                    (false, c) => out.push(c),
                }
            }
            out
        };
        assert_eq!(strip(&color[1]), plain[1]);
        assert!(color[1].contains("\x1b[32mready"), "{:?}", color[1]);
    }

    #[test]
    fn frames_have_a_title_and_a_total() {
        let infos = vec![
            info("a", None, BranchStatus::Running),
            info("b", None, BranchStatus::NoChanges),
        ];
        let lines = frame(
            "by watch · /r",
            &infos,
            &HashMap::new(),
            1_000,
            80,
            Style::PLAIN,
        );
        assert_eq!(lines[0], "by watch · /r");
        assert_eq!(
            lines.last().unwrap(),
            "2 branches, 1 running, $1.00 reported"
        );
        let empty = frame("t", &[], &HashMap::new(), 0, 80, Style::PLAIN);
        assert!(empty[2].starts_with("no branches yet"));
    }

    /// A `by` that prints its arguments and fails when asked to.
    fn fake_by(dir: &std::path::Path) -> Runner {
        use std::os::unix::fs::PermissionsExt;
        let by = dir.join("by");
        std::fs::write(
            &by,
            "#!/bin/sh\necho \"args: $*\"\necho \"color: $NO_COLOR\" >&2\n\
             case \"$*\" in *fail*) exit 3;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
        Runner {
            by: Some(by),
            globals: vec!["--remote".into(), "http://x".into(), "--no-notify".into()],
            root: Some(dir.to_path_buf()),
            logs: dir.join("logs"),
        }
    }

    fn invocation(argv: &[&str], background: bool) -> tui::Invocation {
        tui::Invocation {
            action: actions::ActionId::Send,
            branch: "a/b".into(),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            background,
            terminal: false,
        }
    }

    #[test]
    fn actions_run_by_with_the_global_flags_and_report_back() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake_by(dir.path());
        let (tx, rx) = mpsc::channel();
        let wait = Duration::from_secs(20);

        runner.run(invocation(&["cancel", "--", "ok"], false), &tx);
        match rx.recv_timeout(wait).unwrap() {
            tui::Msg::Done { ok, output, .. } => {
                assert!(ok);
                assert_eq!(
                    output,
                    "args: --remote http://x --no-notify cancel -- ok\ncolor: 1\n"
                );
            }
            other => panic!("{other:?}"),
        }
        runner.run(invocation(&["cancel", "--", "fail"], false), &tx);
        assert!(matches!(
            rx.recv_timeout(wait).unwrap(),
            tui::Msg::Done { ok: false, .. }
        ));

        // In the background: started, with its log, then done with the log's end.
        runner.run(invocation(&["send", "--", "fail", "go"], true), &tx);
        let log = match rx.recv_timeout(wait).unwrap() {
            tui::Msg::Started { log, .. } => log,
            other => panic!("{other:?}"),
        };
        assert!(log.starts_with(
            &dir.path()
                .join("logs")
                .join("a_b-send-")
                .display()
                .to_string()
        ));
        match rx.recv_timeout(wait).unwrap() {
            tui::Msg::Done { ok, output, .. } => {
                assert!(!ok);
                assert!(
                    output.contains("args: --remote http://x --no-notify send -- fail go"),
                    "{output}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(std::fs::read_to_string(&log).unwrap().contains("color: 1"));

        // No executable: a failure, not a hang.
        let missing = Runner {
            by: None,
            ..fake_by(dir.path())
        };
        missing.run(invocation(&["send"], true), &tx);
        assert!(matches!(
            rx.recv_timeout(wait).unwrap(),
            tui::Msg::Done { ok: false, output, .. } if output.contains("cannot find the by executable")
        ));
    }

    #[test]
    fn clipboard_escapes_are_osc_52_in_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"by/impl-2"), "YnkvaW1wbC0y");
        assert_eq!(base64("é".as_bytes()), "w6k=");
        assert_eq!(osc52("impl", false), "\x1b]52;c;aW1wbA==\x07");
        assert_eq!(
            osc52("impl", true),
            "\x1bPtmux;\x1b\x1b]52;c;aW1wbA==\x07\x1b\\"
        );
    }

    #[test]
    fn the_log_fallback_prints_only_changes() {
        let mut previous = HashMap::new();
        let mut infos = vec![info("a", None, BranchStatus::Running)];
        let first = changes(&mut previous, &infos, &HashMap::new(), 0);
        assert_eq!(
            first,
            ["1970-01-01T00:00:00.000Z  a  running  turns 2  cost $0.50"]
        );
        assert!(changes(&mut previous, &infos, &HashMap::new(), 1).is_empty());
        let mut doing = HashMap::new();
        let mut busy = Doing::default();
        busy.apply(&at(1, delta("thinking")));
        doing.insert("a".to_owned(), busy);
        assert_eq!(
            changes(&mut previous, &infos, &doing, 2),
            ["1970-01-01T00:00:00.002Z  a  running  turns 2  cost $0.50  thinking"]
        );
        infos[0].status = BranchStatus::Ready;
        infos.push(info("b", None, BranchStatus::Running));
        let next = changes(&mut previous, &infos, &doing, 3);
        assert_eq!(next.len(), 2);
        assert!(next[0].contains("a  ready"), "{next:?}");
        infos.remove(0);
        assert_eq!(
            changes(&mut previous, &infos, &doing, 4),
            ["1970-01-01T00:00:00.004Z  a  removed"]
        );
    }
}
