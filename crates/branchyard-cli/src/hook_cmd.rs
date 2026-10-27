//! `by hook` and `by statusline`: what a Claude Code session that a person
//! started (not one Branchyard runs, whose status comes from its own
//! stream) learns about the repository's branches, through the plugin's
//! hooks and the `statusLine` setting. See `docs/harness-integration.md`.
//! `--harness ID` speaks another harness's dialect instead ([`DIALECTS`]):
//! where its JSON keeps the session id and the directory, and the shape
//! its reply takes. See `docs/harness-hooks.md`.
//!
//! Both run on every prompt or redraw of someone else's session, so they
//! only read, where `by` itself would ([`source`]): the server `[remote]`
//! or `BRANCHYARD_REMOTE` names, asked with short timeouts; else
//! `.branchyard/state.db`; else the PostgreSQL store `[serve] config`
//! names (a build with the `postgres` feature). Each gives a
//! [`branchyard::glance::Glance`]. They give up after [`DEADLINE`] and
//! never fail: any problem prints nothing on stdout, at most a note on
//! stderr, and exits 0.
//!
//! Each call a harness makes (a JSON object on stdin, not `by setup`'s
//! canned one) is noted in [`heard`], so `by setup`, bare `by` and `by
//! doctor` can say a harness's hooks were heard, not only that `by hook`
//! answers.

pub(crate) mod heard;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use branchyard::glance::{self, Glance};
use branchyard::{BranchInfo, BranchStatus, Message, MessageKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::args::Globals;

/// How long either command may take before it gives up and prints nothing.
const DEADLINE: Duration = Duration::from_secs(2);
/// At most this many lines of context per hook call.
const MAX_LINES: usize = 12;
/// At most this many characters of one line.
const MAX_LINE: usize = 240;

/// `by hook [--harness ID] <event>`: always exits 0.
pub fn hook(harness: Option<&str>, event: &str) -> ExitCode {
    let harness = harness.map(str::to_owned);
    let event = event.to_owned();
    finish(
        "hook",
        within(DEADLINE, move || {
            // Read the input even for a harness not known here, so the
            // harness never writes into a closed pipe.
            let input = stdin_text();
            let dialect = dialect(harness.as_deref())?;
            let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            if Event::parse(&event).is_ok() {
                note_call(dialect, &event, &input, &env, &cwd());
            }
            hook_output_as(dialect, &event, &input, &env, &cwd())
        }),
    )
}

/// `by statusline [--harness ID]`: always exits 0.
pub fn statusline(harness: Option<&str>) -> ExitCode {
    let harness = harness.map(str::to_owned);
    finish(
        "statusline",
        within(DEADLINE, move || {
            let input = stdin_text();
            let dialect = dialect(harness.as_deref())?;
            let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            note_call(dialect, heard::STATUSLINE, &input, &env, &cwd());
            status_line_as(dialect, &input, &env, &cwd())
        }),
    )
}

/// The canonical hook events; each harness's packaging maps its own
/// event names onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    SessionStart,
    UserPromptSubmit,
}

impl Event {
    fn parse(event: &str) -> Result<Self, String> {
        match event {
            "session-start" => Ok(Event::SessionStart),
            "user-prompt-submit" => Ok(Event::UserPromptSubmit),
            other => Err(format!(
                "unknown event {other:?}; expected session-start or user-prompt-submit"
            )),
        }
    }
}

/// How a harness takes the context back on stdout.
#[derive(Debug)]
enum Reply {
    /// Claude Code's `{"hookSpecificOutput": {"hookEventName": ..,
    /// "additionalContext": ..}}`, with the harness's own event names.
    HookSpecific {
        session_start: &'static str,
        user_prompt_submit: &'static str,
    },
    /// `{"<key>": "<text>"}`.
    Field(&'static str),
    /// Antigravity's `{"injectSteps": [{"ephemeralMessage": ..}]}`.
    InjectSteps,
    /// The text itself.
    Plain,
}

/// One harness's hook and status-line dialect. Each path is a JSON
/// pointer into the harness's stdin; the first that holds a non-empty
/// string wins.
#[derive(Debug)]
pub(crate) struct Dialect {
    id: &'static str,
    /// Where the session id is.
    session: &'static [&'static str],
    /// Where the hook's working directory is.
    cwd: &'static [&'static str],
    /// Where the status line's working directory is.
    status_cwd: &'static [&'static str],
    reply: Reply,
    /// Events whose output this harness drops: `by` says nothing there and
    /// leaves the session's cursor alone, so the news waits for an event
    /// that is heard.
    deaf: &'static [Event],
}

/// Claude Code's own hook event names.
const CLAUDE_REPLY: Reply = Reply::HookSpecific {
    session_start: "SessionStart",
    user_prompt_submit: "UserPromptSubmit",
};
/// Claude Code's status-line JSON: `workspace.current_dir`, else `cwd`.
const CLAUDE_STATUS: &[&str] = &["/workspace/current_dir", "/cwd"];

/// Every dialect `--harness` names; the first is the default.
pub(crate) const DIALECTS: &[Dialect] = &[
    Dialect {
        id: "claude-code",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: CLAUDE_STATUS,
        reply: CLAUDE_REPLY,
        deaf: &[],
    },
    Dialect {
        id: "codex",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: CLAUDE_REPLY,
        deaf: &[],
    },
    Dialect {
        id: "gemini-cli",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: Reply::HookSpecific {
            session_start: "SessionStart",
            user_prompt_submit: "BeforeAgent",
        },
        // Headless, Gemini CLI puts a SessionStart context in front of the
        // prompt, so `gemini -p "/cmd"` is no longer a command; BeforeAgent
        // appends after the command is expanded, and hears everything.
        deaf: &[Event::SessionStart],
    },
    Dialect {
        id: "qwen-code",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: CLAUDE_STATUS,
        reply: CLAUDE_REPLY,
        deaf: &[],
    },
    Dialect {
        id: "github-copilot",
        session: &["/sessionId", "/session_id"],
        cwd: &["/cwd"],
        status_cwd: CLAUDE_STATUS,
        reply: Reply::Field("additionalContext"),
        // A config-file userPromptSubmitted hook's output is dropped.
        deaf: &[Event::UserPromptSubmit],
    },
    Dialect {
        id: "droid",
        session: &["/session_id"],
        cwd: &["/cwd"],
        // Its status-line JSON is undocumented.
        status_cwd: &["/cwd", "/workspace/current_dir"],
        reply: CLAUDE_REPLY,
        deaf: &[],
    },
    Dialect {
        id: "deepseek-harness",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: CLAUDE_REPLY,
        deaf: &[],
    },
    Dialect {
        id: "cursor",
        session: &["/session_id", "/conversation_id"],
        // A hook runs from the plugin's directory and is given no `cwd`.
        cwd: &["/workspace_roots/0", "/cwd"],
        status_cwd: &["/cwd", "/workspace/current_dir", "/workspace/project_dir"],
        reply: Reply::Field("additional_context"),
        // beforeSubmitPrompt takes only `continue` and `user_message`.
        deaf: &[Event::UserPromptSubmit],
    },
    Dialect {
        id: "antigravity",
        session: &["/conversationId"],
        cwd: &["/workspacePaths/0", "/cwd"],
        status_cwd: &["/cwd", "/workspacePaths/0", "/workspace/current_dir"],
        reply: Reply::InjectSteps,
        deaf: &[],
    },
    Dialect {
        id: "kiro",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: Reply::Plain,
        deaf: &[],
    },
    Dialect {
        id: "hermes",
        session: &["/session_id"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: Reply::Field("context"),
        // Only pre_llm_call injects context; on_session_start observes.
        deaf: &[Event::SessionStart],
    },
    Dialect {
        id: "generic",
        session: &["/session_id", "/sessionId"],
        cwd: &["/cwd"],
        status_cwd: &["/cwd"],
        reply: Reply::Field("context"),
        deaf: &[],
    },
];

/// The dialect `--harness` names; Claude Code's without one.
pub(crate) fn dialect(id: Option<&str>) -> Result<&'static Dialect, String> {
    let id = id.unwrap_or("claude-code");
    DIALECTS.iter().find(|d| d.id == id).ok_or_else(|| {
        let known: Vec<&str> = DIALECTS.iter().map(|d| d.id).collect();
        format!(
            "unknown harness {id:?}; printed nothing (known: {})",
            known.join(", ")
        )
    })
}

/// The first of `paths` in `input` that holds a non-empty string.
fn first_str<'a>(input: &'a Value, paths: &[&str]) -> Option<&'a str> {
    paths
        .iter()
        .filter_map(|path| input.pointer(path).and_then(Value::as_str))
        .find(|text| !text.is_empty())
}

impl Dialect {
    /// Whether this harness drops what `event` (`session-start` or
    /// `user-prompt-submit`) prints, so `by` says nothing there.
    pub(crate) fn drops(&self, event: &str) -> bool {
        Event::parse(event).is_ok_and(|event| self.deaf.contains(&event))
    }

    /// The hook's stdout for `lines` of context at `event`.
    fn render(&self, event: Event, lines: &[String]) -> String {
        let text = lines.join("\n");
        let output = match &self.reply {
            Reply::HookSpecific {
                session_start,
                user_prompt_submit,
            } => serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": match event {
                        Event::SessionStart => session_start,
                        Event::UserPromptSubmit => user_prompt_submit,
                    },
                    "additionalContext": text,
                }
            }),
            Reply::Field(key) => serde_json::json!({ *key: text }),
            // An ephemeral message at either event: the documented shapes of
            // a system message disagree with each other.
            Reply::InjectSteps => serde_json::json!({
                "injectSteps": [{"ephemeralMessage": text}]
            }),
            Reply::Plain => return format!("{text}\n"),
        };
        format!("{output}\n")
    }
}

/// Print `outcome`'s text, if any; a failure is a note on stderr. Exit 0
/// either way, so a session never sees a failed hook.
fn finish(what: &str, outcome: Result<Option<String>, String>) -> ExitCode {
    match outcome {
        Ok(Some(text)) => {
            let mut stdout = std::io::stdout().lock();
            branchyard_support::best_effort(
                "print the output",
                stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush()),
            );
        }
        Ok(None) => {}
        Err(error) => eprintln!("by {what}: {error}"),
    }
    ExitCode::SUCCESS
}

/// Run `f` on a thread of its own, giving up after `deadline`: the process
/// exits then, and the thread with it.
fn within<F>(deadline: Duration, f: F) -> Result<Option<String>, String>
where
    F: FnOnce() -> Result<Option<String>, String> + Send + 'static,
{
    let (send, receive) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("by-hook".into())
        .spawn(move || {
            // The receiver is gone only after the deadline passed.
            branchyard_support::best_effort("send the result", send.send(f()));
        });
    if let Err(error) = spawned {
        return Err(format!("could not start: {error}"));
    }
    match receive.recv_timeout(deadline) {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "gave up after {}s; printed nothing",
            deadline.as_secs()
        )),
    }
}

/// Standard input, unless it is a terminal (a person running the command
/// by hand), which would block until the deadline.
fn stdin_text() -> String {
    let mut text = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // Unreadable input is no input: the defaults still apply.
        branchyard_support::best_effort(
            "read standard input",
            stdin.lock().read_to_string(&mut text),
        );
    }
    text
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// The JSON Claude Code passes, or `null` when it is not JSON.
fn parse(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or(Value::Null)
}

/// Note a call of `kind` (an event, or [`heard::STATUSLINE`]) as the
/// harness's: its session, its event name and the repository its directory
/// is in, beside the user configuration. Not for `by setup`'s canned call
/// ([`heard::CHECK_VAR`]), nor for input that is not a JSON object (a
/// person at a terminal sends none). A note that cannot be written is
/// dropped: the call goes on as if it were.
fn note_call(
    dialect: &Dialect,
    kind: &str,
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) {
    if env(heard::CHECK_VAR).is_some() {
        return;
    }
    let input = parse(input);
    if !input.is_object() {
        return;
    }
    let paths = match kind {
        heard::STATUSLINE => dialect.status_cwd,
        _ => dialect.cwd,
    };
    let dir = first_str(&input, paths).map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    branchyard_support::best_effort(
        "note the hook call as heard",
        heard::note(
            &heard::store(),
            dialect.id,
            kind,
            first_str(&input, dialect.session),
            heard::harness_event(&input),
            &dir,
            branchyard_support::time::now_ms(),
        ),
    );
}

/// The local yard for `dir`: the one `BRANCHYARD_ROOT` names when it has
/// one, else the nearest above `dir`.
fn root_for(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("BRANCHYARD_ROOT")
        .and_then(|root| glance::find_root(Path::new(&root)))
        .or_else(|| glance::find_root(dir))
}

/// How long one request to a server may take, connecting included: two of
/// them in a row (the repository's name, then its branches) and the
/// inboxes, read at once, stay inside [`DEADLINE`].
const REQUEST_WAIT: Duration = Duration::from_millis(600);
/// At most this many inboxes are read from a server for one glance.
const MAX_INBOXES: usize = 16;

/// Where a glance at the repository at `dir` reads from.
#[derive(Debug, PartialEq)]
enum Source {
    /// The server `by` would run commands on. `root` is the repository
    /// root here, where the session's cursor is kept.
    Remote { root: PathBuf, globals: Globals },
    /// `root/.branchyard/state.db`.
    Local(PathBuf),
    /// A server's PostgreSQL store at `url`, holding this repository under
    /// `scope`.
    Postgres {
        root: PathBuf,
        url: String,
        scope: String,
    },
}

impl Source {
    /// The repository root the session's cursor is kept under.
    fn root(&self) -> &Path {
        match self {
            Source::Remote { root, .. } | Source::Local(root) | Source::Postgres { root, .. } => {
                root
            }
        }
    }
}

/// Where `by` would read `dir`'s repository from: the server
/// `BRANCHYARD_REMOTE` and its kin (no flags here) or `[remote]` name; else the nearest `.branchyard/state.db`;
/// else, when `[serve] config` names a server configuration with a
/// `database`, that store. `None` when it is none of these. A
/// configuration file that does not load is ignored here, as if absent.
fn source(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<Source>, String> {
    let config = match crate::defaults::config_at(dir, env) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("by: ignoring the configuration: {error}");
            None
        }
    };
    let value = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let mut globals = Globals {
        remote: value("BRANCHYARD_REMOTE"),
        token_file: value("BRANCHYARD_TOKEN_FILE"),
        repo: value("BRANCHYARD_REPO"),
        ca_file: value("BRANCHYARD_CA_FILE"),
        ..Globals::default()
    };
    if let Some(config) = &config {
        crate::defaults::apply_globals(config, &mut globals);
    }
    if globals.remote.is_some() {
        let root = branchyard_config::project_root(dir);
        return Ok(Some(Source::Remote { root, globals }));
    }
    if let Some(root) = root_for(dir, env) {
        return Ok(Some(Source::Local(root)));
    }
    let Some(path) = config.as_ref().and_then(|c| c.serve.config.clone()) else {
        return Ok(None);
    };
    let served = branchyard_server::cli::resolve(&["--config".to_owned(), path])?;
    let Some(url) = served.database else {
        return Ok(None);
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let repo = served
        .repos
        .iter()
        .map(|(name, root)| {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            (name, root)
        })
        .filter(|(_, root)| dir.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len());
    Ok(repo.map(|(name, root)| Source::Postgres {
        root,
        url,
        scope: name.clone(),
    }))
}

/// Read `source`, with `inbox_of`'s unread messages; `None` when there is
/// no yard there.
fn look(source: &Source, inbox_of: Option<&str>) -> Result<Option<Glance>, String> {
    match source {
        Source::Local(root) => glance::read(root, inbox_of).map_err(|e| e.to_string()),
        Source::Remote { root, globals } => ask_server(root, globals, inbox_of).map(Some),
        #[cfg(feature = "postgres")]
        Source::Postgres { root, url, scope } => glance::read_postgres(root, url, scope, inbox_of)
            .map(Some)
            .map_err(|e| e.to_string()),
        #[cfg(not(feature = "postgres"))]
        Source::Postgres { .. } => Err(format!(
            "the repository's state is in PostgreSQL, and this by is built without the \
             postgres feature; rebuild with `{}`",
            branchyard_setup::probe::REBUILD_POSTGRES
        )),
    }
}

/// The glance a server gives: its branches, the inboxes that hold the
/// running branches' questions (their parents') and answers (their own),
/// and `inbox_of`'s. Each request is given [`REQUEST_WAIT`]; any failure
/// is the whole glance's.
fn ask_server(root: &Path, globals: &Globals, inbox_of: Option<&str>) -> Result<Glance, String> {
    let url = globals.remote.as_deref().unwrap_or_default();
    if crate::ssh_remote::is_ssh(url) {
        return Err(
            "an ssh:// remote is not read from a hook (docs/harness-integration.md)".into(),
        );
    }
    let token_file = globals
        .token_file
        .as_deref()
        .ok_or("remote mode needs a token: set BRANCHYARD_TOKEN_FILE or [remote] token_file")?;
    let mut client = branchyard_client::Client::from_token_file(url, token_file)
        .map_err(|e| e.to_string())?
        .with_timeout(REQUEST_WAIT);
    if let Some(ca) = &globals.ca_file {
        client = client.with_ca_file(ca).map_err(|e| e.to_string())?;
    }
    let name = match &globals.repo {
        Some(name) => name.clone(),
        None => match client.repos().map_err(|e| e.to_string())?.as_slice() {
            [only] => only.name.clone(),
            _ => return Err("the server serves several repositories; set BRANCHYARD_REPO".into()),
        },
    };
    let repo = client.repo(&name);
    let branches = repo.branches().map_err(|e| e.to_string())?;
    let mut inboxes: BTreeSet<&str> = inbox_of.into_iter().collect();
    for info in &branches {
        if let (BranchStatus::Running, Some(parent)) = (&info.status, &info.parent) {
            inboxes.insert(parent);
            inboxes.insert(&info.name);
        }
    }
    if inboxes.len() > MAX_INBOXES {
        return Err(format!(
            "{} inboxes to read from the server; at most {MAX_INBOXES} are",
            inboxes.len()
        ));
    }
    let read: Vec<(&str, Result<branchyard::Inbox, String>)> = std::thread::scope(|scope| {
        let reads: Vec<_> = inboxes
            .iter()
            .map(|&branch| {
                let repo = &repo;
                (
                    branch,
                    scope.spawn(move || repo.inbox(branch).map_err(|e| e.to_string())),
                )
            })
            .collect();
        reads
            .into_iter()
            .map(|(branch, read)| {
                let inbox = read
                    .join()
                    .unwrap_or_else(|_| Err("reading an inbox panicked".into()));
                (branch, inbox)
            })
            .collect()
    });
    let mut messages = Vec::new();
    let mut unread = Vec::new();
    for (branch, inbox) in read {
        let inbox = inbox.map_err(|e| format!("{branch}'s inbox: {e}"))?;
        if Some(branch) == inbox_of {
            unread = inbox.clone().unread_only().messages;
        }
        messages.extend(inbox.messages);
    }
    Ok(Glance::from_parts(
        root.to_path_buf(),
        branches,
        glance::unanswered(&messages),
        unread,
    ))
}

/// A hook's stdout for `event`, given Claude Code's JSON `input`: `None`
/// when there is nothing new, or no yard.
#[cfg(test)]
pub(crate) fn hook_output(
    event: &str,
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    hook_output_as(&DIALECTS[0], event, input, env, cwd)
}

/// A hook's stdout for `event` in `dialect`, given the harness's JSON
/// `input`: `None` when there is nothing new, no yard, or the harness
/// drops what this event prints.
pub(crate) fn hook_output_as(
    dialect: &Dialect,
    event: &str,
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let event = Event::parse(event)?;
    // Without the hook's JSON there is no session to remember for.
    let input = parse(input);
    if !input.is_object() {
        return Err("standard input is not a hook's JSON object; printed nothing".into());
    }
    if dialect.deaf.contains(&event) {
        return Ok(None);
    }
    let dir = first_str(&input, dialect.cwd).map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    let Some(source) = source(&dir, env)? else {
        return Ok(None);
    };
    let branch = env("BRANCHYARD_BRANCH");
    let Some(glance) = look(&source, branch.as_deref())? else {
        return Ok(None);
    };
    let session = session_file_name(first_str(&input, dialect.session).unwrap_or_default());
    // For a server's repository too: only `.branchyard/hooks/` is made here.
    let path = glance::state_dir(source.root())
        .join("hooks")
        .join(format!("{session}.json"));
    let cursor = read_cursor(&path);
    let (lines, next) = match &branch {
        Some(branch) => inbox_lines(&glance, branch, cursor.as_ref()),
        None => change_lines(&glance, cursor.as_ref()),
    };
    if cursor.as_ref() != Some(&next) {
        if let Err(error) = write_cursor(&path, &next) {
            eprintln!("by hook: could not remember what was shown: {error}");
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(dialect.render(event, &capped(lines))))
}

/// What a session has been told: the last message id shown, and each
/// branch's last reported state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cursor {
    #[serde(default)]
    message: u64,
    #[serde(default)]
    branches: BTreeMap<String, String>,
}

fn read_cursor(path: &Path) -> Option<Cursor> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the cursor through a temporary file and a rename, so a reader
/// never sees half of one.
fn write_cursor(path: &Path, cursor: &Cursor) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string(cursor).map_err(std::io::Error::other)?;
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        branchyard_support::cleanup_file(&temporary);
    })
}

/// A session id as a file name: letters, digits, `-` and `_` only.
fn session_file_name(id: &str) -> String {
    let name: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(100)
        .collect();
    match name.is_empty() {
        true => "default".to_owned(),
        false => name,
    }
}

/// What every call that says anything opens with: what the lines are, and
/// what to do with them.
const HEADER: &str = "Branchyard, this repository. Tell the person in one line about any \
                      question or failure below; act on nothing unless they ask:";
/// At most this many ready, and this many failed, branches are named on a
/// session's first call.
const NAMED_AT_START: usize = 3;

/// `branch`'s unread messages not shown to this session yet.
fn inbox_lines(glance: &Glance, branch: &str, cursor: Option<&Cursor>) -> (Vec<String>, Cursor) {
    let seen = cursor.map_or(0, |c| c.message);
    let new: Vec<&Message> = glance.unread.iter().filter(|m| m.id > seen).collect();
    let next = Cursor {
        message: glance
            .unread
            .iter()
            .map(|m| m.id)
            .max()
            .unwrap_or(0)
            .max(seen),
        branches: BTreeMap::new(),
    };
    if new.is_empty() {
        return (Vec::new(), next);
    }
    let mut lines = vec![format!(
        "Branchyard, {} unread message{} for {branch}. Answer your children's questions with \
         by answer --as {branch} ID \"TEXT\"; ask your parent with by ask \"TEXT\".",
        new.len(),
        plural(new.len())
    )];
    lines.extend(new.iter().map(|m| {
        let reply = m
            .in_reply_to
            .map(|id| format!(" re #{id}"))
            .unwrap_or_default();
        line(&format!(
            "- #{} {} from {}{reply}: {}",
            m.id, m.kind, m.from, m.text
        ))
    }));
    (lines, next)
}

/// Branches whose state a person acts on and that changed since this
/// session's last hook call, each with the command that deals with it. On a
/// session's first call, only what needs an answer now (asking, blocked, a
/// plan to approve); then up to [`NAMED_AT_START`] ready and as many failed
/// branches by name (older endings are not news one by one, but each still
/// waits on a decision), and a count of what is running.
fn change_lines(glance: &Glance, cursor: Option<&Cursor>) -> (Vec<String>, Cursor) {
    let mut next = Cursor::default();
    let mut changed = Vec::new();
    let mut ready = Vec::new();
    let mut failed = Vec::new();
    for info in &glance.branches {
        let asking = glance.asking.iter().rev().find(|m| m.from == info.name);
        let Some(label) = notable(info, asking) else {
            continue;
        };
        let key = format!("{}|{}", info.turns, label.state);
        let new = match cursor {
            Some(cursor) => cursor.branches.get(&info.name) != Some(&key),
            None => label.urgent,
        };
        match (new, label.group) {
            (true, _) => changed.push(label.line),
            (false, Group::Ready) if cursor.is_none() => ready.push(label.line),
            (false, Group::Failed) if cursor.is_none() => failed.push(label.line),
            (false, _) => {}
        }
        next.branches.insert(info.name.clone(), key);
    }
    if cursor.is_none() {
        changed.extend(newest(ready, "ready"));
        changed.extend(newest(failed, "failed"));
        let running = glance.counts().running;
        if running > 0 {
            changed.push(format!("- {running} running: by ls"));
        }
    }
    let mut lines = Vec::new();
    if !changed.is_empty() {
        lines.push(HEADER.to_owned());
        lines.extend(changed);
    }
    (lines, next)
}

/// The last [`NAMED_AT_START`] of `lines`, oldest first, then a line
/// saying how many older ones there are.
fn newest(mut lines: Vec<String>, what: &str) -> Vec<String> {
    let older = lines.len().saturating_sub(NAMED_AT_START);
    lines.drain(..older);
    if older > 0 {
        lines.push(format!("- ... and {older} more {what}: by ls"));
    }
    lines
}

/// What a branch's state is to a session that has to act on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    /// A candidate waits to be integrated.
    Ready,
    /// Failed, interrupted or stopped at a limit.
    Failed,
    /// Everything else a session is told about.
    Other,
}

/// A branch state worth telling a session about.
struct Label {
    /// The state, as the session's cursor remembers it.
    state: String,
    /// Needs an answer or a decision, not only news.
    urgent: bool,
    group: Group,
    /// The line a session is given: the branch, its state and the command
    /// that deals with it.
    line: String,
}

/// `head`, `variable` and `tail` as one line of at most [`MAX_LINE`]
/// characters, where only `variable` (a reason or a question, which may be
/// long) gives way, so the command in `tail` is never cut off.
fn fitted(head: &str, variable: &str, tail: &str) -> String {
    let variable = variable.replace(['\n', '\r'], " ");
    let room = MAX_LINE.saturating_sub(head.chars().count() + tail.chars().count());
    let shown = match variable.chars().count() > room {
        true => format!(
            "{}...",
            variable
                .chars()
                .take(room.saturating_sub(3))
                .collect::<String>()
        ),
        false => variable,
    };
    line(&format!("{head}{shown}{tail}"))
}

fn notable(info: &BranchInfo, asking: Option<&Message>) -> Option<Label> {
    let name = &info.name;
    let label = |state: String, urgent: bool, group: Group, line: String| {
        Some(Label {
            state,
            urgent,
            group,
            line,
        })
    };
    if let Some(message) = asking {
        let verb = match message.kind {
            MessageKind::Escalation => "escalates",
            _ => "asks",
        };
        let (id, to) = (message.id, &message.to);
        return label(
            format!(
                "asking (#{} {}): {}",
                message.id, message.kind, message.text
            ),
            true,
            Group::Other,
            fitted(
                &format!("- {name} (under {to}): {verb} #{id}: \""),
                &message.text,
                &format!("\". Answer: by answer --as {to} {id} \"TEXT\""),
            ),
        );
    }
    match &info.status {
        BranchStatus::Running
        | BranchStatus::Waiting
        | BranchStatus::WaitingOnChildren
        | BranchStatus::Merged { .. }
        | BranchStatus::Discarded { .. } => None,
        BranchStatus::Ready => {
            let files = info.candidate.as_ref().map_or(0, |c| c.files_changed);
            let s = plural(files as usize);
            let text = match &info.parent {
                Some(parent) => {
                    format!("- {name} (under {parent}): ready; {parent} integrates it: by show {name}")
                }
                None => format!(
                    "- {name}: ready to merge, {files} file{s} changed: by diff {name}, then by merge {name}"
                ),
            };
            label(
                format!("finished, ready to integrate ({files} file{s} changed)"),
                false,
                Group::Ready,
                line(&text),
            )
        }
        BranchStatus::NoChanges => label(
            "finished with no changes".into(),
            false,
            Group::Other,
            line(&format!("- {name}: finished with no changes. by show {name}")),
        ),
        BranchStatus::Failed { reason } => label(
            format!("failed: {reason}"),
            false,
            Group::Failed,
            fitted(
                &format!("- {name}: failed: "),
                reason,
                &format!(". by log {name} shows why"),
            ),
        ),
        BranchStatus::BudgetExceeded { limit } => label(
            format!("stopped at its {limit} limit"),
            false,
            Group::Failed,
            fitted(
                &format!("- {name}: stopped at its "),
                limit,
                &format!(" limit. by log {name} shows why"),
            ),
        ),
        BranchStatus::Interrupted => label(
            "interrupted".into(),
            false,
            Group::Failed,
            line(&format!("- {name}: interrupted. by log {name} shows why")),
        ),
        BranchStatus::Blocked { reason } => label(
            format!("blocked: {reason}"),
            true,
            Group::Other,
            fitted(
                &format!("- {name}: blocked: "),
                reason,
                &format!(". by show {name}"),
            ),
        ),
        BranchStatus::AwaitingPlanApproval => label(
            "waiting for its plan to be approved".into(),
            true,
            Group::Other,
            line(&format!(
                "- {name}: waiting for plan approval: by plan show {name}, then by plan approve {name}"
            )),
        ),
    }
}

/// `lines`, at most [`MAX_LINES`] of them, the last saying how many more.
fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_LINES {
        let more = lines.len() - (MAX_LINES - 1);
        lines.truncate(MAX_LINES - 1);
        lines.push(format!("- ... and {more} more: by ls"));
    }
    lines
}

/// One line of at most [`MAX_LINE`] characters, newlines folded.
fn line(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    match flat.chars().count() > MAX_LINE {
        true => format!("{}...", flat.chars().take(MAX_LINE - 3).collect::<String>()),
        false => flat,
    }
}

fn plural(n: usize) -> &'static str {
    match n {
        1 => "",
        _ => "s",
    }
}

/// The status line for Claude Code's JSON `input`: `None` outside a yard.
#[cfg(test)]
pub(crate) fn status_line(
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    status_line_as(&DIALECTS[0], input, env, cwd)
}

/// The status line for `dialect`'s JSON `input`, its directory read from
/// the harness's own field, else this process's: `None` outside a yard.
pub(crate) fn status_line_as(
    dialect: &Dialect,
    input: &str,
    env: &dyn Fn(&str) -> Option<String>,
    cwd: &Path,
) -> Result<Option<String>, String> {
    let input = parse(input);
    let dir =
        first_str(&input, dialect.status_cwd).map_or_else(|| cwd.to_path_buf(), PathBuf::from);
    let Some(source) = source(&dir, env)? else {
        return Ok(None);
    };
    let Some(glance) = look(&source, None)? else {
        return Ok(None);
    };
    let mut parts = Vec::new();
    if let Some(branch) =
        env("BRANCHYARD_BRANCH").or_else(|| glance.branch_at(&dir).map(|info| info.name.clone()))
    {
        parts.push(branch);
    }
    let counts = glance.counts();
    for (n, word) in [
        (counts.running, "running"),
        (counts.waiting, "waiting"),
        (counts.asking, "asking"),
        (counts.blocked, "blocked"),
        (counts.ready, "ready"),
        (counts.failed, "failed"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        parts.push(match glance.branches.is_empty() {
            true => "no branches".to_owned(),
            false => "idle".to_owned(),
        });
    }
    Ok(Some(format!("by: {}\n", parts.join(" · "))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_safe_file_names() {
        assert_eq!(session_file_name("abc-123_x"), "abc-123_x");
        assert_eq!(session_file_name("../../etc/passwd"), "etcpasswd");
        assert_eq!(session_file_name(""), "default");
        assert_eq!(session_file_name("///"), "default");
        assert_eq!(session_file_name(&"a".repeat(300)).len(), 100);
    }

    #[test]
    fn lines_are_capped_and_folded() {
        let many: Vec<String> = (0..20).map(|n| n.to_string()).collect();
        let shown = capped(many);
        assert_eq!(shown.len(), MAX_LINES);
        assert_eq!(shown[MAX_LINES - 1], "- ... and 9 more: by ls");
        assert_eq!(line("a\nb"), "a b");
        assert_eq!(line(&"x".repeat(500)).chars().count(), MAX_LINE);
    }

    fn branch(name: &str, parent: Option<&str>, status: BranchStatus, files: u32) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: PathBuf::from(format!("/repo/.branchyard/worktrees/{name}")),
            prompt: "Fix the flaky test".into(),
            harness: "claude-code".into(),
            profile: "claude-code-stream-json".into(),
            session: None,
            parent: parent.map(str::to_owned),
            provider: "local".into(),
            isolated: false,
            children: Vec::new(),
            depth: u32::from(parent.is_some()),
            base: "0123456789abcdef".into(),
            candidate: (files > 0).then(|| branchyard::CandidateInfo {
                commit: "fedcba9876543210".into(),
                files_changed: files,
                insertions: 3,
                deletions: 1,
            }),
            status,
            turns: 1,
            cost_usd: None,
            created_at: 10_000,
            stalled: false,
            superseded_by: None,
            model: None,
        }
    }

    fn question(id: u64, from: &str, to: &str, kind: MessageKind, text: &str) -> Message {
        Message {
            id,
            from: from.into(),
            to: to.into(),
            kind,
            text: text.into(),
            in_reply_to: None,
            at_ms: 1,
            delivered: false,
        }
    }

    fn glance_of(branches: Vec<BranchInfo>, asking: Vec<Message>) -> Glance {
        Glance {
            root: PathBuf::from("/repo"),
            branches,
            asking,
            unread: Vec::new(),
        }
    }

    /// Every `by ...` command in `text`, as the words a shell would give
    /// `by`, with the placeholder `ID` made a number. A command ends at a
    /// comma, a semicolon, a sentence's full stop or `shows why`.
    fn commands_in(text: &str) -> Vec<Vec<String>> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("by ") {
            let before_is_word = rest[..at].chars().last().is_some_and(char::is_alphanumeric);
            let tail = &rest[at..];
            rest = &rest[at + 3..];
            let next_is_command = tail[3..].chars().next().is_some_and(char::is_lowercase);
            if before_is_word || !next_is_command {
                continue;
            }
            let end = [", then ", ";", " shows why"]
                .iter()
                .filter_map(|stop| tail.find(stop))
                .min()
                .unwrap_or(tail.len());
            let command = tail[..end].trim_end_matches('.');
            let words = crate::args::split_words(command).unwrap();
            found.push(
                words
                    .into_iter()
                    .map(|w| if w == "ID" { "7".to_owned() } else { w })
                    .collect(),
            );
        }
        found
    }

    /// Each command `text` names must be one `by` accepts, so a hook line
    /// cannot send a session to a subcommand or flag that is gone.
    fn assert_commands_parse(text: &str, expected: &[&str]) {
        let commands = commands_in(text);
        let shown: Vec<String> = commands.iter().map(|words| words.join(" ")).collect();
        assert_eq!(shown, expected, "{text}");
        for words in commands {
            let argv = std::iter::once("by".to_owned()).chain(words.iter().skip(1).cloned());
            match crate::args::parse_from(argv) {
                Ok(cli) => assert!(cli.command.is_some(), "{words:?}: {text}"),
                Err(error) => panic!("{text}\n{words:?} does not parse: {error}"),
            }
        }
    }

    #[test]
    fn a_changed_branch_is_named_with_the_command_that_deals_with_it() {
        let seen = Cursor::default();
        let infos = vec![
            branch("hello", None, BranchStatus::Ready, 2),
            branch("kid", Some("lead"), BranchStatus::Ready, 1),
            branch(
                "bad",
                None,
                BranchStatus::Failed {
                    reason: "check failed".into(),
                },
                0,
            ),
            branch("slow", None, BranchStatus::Interrupted, 0),
            branch(
                "big",
                None,
                BranchStatus::BudgetExceeded { limit: "$2".into() },
                0,
            ),
            branch("idle", None, BranchStatus::NoChanges, 0),
            branch("wait", None, BranchStatus::AwaitingPlanApproval, 0),
            branch(
                "stuck",
                None,
                BranchStatus::Blocked {
                    reason: "no tool".into(),
                },
                0,
            ),
            branch("asker", Some("lead"), BranchStatus::Running, 0),
        ];
        let asking = vec![question(
            9,
            "asker",
            "lead",
            MessageKind::Question,
            "Which parser?",
        )];
        let here = glance_of(infos, asking);
        let (lines, next) = change_lines(&here, Some(&seen));
        assert_eq!(next.branches.len(), 9);
        assert_eq!(
            lines,
            [
                HEADER,
                "- hello: ready to merge, 2 files changed: by diff hello, then by merge hello",
                "- kid (under lead): ready; lead integrates it: by show kid",
                "- bad: failed: check failed. by log bad shows why",
                "- slow: interrupted. by log slow shows why",
                "- big: stopped at its $2 limit. by log big shows why",
                "- idle: finished with no changes. by show idle",
                "- wait: waiting for plan approval: by plan show wait, then by plan approve wait",
                "- stuck: blocked: no tool. by show stuck",
                "- asker (under lead): asks #9: \"Which parser?\". Answer: by answer --as lead 9 \"TEXT\"",
            ]
        );
        assert!(lines.len() <= MAX_LINES);
        assert_commands_parse(&lines[1], &["by diff hello", "by merge hello"]);
        assert_commands_parse(&lines[2], &["by show kid"]);
        assert_commands_parse(&lines[3], &["by log bad"]);
        assert_commands_parse(&lines[4], &["by log slow"]);
        assert_commands_parse(&lines[5], &["by log big"]);
        assert_commands_parse(&lines[6], &["by show idle"]);
        assert_commands_parse(&lines[7], &["by plan show wait", "by plan approve wait"]);
        assert_commands_parse(&lines[8], &["by show stuck"]);
        assert_commands_parse(&lines[9], &["by answer --as lead 9 TEXT"]);
        // docs/harness-hooks.md shows these lines; it keeps showing what is said.
        let doc = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/harness-hooks.md"),
        )
        .unwrap();
        for shown in [0, 1, 2, 3, 7, 8, 9] {
            assert!(doc.contains(lines[shown].as_str()), "{}", lines[shown]);
        }
        // Told once: the cursor those lines left says nothing more.
        assert_eq!(change_lines(&here, Some(&next)).0, Vec::<String>::new());
    }

    #[test]
    fn an_escalation_is_answered_by_whoever_it_went_to() {
        let infos = vec![branch("kid", Some("lead"), BranchStatus::Running, 0)];
        let asking = vec![question(
            4,
            "kid",
            "root",
            MessageKind::Escalation,
            "May I drop the table?",
        )];
        let (lines, _) = change_lines(&glance_of(infos, asking), None);
        assert_eq!(
            lines[1],
            "- kid (under root): escalates #4: \"May I drop the table?\". \
             Answer: by answer --as root 4 \"TEXT\""
        );
        assert_commands_parse(&lines[1], &["by answer --as root 4 TEXT"]);
    }

    #[test]
    fn a_first_call_names_what_waits_and_counts_what_runs() {
        let mut infos = vec![
            branch("r1", None, BranchStatus::Ready, 1),
            branch("r2", None, BranchStatus::Ready, 1),
            branch("r3", None, BranchStatus::Ready, 3),
            branch("r4", None, BranchStatus::Ready, 4),
            branch(
                "f1",
                None,
                BranchStatus::Failed {
                    reason: "boom".into(),
                },
                0,
            ),
            branch("w", None, BranchStatus::Waiting, 0),
            branch(
                "merged",
                None,
                BranchStatus::Merged {
                    target: "main".into(),
                    commit: "c".into(),
                },
                0,
            ),
        ];
        infos.push(branch("run1", None, BranchStatus::Running, 0));
        infos.push(branch("run2", None, BranchStatus::WaitingOnChildren, 0));
        let (lines, next) = change_lines(&glance_of(infos, Vec::new()), None);
        assert_eq!(
            lines,
            [
                HEADER,
                "- r2: ready to merge, 1 file changed: by diff r2, then by merge r2",
                "- r3: ready to merge, 3 files changed: by diff r3, then by merge r3",
                "- r4: ready to merge, 4 files changed: by diff r4, then by merge r4",
                "- ... and 1 more ready: by ls",
                "- f1: failed: boom. by log f1 shows why",
                "- 2 running: by ls",
            ]
        );
        assert_commands_parse(lines.last().unwrap(), &["by ls"]);
        assert_commands_parse(&lines[4], &["by ls"]);
        // The cursor still remembers every ready and failed branch, so a
        // later call is told only of what changes.
        assert_eq!(next.branches.len(), 5);
        // A session already told nothing new hears nothing.
        let infos = vec![branch("r1", None, BranchStatus::Ready, 1)];
        let (_, cursor) = change_lines(&glance_of(infos.clone(), Vec::new()), None);
        assert_eq!(
            change_lines(&glance_of(infos, Vec::new()), Some(&cursor)).0,
            Vec::<String>::new()
        );
    }

    #[test]
    fn nothing_to_name_or_count_is_nothing_to_say() {
        let infos = vec![branch(
            "done",
            None,
            BranchStatus::Merged {
                target: "main".into(),
                commit: "c".into(),
            },
            0,
        )];
        assert_eq!(
            change_lines(&glance_of(infos, Vec::new()), None).0,
            Vec::<String>::new()
        );
        assert_eq!(
            change_lines(&glance_of(Vec::new(), Vec::new()), None).0,
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_long_reason_gives_way_to_the_command_that_follows_it() {
        let reason = format!("check failed\n{}", "because ".repeat(100));
        let infos = vec![branch("bad", None, BranchStatus::Failed { reason }, 0)];
        let (lines, _) = change_lines(&glance_of(infos, Vec::new()), None);
        let said = &lines[1];
        assert_eq!(said.chars().count(), MAX_LINE, "{said}");
        assert!(
            said.starts_with("- bad: failed: check failed because"),
            "{said}"
        );
        assert!(
            said.contains("...") && said.ends_with(". by log bad shows why"),
            "{said}"
        );
        assert_commands_parse(said, &["by log bad"]);

        let ask = "Should the retry be per request or per batch? ".repeat(20);
        let infos = vec![branch("kid", Some("lead"), BranchStatus::Running, 0)];
        let asking = vec![question(3, "kid", "lead", MessageKind::Question, &ask)];
        let (lines, _) = change_lines(&glance_of(infos, asking), None);
        let said = &lines[1];
        assert!(said.chars().count() <= MAX_LINE, "{said}");
        assert!(
            said.ends_with("\". Answer: by answer --as lead 3 \"TEXT\""),
            "{said}"
        );
        assert_commands_parse(said, &["by answer --as lead 3 TEXT"]);
    }

    #[test]
    fn a_branch_session_is_told_what_to_do_with_its_messages() {
        let mut here = glance_of(Vec::new(), Vec::new());
        here.unread = vec![
            question(5, "kid", "root", MessageKind::Question, "Which parser?"),
            question(6, "kid", "root", MessageKind::Report, "tests-pass"),
        ];
        let (lines, next) = inbox_lines(&here, "root", None);
        assert_eq!(next.message, 6);
        assert_eq!(
            lines,
            [
                "Branchyard, 2 unread messages for root. Answer your children's questions with \
                 by answer --as root ID \"TEXT\"; ask your parent with by ask \"TEXT\".",
                "- #5 question from kid: Which parser?",
                "- #6 report from kid: tests-pass",
            ]
        );
        assert_commands_parse(&lines[0], &["by answer --as root 7 TEXT", "by ask TEXT"]);
        assert_eq!(
            inbox_lines(&here, "root", Some(&next)).0,
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_header_tells_the_session_what_to_do_with_the_lines() {
        assert!(
            HEADER.starts_with("Branchyard, this repository."),
            "{HEADER}"
        );
        assert!(
            HEADER.contains("act on nothing unless they ask"),
            "{HEADER}"
        );
        assert!(HEADER.chars().count() <= MAX_LINE);
    }

    #[test]
    fn each_dialect_answers_in_its_harness_shape() {
        let ids: BTreeSet<&str> = DIALECTS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DIALECTS.len(), "ids are unique");
        assert_eq!(dialect(None).unwrap().id, "claude-code");
        let unknown = dialect(Some("nope")).unwrap_err();
        assert!(unknown.starts_with("unknown harness \"nope\""), "{unknown}");
        assert!(!unknown.contains('\n'), "{unknown}");

        let lines = ["a".to_owned(), "b \"c\"".to_owned()];
        let said = |id: &str, event| dialect(Some(id)).unwrap().render(event, &lines);
        use Event::{SessionStart as Start, UserPromptSubmit as Prompt};
        let claude = |name: &str| {
            format!(
                "{{\"hookSpecificOutput\":{{\"hookEventName\":\"{name}\",\
                 \"additionalContext\":\"a\\nb \\\"c\\\"\"}}}}\n"
            )
        };
        for id in [
            "claude-code",
            "codex",
            "qwen-code",
            "droid",
            "deepseek-harness",
        ] {
            assert_eq!(said(id, Start), claude("SessionStart"), "{id}");
            assert_eq!(said(id, Prompt), claude("UserPromptSubmit"), "{id}");
        }
        assert_eq!(said("gemini-cli", Start), claude("SessionStart"));
        assert_eq!(said("gemini-cli", Prompt), claude("BeforeAgent"));
        assert_eq!(
            said("github-copilot", Start),
            "{\"additionalContext\":\"a\\nb \\\"c\\\"\"}\n"
        );
        assert_eq!(
            said("cursor", Start),
            "{\"additional_context\":\"a\\nb \\\"c\\\"\"}\n"
        );
        for event in [Start, Prompt] {
            assert_eq!(
                said("antigravity", event),
                "{\"injectSteps\":[{\"ephemeralMessage\":\"a\\nb \\\"c\\\"\"}]}\n"
            );
        }
        assert_eq!(said("kiro", Prompt), "a\nb \"c\"\n");
        for id in ["hermes", "generic"] {
            assert_eq!(
                said(id, Prompt),
                "{\"context\":\"a\\nb \\\"c\\\"\"}\n",
                "{id}"
            );
        }
    }

    #[test]
    fn each_dialect_finds_its_session_and_directory() {
        let found = |id: &str, input: Value| {
            let d = dialect(Some(id)).unwrap();
            (
                first_str(&input, d.session).map(str::to_owned),
                first_str(&input, d.cwd).map(str::to_owned),
                first_str(&input, d.status_cwd).map(str::to_owned),
            )
        };
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(
            found(
                "github-copilot",
                serde_json::json!({"sessionId": "s", "cwd": "/r"})
            ),
            (some("s"), some("/r"), some("/r"))
        );
        assert_eq!(
            found(
                "cursor",
                serde_json::json!({"conversation_id": "c", "workspace_roots": ["/r", "/o"]})
            ),
            (some("c"), some("/r"), None)
        );
        assert_eq!(
            found(
                "antigravity",
                serde_json::json!({"conversationId": "c", "workspacePaths": ["/r"]})
            ),
            (some("c"), some("/r"), some("/r"))
        );
        // An empty field is no field: the next one is read.
        assert_eq!(
            found(
                "claude-code",
                serde_json::json!({"session_id": "", "workspace": {"current_dir": ""}, "cwd": "/r"})
            ),
            (None, some("/r"), some("/r"))
        );
    }

    #[test]
    fn a_harness_that_drops_an_events_output_is_told_nothing_there() {
        let dir = branchyard_testkit::Scratch::new("hook-deaf");
        let none = |_: &str| None;
        for (id, event) in [
            ("github-copilot", "user-prompt-submit"),
            ("cursor", "user-prompt-submit"),
            ("hermes", "session-start"),
            ("gemini-cli", "session-start"),
        ] {
            let d = dialect(Some(id)).unwrap();
            assert_eq!(
                hook_output_as(d, event, "{}", &none, dir.path()),
                Ok(None),
                "{id}"
            );
            // The event and the input are still checked.
            assert!(hook_output_as(d, "stop", "{}", &none, dir.path()).is_err());
        }
    }

    #[test]
    fn an_unknown_event_or_no_yard_prints_nothing() {
        let dir = branchyard_testkit::Scratch::new("hook-unit");
        let none = |_: &str| None;
        assert!(hook_output("stop", "{}", &none, dir.path()).is_err());
        assert!(hook_output("session-start", "not json", &none, dir.path()).is_err());
        assert_eq!(
            hook_output("session-start", "{}", &none, dir.path()),
            Ok(None)
        );
        assert_eq!(status_line("", &none, dir.path()), Ok(None));
    }

    #[test]
    fn the_source_is_where_by_would_read() {
        let dir = branchyard_testkit::Scratch::new("hook-source");
        let root = dir.path().to_path_buf();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap();
        assert!(init.success());
        let below = root.join("src");
        std::fs::create_dir_all(&below).unwrap();
        let none = |_: &str| None;
        assert_eq!(source(&below, &none), Ok(None));

        // A server named by the variables, whatever is here.
        let remote = |name: &str| match name {
            "BRANCHYARD_REMOTE" => Some("http://127.0.0.1:9".to_owned()),
            "BRANCHYARD_REPO" => Some("app".to_owned()),
            _ => None,
        };
        match source(&below, &remote) {
            Ok(Some(Source::Remote { root: at, globals })) => {
                assert_eq!(at, root);
                assert_eq!(globals.repo.as_deref(), Some("app"));
                assert_eq!(globals.token_file, None);
            }
            other => panic!("{other:?}"),
        }
        assert!(look(&source(&below, &remote).unwrap().unwrap(), None)
            .unwrap_err()
            .contains("needs a token"));
        // Nothing was made for it.
        assert!(!root.join(".branchyard").exists());

        // A server configuration keeping the state in PostgreSQL.
        let served = root.join("server.json");
        std::fs::write(
            &served,
            serde_json::json!({
                "repos": {"app": root},
                "data_dir": root.join("data"),
                "database": "postgres://nobody@127.0.0.1:9/none",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("branchyard.toml"),
            format!("[serve]\nconfig = \"{}\"\n", served.display()),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(
            source(&below, &none),
            Ok(Some(Source::Postgres {
                root: canonical,
                url: "postgres://nobody@127.0.0.1:9/none".into(),
                scope: "app".into(),
            }))
        );
        // Unreachable, or not built in: an error, so nothing is printed.
        assert!(look(&source(&below, &none).unwrap().unwrap(), None).is_err());
        assert_eq!(status_line("{}", &none, &below).ok(), None);

        // A state.db here comes first.
        drop(branchyard::Yard::open(&root).unwrap());
        assert_eq!(source(&below, &none), Ok(Some(Source::Local(root.clone()))));
        assert_eq!(
            status_line("{}", &none, &below),
            Ok(Some("by: no branches\n".into()))
        );
    }
}
