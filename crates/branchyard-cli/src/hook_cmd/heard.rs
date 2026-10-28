//! Proof that a harness ran one of Branchyard's hooks: `by hook` and `by
//! statusline` note each real call here, and `by setup`, bare `by`, `by
//! doctor` and `by init plugin --status` read it back as "heard" (when, at
//! which event) or "never heard". A canned call proves only that `by hook`
//! answers; a note proves the harness loaded the file and ran the command.
//! See docs/harness-hooks.md.
//!
//! One small JSON file per dialect, `hooks-heard/<dialect>.json` beside the
//! user configuration, replaced whole through a temporary file and a rename,
//! so a reader never sees half of one. A note holds the harness's own name
//! for the event, a digest of the session id, the repository the call was
//! made in, and the time: no prompt, no transcript path, nothing the harness
//! sent beyond those. Two calls at once may lose one of the two notes (the
//! last write wins); either is proof of a call.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// `by setup`'s canned call sets this, so its own call is not taken for the
/// harness's.
pub(crate) const CHECK_VAR: &str = "BRANCHYARD_HOOK_CHECK";

/// The kind a status-line call is noted under, beside the two events.
pub(crate) const STATUSLINE: &str = "statusline";

/// A status line is redrawn often: its note is rewritten at most this often
/// for the same repository.
const STATUSLINE_EVERY_MS: u64 = 30_000;

/// The longest harness event name kept.
const MAX_EVENT: usize = 40;

/// One real call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Call {
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The harness's own name for the event (`BeforeAgent`,
    /// `pre_llm_call`), when its JSON gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_event: Option<String>,
    /// The first twelve hex digits of the SHA-256 of the session id, when
    /// the JSON gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The git repository the call was made in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// What one dialect's file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The `--harness` dialect the calls were made in.
    #[serde(default)]
    pub dialect: String,
    /// The last call of each kind: `session-start`, `user-prompt-submit`,
    /// `statusline`.
    #[serde(default)]
    pub calls: BTreeMap<String, Call>,
    /// When `by setup` last wrote each harness's hook files, by harness id.
    #[serde(default)]
    pub installed: BTreeMap<String, u64>,
}

/// Where the notes are kept: `hooks-heard/` beside the user configuration
/// (`~/.config/branchyard/hooks-heard/`, or beside
/// `BRANCHYARD_USER_CONFIG`).
pub(crate) fn store() -> PathBuf {
    crate::setup_io::user_file().with_file_name("hooks-heard")
}

/// The hook dialect a harness's install speaks: its own, or `generic` for
/// the plugins written in code, which share one dialect and so one note.
pub(crate) fn dialect_of(harness: &str) -> &str {
    match harness {
        "pi" | "oh-my-pi" | "opencode" | "amp" => "generic",
        other => other,
    }
}

fn file(store: &Path, dialect: &str) -> PathBuf {
    store.join(format!("{dialect}.json"))
}

/// `dialect`'s record, or an empty one when there is none or it does not
/// read. An empty record says "never heard", which is safe: a note is
/// evidence for a call, and a lost one is only evidence missing (the next
/// call writes a whole new file).
pub(crate) fn read(store: &Path, dialect: &str) -> Record {
    let path = file(store, dialect);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Record::default(),
        Err(error) => {
            let what = format!("read the hook note {}", path.display());
            branchyard_support::best_effort::<(), _>(&what, Err(error));
            return Record::default();
        }
    };
    let what = format!("parse the hook note {}", path.display());
    branchyard_support::best_effort(&what, serde_json::from_str(&text)).unwrap_or_default()
}

/// Write `record` through a temporary file and a rename.
fn write(store: &Path, record: &Record) -> std::io::Result<()> {
    std::fs::create_dir_all(store)?;
    let path = file(store, &record.dialect);
    let text = serde_json::to_string(record).map_err(std::io::Error::other)?;
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        branchyard_support::cleanup_file(&temporary);
    })
}

/// The harness's own name for the event, from the fields harnesses use for
/// it: letters, digits, `_` and `-` only.
pub(crate) fn harness_event(input: &Value) -> Option<String> {
    ["/hook_event_name", "/hookEventName"]
        .iter()
        .filter_map(|path| input.pointer(path).and_then(Value::as_str))
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .take(MAX_EVENT)
                .collect::<String>()
        })
        .find(|name| !name.is_empty())
}

/// The first twelve hex digits of `id`'s SHA-256: enough to tell sessions
/// apart, nothing to find one by.
pub(crate) fn session_digest(id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = hex::encode(Sha256::digest(id.as_bytes()));
    digest.truncate(12);
    digest
}

/// The git repository `dir` is in: the nearest directory at or above it
/// that is one ([`branchyard_config::is_repository`]: an empty `.git` is not).
fn repository_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| branchyard_config::is_repository(d))
        .map(Path::to_path_buf)
}

/// Note a real call of `kind` (`session-start`, `user-prompt-submit` or
/// [`STATUSLINE`]) in `dialect`, made in `dir`. `Ok(false)` when nothing
/// was written: a status line noted less than [`STATUSLINE_EVERY_MS`] ago
/// for the same repository.
pub(crate) fn note(
    store: &Path,
    dialect: &str,
    kind: &str,
    session: Option<&str>,
    harness_event: Option<String>,
    dir: &Path,
    now: u64,
) -> std::io::Result<bool> {
    let call = Call {
        at_ms: now,
        harness_event,
        session: session.filter(|s| !s.is_empty()).map(session_digest),
        repository: repository_of(dir).map(|root| root.display().to_string()),
    };
    let mut record = read(store, dialect);
    if kind == STATUSLINE {
        if let Some(last) = record.calls.get(kind) {
            let recent = now.saturating_sub(last.at_ms) < STATUSLINE_EVERY_MS;
            if recent && last.repository == call.repository && last.at_ms <= now {
                return Ok(false);
            }
        }
    }
    record.dialect = dialect.to_owned();
    record.calls.insert(kind.to_owned(), call);
    write(store, &record)?;
    Ok(true)
}

/// Note that `by setup` wrote `harness`'s hook files at `now`: a call
/// before it says nothing about the files written.
pub(crate) fn stamp_install(store: &Path, harness: &str, now: u64) -> std::io::Result<()> {
    let dialect = dialect_of(harness);
    let mut record = read(store, dialect);
    record.dialect = dialect.to_owned();
    record.installed.insert(harness.to_owned(), now);
    write(store, &record)
}

/// Whether a harness's hooks were heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// A real call, since the hook files were last written by `by setup`.
    Heard,
    /// A real call, but before `by setup` last wrote the hook files.
    Stale,
    /// No real call noted.
    Never,
}

impl State {
    /// The word `--json` uses.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            State::Heard => "heard",
            State::Stale => "stale",
            State::Never => "never",
        }
    }
}

/// What is known of one harness's hooks being run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hearing {
    /// The harness.
    pub harness: String,
    /// The dialect its hooks speak.
    pub dialect: String,
    /// The latest real hook call, else the latest status-line call: its
    /// kind and the call.
    pub last: Option<(String, Call)>,
    /// When `by setup` last wrote the harness's hook files.
    pub installed_at: Option<u64>,
}

impl Hearing {
    /// What the notes in `store` say of `harness`.
    pub(crate) fn of(store: &Path, harness: &str) -> Hearing {
        let dialect = dialect_of(harness);
        let record = read(store, dialect);
        let latest = |hooks: bool| {
            record
                .calls
                .iter()
                .filter(|(kind, _)| (kind.as_str() != STATUSLINE) == hooks)
                .max_by_key(|(_, call)| call.at_ms)
                .map(|(kind, call)| (kind.clone(), call.clone()))
        };
        Hearing {
            harness: harness.to_owned(),
            dialect: dialect.to_owned(),
            last: latest(true).or_else(|| latest(false)),
            installed_at: record.installed.get(harness).copied(),
        }
    }

    /// Heard, heard before the last install, or never.
    pub(crate) fn state(&self) -> State {
        match (&self.last, self.installed_at) {
            (None, _) => State::Never,
            (Some((_, call)), Some(installed)) if call.at_ms < installed => State::Stale,
            (Some(_), _) => State::Heard,
        }
    }

    /// What was heard last, in brackets: `(session-start)`, and the
    /// dialect when several harnesses share it.
    pub(crate) fn what(&self, kind: &str) -> String {
        match self.dialect.as_str() {
            "generic" => format!("({kind}, generic dialect)"),
            _ => format!("({kind})"),
        }
    }

    /// `heard 3m ago (session-start)`, `never heard: start a new Codex
    /// session`, or `last heard 2d ago (session-start), before by setup
    /// rewrote the hooks: start a new Codex session`. `label` names the
    /// harness.
    pub(crate) fn words(&self, label: &str, now: u64) -> String {
        let start = format!("start a new {label} session");
        match (self.state(), &self.last) {
            (State::Heard, Some((kind, call))) => {
                format!("heard {} {}", ago(now, call.at_ms), self.what(kind))
            }
            (State::Stale, Some((kind, call))) => format!(
                "last heard {} {}, before by setup rewrote the hooks: {start}",
                ago(now, call.at_ms),
                self.what(kind)
            ),
            _ => format!("never heard: {start}"),
        }
    }

    /// The `heard` object `--json` prints.
    pub(crate) fn json(&self, now: u64) -> Value {
        let (kind, call) = match &self.last {
            Some((kind, call)) => (Some(kind.as_str()), Some(call)),
            None => (None, None),
        };
        json!({
            "state": self.state().as_str(),
            "dialect": self.dialect,
            "kind": kind,
            "harness_event": call.and_then(|c| c.harness_event.clone()),
            "at_ms": call.map(|c| c.at_ms),
            "age_seconds": call.map(|c| now.saturating_sub(c.at_ms) / 1000),
            "session": call.and_then(|c| c.session.clone()),
            "repository": call.and_then(|c| c.repository.clone()),
            "installed_at_ms": self.installed_at,
        })
    }
}

/// `3m ago`, or `just now`.
pub(crate) fn ago(now: u64, at_ms: u64) -> String {
    match crate::render::age_text(now.saturating_sub(at_ms) / 1000).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn a_call_is_noted_with_its_event_a_digest_and_its_repository() {
        let scratch = branchyard_testkit::Scratch::new("heard-note");
        let store = scratch.path().join("user/hooks-heard");
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // A git directory has a HEAD; an empty `.git` is not a repository.
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let wrote = note(
            &store,
            "codex",
            "session-start",
            Some("01a16dba-7f5b"),
            Some("SessionStart".into()),
            &repo.join("src"),
            NOW,
        );
        assert_eq!(wrote.ok(), Some(true));
        let record = read(&store, "codex");
        assert_eq!(record.dialect, "codex");
        let call = &record.calls["session-start"];
        assert_eq!(call.at_ms, NOW);
        assert_eq!(call.harness_event.as_deref(), Some("SessionStart"));
        assert_eq!(
            call.session.as_deref(),
            Some(&session_digest("01a16dba-7f5b")[..])
        );
        assert_eq!(call.session.as_ref().map(String::len), Some(12));
        assert_eq!(call.repository, Some(repo.display().to_string()));
        // The file holds the digest, never the id.
        let text = std::fs::read_to_string(store.join("codex.json")).unwrap();
        assert!(!text.contains("01a16dba-7f5b"), "{text}");
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(&store).unwrap().collect();
        assert_eq!(names.len(), 1);

        // Outside a repository: noted, with no repository.
        let outside = scratch.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let wrote = note(
            &store,
            "codex",
            "user-prompt-submit",
            None,
            None,
            &outside,
            NOW + 5,
        );
        assert_eq!(wrote.ok(), Some(true));
        let record = read(&store, "codex");
        assert_eq!(record.calls.len(), 2);
        let call = &record.calls["user-prompt-submit"];
        assert_eq!(
            (call.repository.as_ref(), call.session.as_ref()),
            (None, None)
        );
    }

    #[test]
    fn a_status_line_is_noted_at_most_every_thirty_seconds_per_repository() {
        let scratch = branchyard_testkit::Scratch::new("heard-status");
        let store = scratch.path().join("hooks-heard");
        let here = scratch.path();
        let at = |now| note(&store, "claude-code", STATUSLINE, None, None, here, now).ok();
        assert_eq!(at(NOW), Some(true));
        assert_eq!(at(NOW + 1_000), Some(false));
        assert_eq!(at(NOW + STATUSLINE_EVERY_MS), Some(true));
        // A hook is noted every time.
        for later in [1, 2] {
            let wrote = note(
                &store,
                "claude-code",
                "session-start",
                None,
                None,
                here,
                NOW + later,
            );
            assert_eq!(wrote.ok(), Some(true));
        }
    }

    #[test]
    fn heard_never_and_stale_read_back_in_words() {
        let scratch = branchyard_testkit::Scratch::new("heard-read");
        let store = scratch.path().join("hooks-heard");
        let here = scratch.path();
        let never = Hearing::of(&store, "codex");
        assert_eq!(never.state(), State::Never);
        assert_eq!(
            never.words("Codex", NOW),
            "never heard: start a new Codex session"
        );
        assert_eq!(never.json(NOW)["state"], "never");

        // A status line alone is heard, and named as such.
        note(&store, "codex", STATUSLINE, None, None, here, NOW - 5_000).unwrap();
        let status = Hearing::of(&store, "codex");
        assert_eq!(status.words("Codex", NOW), "heard just now (statusline)");
        // A hook call is preferred to a later status line.
        note(
            &store,
            "codex",
            "session-start",
            Some("s"),
            None,
            here,
            NOW - 180_000,
        )
        .unwrap();
        let heard = Hearing::of(&store, "codex");
        assert_eq!(heard.state(), State::Heard);
        assert_eq!(heard.words("Codex", NOW), "heard 3m ago (session-start)");
        let shown = heard.json(NOW);
        assert_eq!(shown["state"], "heard");
        assert_eq!(shown["kind"], "session-start");
        assert_eq!(shown["age_seconds"], 180);
        assert_eq!(shown["session"], session_digest("s"));

        // by setup rewrote the files after that call: stale until heard again.
        stamp_install(&store, "codex", NOW - 60_000).unwrap();
        let stale = Hearing::of(&store, "codex");
        assert_eq!(stale.state(), State::Stale);
        assert_eq!(
            stale.words("Codex", NOW),
            "last heard 3m ago (session-start), before by setup rewrote the hooks: \
             start a new Codex session"
        );
        assert_eq!(stale.json(NOW)["installed_at_ms"], NOW - 60_000);
        note(
            &store,
            "codex",
            "user-prompt-submit",
            None,
            None,
            here,
            NOW - 1_000,
        )
        .unwrap();
        assert_eq!(Hearing::of(&store, "codex").state(), State::Heard);
    }

    #[test]
    fn the_harnesses_written_in_code_share_the_generic_note() {
        let scratch = branchyard_testkit::Scratch::new("heard-generic");
        let store = scratch.path().join("hooks-heard");
        note(
            &store,
            "generic",
            "session-start",
            None,
            None,
            scratch.path(),
            NOW,
        )
        .unwrap();
        for id in ["pi", "oh-my-pi", "opencode", "amp"] {
            let heard = Hearing::of(&store, id);
            assert_eq!(heard.dialect, "generic", "{id}");
            assert_eq!(
                heard.words("X", NOW),
                "heard just now (session-start, generic dialect)"
            );
        }
        // Each one's install is its own.
        stamp_install(&store, "pi", NOW + 1).unwrap();
        assert_eq!(Hearing::of(&store, "pi").state(), State::Stale);
        assert_eq!(Hearing::of(&store, "opencode").state(), State::Heard);
    }

    #[test]
    fn a_harness_event_is_a_short_safe_word() {
        let event = |input: Value| harness_event(&input);
        assert_eq!(
            event(json!({"hook_event_name": "BeforeAgent"})).as_deref(),
            Some("BeforeAgent")
        );
        assert_eq!(
            event(json!({"hookEventName": "x/../y z"})).as_deref(),
            Some("xyz")
        );
        assert_eq!(
            event(json!({"hook_event_name": "a".repeat(200)})).map(|e| e.len()),
            Some(MAX_EVENT)
        );
        assert_eq!(event(json!({"hook_event_name": "///"})), None);
        assert_eq!(event(json!({"cwd": "/r"})), None);
    }

    #[test]
    fn an_unreadable_record_is_an_empty_one_and_a_read_only_store_is_an_error() {
        let scratch = branchyard_testkit::Scratch::new("heard-bad");
        let store = scratch.path().join("hooks-heard");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("codex.json"), "not json").unwrap();
        assert_eq!(read(&store, "codex"), Record::default());
        // A store that is a file cannot be written: an error, not a panic.
        let blocked = scratch.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        assert!(note(
            &blocked,
            "codex",
            "session-start",
            None,
            None,
            scratch.path(),
            NOW
        )
        .is_err());
    }
}
