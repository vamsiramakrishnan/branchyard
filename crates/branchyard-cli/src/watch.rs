//! `by watch`: a live tree of every branch with its status, harness,
//! current activity, cost, turns and age.
//!
//! On a terminal it redraws in place with plain ANSI on the alternate
//! screen, reading keys through `stty` (no terminal library): `q` or
//! Ctrl-C exits and restores the terminal. Otherwise it appends one line
//! per change. Activity comes from reading event logs incrementally:
//! locally by byte offset in `.branchyard/events/`, remotely from the
//! server's event stream.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use branchyard::{Activity, BranchInfo, Event, RecordedEvent, Yard};
use branchyard_client::api::FeedEntry;
use branchyard_client::Repo;
use branchyard_server::tail::{self, LogTail};

use crate::commands::{self, Env, Failure, Outcome, Target};
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
    let fill = " ".repeat(width.saturating_sub(text.chars().count()));
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
                .map(|(fixed, _, _)| fixed[i].chars().count())
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
            visible += sep.len() + padded.chars().count();
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
        tails: HashMap<String, LogTail>,
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

    fn branches(&self) -> Result<Vec<BranchInfo>, Failure> {
        Ok(match self {
            Source::Local { yard, .. } => yard.branches()?,
            Source::Remote { repo, .. } => repo.branches()?,
        })
    }

    /// Events recorded since the last call, by branch.
    fn events(&mut self, infos: &[BranchInfo]) -> Vec<(String, RecordedEvent)> {
        let mut out = Vec::new();
        match self {
            Source::Local { yard, tails } => {
                let names: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();
                tails.retain(|name, _| names.contains(name.as_str()));
                for info in infos {
                    let tail = tails
                        .entry(info.name.clone())
                        .or_insert_with(|| LogTail::new(tail::log_path(yard.root(), &info.name)));
                    // A log being rotated or unreadable just shows no
                    // activity this round.
                    for line in tail.read().unwrap_or_default() {
                        if let Ok(event) = line.event {
                            out.push((info.name.clone(), event));
                        }
                    }
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

/// The controlling terminal in no-echo, byte-at-a-time mode with signals
/// off (so Ctrl-C arrives as a key), showing the alternate screen. Dropping
/// it restores everything.
struct Screen {
    tty: File,
    saved: String,
}

fn stty(tty: &File, args: &[&str]) -> Option<String> {
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::from(tty.try_clone().ok()?))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Screen {
    fn enter() -> Option<Screen> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let saved = stty(&tty, &["-g"])?;
        stty(
            &tty,
            &["-icanon", "-echo", "-isig", "min", "1", "time", "0"],
        )?;
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049h\x1b[?25l");
        let _ = stdout.flush();
        Some(Screen { tty, saved })
    }

    /// Rows and columns.
    fn size(&self) -> Option<(usize, usize)> {
        let text = stty(&self.tty, &["size"])?;
        let mut parts = text.split_whitespace().map(|n| n.parse::<usize>().ok());
        match (parts.next()??, parts.next()??) {
            (rows, cols) if rows > 0 && cols > 0 => Some((rows, cols)),
            _ => None,
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
        stty(&self.tty, &[&self.saved]);
    }
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

fn columns_from_env() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|c| *c > 0)
        .unwrap_or(120)
}

pub fn run(env: &Env, target: &Target, interval: Duration, once: bool) -> Outcome {
    let mut source = match target {
        Target::Local => Source::Local {
            yard: commands::open()?,
            tails: HashMap::new(),
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
        let infos = update(&mut source, &mut doing)?;
        let title = format!("by watch · {}", source.label());
        let lines = frame(
            &title,
            &infos,
            &doing,
            now_ms() / 1000,
            columns_from_env(),
            style,
        );
        return commands::print(&(lines.join("\n") + "\n"));
    }
    if !env.stdout_tty {
        let input = keys(io::stdin());
        let mut previous = HashMap::new();
        loop {
            let infos = update(&mut source, &mut doing)?;
            let lines = changes(&mut previous, &infos, &doing, now_ms());
            if !lines.is_empty() {
                commands::print(&(lines.join("\n") + "\n"))?;
            }
            if wait(Some(&input), interval) {
                return Ok(());
            }
        }
    }
    let screen = Screen::enter();
    let input = match &screen {
        Some(screen) => screen.tty.try_clone().ok().map(keys),
        None => Some(keys(io::stdin())),
    };
    loop {
        let infos = update(&mut source, &mut doing)?;
        let (rows, cols) = screen
            .as_ref()
            .and_then(Screen::size)
            .unwrap_or((40, columns_from_env()));
        let title = format!("by watch · {} · q to quit", source.label());
        let mut lines = frame(&title, &infos, &doing, now_ms() / 1000, cols, style);
        if lines.len() > rows.max(2) {
            let hidden = lines.len() - (rows.max(2) - 1);
            lines.truncate(rows.max(2) - 1);
            lines.push(format!(
                "… {hidden} more lines; widen or heighten the terminal"
            ));
        }
        let mut text = String::from("\x1b[H");
        for line in &lines {
            text.push_str(line);
            text.push_str("\x1b[K\r\n");
        }
        text.push_str("\x1b[J");
        if screen.is_none() {
            // Without the alternate screen, start from a clean one.
            text.insert_str(0, "\x1b[2J");
        }
        commands::print(&text)?;
        if wait(input.as_ref(), interval) {
            return Ok(());
        }
    }
}

/// Refresh the branch list and apply new events.
fn update(
    source: &mut Source,
    doing: &mut HashMap<String, Doing>,
) -> Result<Vec<BranchInfo>, Failure> {
    let infos = source.branches()?;
    for (branch, event) in source.events(&infos) {
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
            base: "b".into(),
            candidate: None,
            status,
            turns: 2,
            cost_usd: Some(0.5),
            created_at: 1_000,
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
