//! Telling a person when a branch needs them or ends: a permission request,
//! a question or escalation between branches, a stall, a failure, an
//! interruption, or a finished turn. `by watch` and a waiting `by run`,
//! `by fan`, `by send` or `by fork` feed their activity through a
//! [`Tracker`], which decides what is worth a notice and says each once.
//!
//! A notice is a terminal bell plus a desktop-notification escape the
//! terminal turns into a system notification: OSC 9 (iTerm2, WezTerm,
//! kitty, Ghostty, Windows Terminal) or OSC 777 (foot, urxvt, VTE
//! terminals such as GNOME Terminal); a terminal that knows neither ignores
//! it. Inside tmux the escape is wrapped to pass through. Nothing needs
//! D-Bus. With `[notify] desktop = true` in `branchyard.toml` it also runs
//! `notify-send` or, on macOS, `osascript`. `--no-notify` or
//! `[notify] enabled = false` turns it all off.

use branchyard_support::LockExt as _;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use branchyard::{Activity, BranchStatus, Event, MessageKind};
use branchyard_setup::config::{Notify, NotifyTerminal};

/// Why a branch wants attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A tool is waiting for permission.
    Permission,
    /// A branch asked or escalated to another.
    Question,
    Stalled,
    /// Failed or blocked.
    Failed,
    Interrupted,
    /// Ready, no changes, or stopped at its budget.
    Finished,
}

/// One notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub branch: String,
    pub kind: Kind,
    /// One line, such as `parser asks to use Bash`.
    pub text: String,
}

/// Events older than this when first seen are history, not news: `by
/// watch` reads a branch's whole log when it starts.
pub const FRESH_MS: u64 = 15_000;

/// Whether an event recorded at `at_ms` is news at `now_ms`.
pub fn fresh(at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(at_ms) <= FRESH_MS
}

/// Notices shown per line of text, at most.
const TEXT_MAX: usize = 160;

fn short(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::render::truncate(&line, TEXT_MAX)
}

/// Which activity is worth a notice, each once.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    /// Keys of the events already noticed: a permission request by its
    /// key, a message by its ID, a stall by when activity stopped.
    seen: HashSet<String>,
    /// The status each branch was last noticed in, so a status recorded
    /// again (as recovery may) is not said twice; a new turn clears it.
    status: HashMap<String, Kind>,
}

impl Tracker {
    /// The notice for `activity` on `branch`, if it deserves one that has
    /// not been given yet.
    pub fn observe(&mut self, branch: &str, activity: &Activity) -> Option<Notice> {
        let notice = |kind, text: String| {
            Some(Notice {
                branch: branch.to_owned(),
                kind,
                text,
            })
        };
        match activity {
            Activity::Harness(Event::PermissionRequested { request, .. }) => {
                if !self
                    .seen
                    .insert(format!("permission:{branch}:{}", request.key.0))
                {
                    return None;
                }
                notice(
                    Kind::Permission,
                    format!("{branch} asks to use {}", request.tool),
                )
            }
            // Recorded on both branches' logs: one notice per message.
            Activity::Message(message)
                if matches!(
                    message.kind,
                    MessageKind::Question | MessageKind::Escalation
                ) =>
            {
                if !self.seen.insert(format!("message:{}", message.id)) {
                    return None;
                }
                let verb = match message.kind {
                    MessageKind::Question => "asks",
                    _ => "escalates to",
                };
                notice(
                    Kind::Question,
                    short(&format!(
                        "{} {verb} {}: {}",
                        message.from, message.to, message.text
                    )),
                )
            }
            Activity::Stalled { since_ms } => {
                if !self.seen.insert(format!("stall:{branch}:{since_ms}")) {
                    return None;
                }
                notice(
                    Kind::Stalled,
                    format!("{branch} has stalled: no activity from its harness"),
                )
            }
            Activity::Status(status) => {
                let (kind, text) = match status {
                    BranchStatus::Running
                    | BranchStatus::Waiting
                    | BranchStatus::WaitingOnChildren => {
                        self.status.remove(branch);
                        return None;
                    }
                    // Someone set it aside on purpose: nothing to tell them.
                    BranchStatus::Merged { .. } | BranchStatus::Discarded { .. } => return None,
                    BranchStatus::Ready => (Kind::Finished, format!("{branch} is ready to merge")),
                    BranchStatus::NoChanges => {
                        (Kind::Finished, format!("{branch} finished with no changes"))
                    }
                    BranchStatus::BudgetExceeded { .. } => {
                        (Kind::Finished, format!("{branch} stopped at its budget"))
                    }
                    BranchStatus::Interrupted => {
                        (Kind::Interrupted, format!("{branch} was interrupted"))
                    }
                    BranchStatus::Failed { reason } => {
                        (Kind::Failed, short(&format!("{branch} failed: {reason}")))
                    }
                    BranchStatus::Blocked { reason } => (
                        Kind::Failed,
                        short(&format!("{branch} is blocked: {reason}")),
                    ),
                    BranchStatus::AwaitingPlanApproval => (
                        Kind::Question,
                        format!("{branch}'s plan awaits your approval (by plan show {branch})"),
                    ),
                };
                if self.status.insert(branch.to_owned(), kind) == Some(kind) {
                    return None;
                }
                notice(kind, text)
            }
            _ => None,
        }
    }
}

/// What a notice writes to the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    Osc9,
    Osc777,
    Bell,
    None,
}

impl Escape {
    /// The escape for `auto`, from the terminal's variables: OSC 777 where
    /// it is the terminal's only desktop-notification escape (foot, urxvt,
    /// VTE terminals), OSC 9 elsewhere.
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Escape {
        let term = env("TERM").unwrap_or_default();
        let program = env("TERM_PROGRAM").unwrap_or_default();
        let osc9_program = ["iTerm.app", "WezTerm", "ghostty"].contains(&program.as_str());
        let osc777 = term.starts_with("foot")
            || term.starts_with("rxvt")
            || env("VTE_VERSION").is_some_and(|v| !v.is_empty());
        match osc777 && !osc9_program {
            true => Escape::Osc777,
            false => Escape::Osc9,
        }
    }
}

/// Text safe inside an OSC string: no control characters, which would end
/// or corrupt it, and for OSC 777 no `;`, which separates its fields.
fn clean(text: &str, semicolons: bool) -> String {
    text.chars()
        .map(|c| match c {
            c if c.is_control() => ' ',
            ';' if !semicolons => ',',
            c => c,
        })
        .collect()
}

/// The title every notice carries.
pub const TITLE: &str = "Branchyard";

/// What to write to the terminal for `notice`: a bell, then the escape.
pub fn encode(notice: &Notice, escape: Escape, tmux: bool) -> String {
    let sequence = match escape {
        Escape::None => return String::new(),
        Escape::Bell => return "\x07".to_owned(),
        Escape::Osc9 => format!("\x1b]9;{TITLE}: {}\x07", clean(&notice.text, true)),
        Escape::Osc777 => format!("\x1b]777;notify;{TITLE};{}\x07", clean(&notice.text, false)),
    };
    let sequence = match tmux {
        true => crate::watch::tmux_passthrough(&sequence),
        false => sequence,
    };
    format!("\x07{sequence}")
}

/// The command that shows `notice` as a desktop notification here.
pub fn desktop_command(notice: &Notice, macos: bool) -> (&'static str, Vec<String>) {
    match macos {
        true => {
            let quote = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
            (
                "osascript",
                vec![
                    "-e".to_owned(),
                    format!(
                        "display notification \"{}\" with title \"{TITLE}\"",
                        quote(&clean(&notice.text, true))
                    ),
                ],
            )
        }
        false => (
            "notify-send",
            vec![
                "--app-name=Branchyard".to_owned(),
                "--".to_owned(),
                TITLE.to_owned(),
                clean(&notice.text, true),
            ],
        ),
    }
}

/// Whether and how to notify, from `--no-notify`, `[notify]` and the
/// terminal's variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub desktop: bool,
    pub escape: Escape,
    pub tmux: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: false,
            desktop: false,
            escape: Escape::None,
            tmux: false,
        }
    }
}

impl Settings {
    pub fn resolve(
        no_notify: bool,
        config: &Notify,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Settings {
        let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
        // Inside a harness's branch, `by` acts for the harness: nobody is
        // watching its terminal.
        let enabled = !no_notify && config.enabled != Some(false) && !set("BRANCHYARD_BRANCH");
        if !enabled {
            return Settings::default();
        }
        let escape = match config.terminal.unwrap_or(NotifyTerminal::Auto) {
            NotifyTerminal::Auto => Escape::detect(env),
            NotifyTerminal::Osc9 => Escape::Osc9,
            NotifyTerminal::Osc777 => Escape::Osc777,
            NotifyTerminal::Bell => Escape::Bell,
            NotifyTerminal::None => Escape::None,
        };
        Settings {
            enabled,
            desktop: config.desktop == Some(true),
            escape,
            tmux: set("TMUX"),
        }
    }

    /// The flag that keeps a `by` this one starts from notifying too.
    pub const CHILD_FLAG: &'static str = "--no-notify";
}

/// Shows notices: the escape on `out` (a terminal), and the desktop
/// notification when configured.
pub struct Notifier {
    settings: Settings,
    tracker: Mutex<Tracker>,
    /// `None` when the stream is not a terminal: desktop only.
    out: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Notifier {
    /// `None` when nothing would ever be shown.
    pub fn new(settings: Settings, out: Option<Box<dyn Write + Send>>) -> Option<Notifier> {
        let terminal = out.is_some() && settings.escape != Escape::None;
        if !settings.enabled || !(terminal || settings.desktop) {
            return None;
        }
        Some(Notifier {
            settings,
            tracker: Mutex::new(Tracker::default()),
            out: Mutex::new(out),
        })
    }

    /// Notice `activity` if it deserves it and has not been noticed yet.
    pub fn observe(&self, branch: &str, activity: &Activity) {
        let notice = self
            .tracker
            .lock_recovering("tracker")
            .observe(branch, activity);
        if let Some(notice) = notice {
            self.show(&notice);
        }
    }

    /// Show `notice` now.
    pub fn show(&self, notice: &Notice) {
        show(&self.settings, notice, &mut self.out.lock_recovering("out"));
    }
}

/// Show `notice` on `out` and, when configured, on the desktop. Best
/// effort: a notification never fails the command.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
pub fn show(settings: &Settings, notice: &Notice, out: &mut Option<Box<dyn Write + Send>>) {
    if !settings.enabled {
        return;
    }
    if let Some(out) = out {
        let text = encode(notice, settings.escape, settings.tmux);
        if !text.is_empty() {
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
    }
    if settings.desktop {
        let (program, args) = desktop_command(notice, cfg!(target_os = "macos"));
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            // Reap it without waiting here.
            std::thread::spawn(move || child.wait());
        }
    }
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{Message, PermissionKey, PermissionRequest};

    fn permission(key: &str) -> Activity {
        Activity::Harness(Event::PermissionRequested {
            turn: Some(1),
            request: PermissionRequest {
                key: PermissionKey(key.into()),
                tool: "Bash".into(),
                input: serde_json::Value::Null,
            },
        })
    }

    fn message(id: u64, kind: MessageKind) -> Activity {
        Activity::Message(Message {
            id,
            from: "impl".into(),
            to: "lead".into(),
            kind,
            text: "which\nparser?".into(),
            in_reply_to: None,
            at_ms: 0,
            delivered: false,
        })
    }

    fn texts(tracker: &mut Tracker, events: &[(&str, Activity)]) -> Vec<String> {
        events
            .iter()
            .filter_map(|(branch, activity)| tracker.observe(branch, activity))
            .map(|n| n.text)
            .collect()
    }

    #[test]
    fn what_deserves_a_notice_and_only_once() {
        let mut t = Tracker::default();
        let status = |s: BranchStatus| Activity::Status(s);
        assert_eq!(
            texts(
                &mut t,
                &[
                    ("impl", status(BranchStatus::Running)),
                    ("impl", Activity::Prompt("go".into())),
                    ("impl", permission("k1")),
                    ("impl", permission("k1")),
                    ("impl", permission("k2")),
                    // A question lands on both logs; one notice.
                    ("impl", message(7, MessageKind::Question)),
                    ("lead", message(7, MessageKind::Question)),
                    ("impl", message(8, MessageKind::Report)),
                    ("impl", message(9, MessageKind::Escalation)),
                    ("impl", Activity::Stalled { since_ms: 5 }),
                    ("impl", Activity::Stalled { since_ms: 5 }),
                    ("impl", Activity::Resumed),
                    ("impl", status(BranchStatus::Interrupted)),
                    ("impl", status(BranchStatus::Interrupted)),
                    // A new turn, then the same status again: a new notice.
                    ("impl", status(BranchStatus::Running)),
                    ("impl", status(BranchStatus::Interrupted)),
                    (
                        "docs",
                        status(BranchStatus::Failed {
                            reason: "exit 1\nboom".into()
                        })
                    ),
                    ("a", status(BranchStatus::Ready)),
                    (
                        "a",
                        status(BranchStatus::Merged {
                            target: "main".into(),
                            commit: "c".into()
                        })
                    ),
                    ("b", status(BranchStatus::NoChanges)),
                    (
                        "c",
                        status(BranchStatus::BudgetExceeded { limit: "$1".into() })
                    ),
                    (
                        "d",
                        status(BranchStatus::Blocked {
                            reason: "a failed".into()
                        })
                    ),
                ],
            ),
            [
                "impl asks to use Bash",
                "impl asks to use Bash",
                "impl asks lead: which parser?",
                "impl escalates to lead: which parser?",
                "impl has stalled: no activity from its harness",
                "impl was interrupted",
                "impl was interrupted",
                "docs failed: exit 1 boom",
                "a is ready to merge",
                "b finished with no changes",
                "c stopped at its budget",
                "d is blocked: a failed",
            ]
        );
        let kinds: Vec<Kind> = [
            permission("k9"),
            message(10, MessageKind::Question),
            Activity::Stalled { since_ms: 9 },
            Activity::Status(BranchStatus::Failed { reason: "x".into() }),
        ]
        .iter()
        .filter_map(|a| t.observe("z", a).map(|n| n.kind))
        .collect();
        assert_eq!(
            kinds,
            [
                Kind::Permission,
                Kind::Question,
                Kind::Stalled,
                Kind::Failed
            ]
        );
    }

    fn notice(text: &str) -> Notice {
        Notice {
            branch: "impl".into(),
            kind: Kind::Finished,
            text: text.into(),
        }
    }

    #[test]
    fn escapes_are_a_bell_and_osc_9_or_777() {
        let n = notice("impl is ready; merge it\x1b]0;evil\x07");
        assert_eq!(
            encode(&n, Escape::Osc9, false),
            "\x07\x1b]9;Branchyard: impl is ready; merge it ]0;evil \x07"
        );
        assert_eq!(
            encode(&n, Escape::Osc777, false),
            "\x07\x1b]777;notify;Branchyard;impl is ready, merge it ]0,evil \x07"
        );
        assert_eq!(encode(&n, Escape::Bell, false), "\x07");
        assert_eq!(encode(&n, Escape::None, false), "");
        assert_eq!(
            encode(&notice("done"), Escape::Osc9, true),
            "\x07\x1bPtmux;\x1b\x1b]9;Branchyard: done\x07\x1b\\"
        );
    }

    #[test]
    fn auto_picks_the_escape_the_terminal_takes() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            Escape::detect(&move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            })
        };
        assert_eq!(with(&[("TERM_PROGRAM", "iTerm.app")]), Escape::Osc9);
        assert_eq!(with(&[("TERM", "xterm-kitty")]), Escape::Osc9);
        assert_eq!(with(&[("TERM", "foot")]), Escape::Osc777);
        assert_eq!(with(&[("TERM", "rxvt-unicode-256color")]), Escape::Osc777);
        assert_eq!(with(&[("VTE_VERSION", "7600")]), Escape::Osc777);
        assert_eq!(
            with(&[("VTE_VERSION", "7600"), ("TERM_PROGRAM", "WezTerm")]),
            Escape::Osc9
        );
        assert_eq!(with(&[]), Escape::Osc9);
    }

    #[test]
    fn settings_follow_the_flag_the_file_and_the_harness() {
        let none = |_: &str| None;
        let on = Settings::resolve(false, &Notify::default(), &none);
        assert!(on.enabled && !on.desktop);
        assert_eq!(on.escape, Escape::Osc9);
        assert!(!Settings::resolve(true, &Notify::default(), &none).enabled);
        let off = Notify {
            enabled: Some(false),
            ..Notify::default()
        };
        assert!(!Settings::resolve(false, &off, &none).enabled);
        let desktop = Notify {
            desktop: Some(true),
            terminal: Some(NotifyTerminal::None),
            ..Notify::default()
        };
        let s = Settings::resolve(false, &desktop, &|n: &str| {
            (n == "TMUX").then(|| "/tmp/tmux".to_owned())
        });
        assert_eq!((s.desktop, s.escape, s.tmux), (true, Escape::None, true));
        let in_harness = |n: &str| (n == "BRANCHYARD_BRANCH").then(|| "impl".to_owned());
        assert!(!Settings::resolve(false, &Notify::default(), &in_harness).enabled);
        // Nothing to show on: no notifier at all.
        assert!(Notifier::new(on, None).is_none());
        assert!(
            Notifier::new(s, None).is_some(),
            "desktop needs no terminal"
        );
    }

    #[test]
    fn a_notifier_writes_each_notice_once_to_its_terminal() {
        #[derive(Clone, Default)]
        struct Shared(std::sync::Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let out = Shared::default();
        let settings = Settings {
            enabled: true,
            desktop: false,
            escape: Escape::Osc9,
            tmux: false,
        };
        let notifier = Notifier::new(settings, Some(Box::new(out.clone()))).unwrap();
        notifier.observe("impl", &permission("k"));
        notifier.observe("impl", &permission("k"));
        notifier.observe("impl", &Activity::Prompt("x".into()));
        let written = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert_eq!(written, "\x07\x1b]9;Branchyard: impl asks to use Bash\x07");
    }

    #[test]
    fn desktop_commands_quote_their_text() {
        let n = notice("say \"hi\" \\ now");
        assert_eq!(
            desktop_command(&n, true),
            (
                "osascript",
                vec![
                    "-e".to_owned(),
                    "display notification \"say \\\"hi\\\" \\\\ now\" with title \"Branchyard\""
                        .to_owned()
                ]
            )
        );
        let (program, args) = desktop_command(&notice("-x"), false);
        assert_eq!(program, "notify-send");
        assert_eq!(args, ["--app-name=Branchyard", "--", "Branchyard", "-x"]);
    }
}
