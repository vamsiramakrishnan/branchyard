// Derived from stablyai/orca at revision
// 280733273545f0b3eeedc1be54b14d406239030e:
// src/main/ai-vault/claude-project-dir-encoding.ts,
// src/main/ai-vault/session-scanner-codex-session-meta.ts and
// src/main/ai-vault/session-scanner-codex-non-user-origin.ts.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust; the NFC
// spelling of a macOS path is not tried (Rust's standard library has no
// Unicode normalization); a Codex session's origin is read only to skip
// threads Codex started itself (subagents, reviews, compactions), without
// their parentage; and the sessions found are turned into Branchyard
// branches rather than listed in Orca's AI Vault.

//! `by adopt`: the Claude Code and Codex sessions already on this machine
//! for this repository, read-only from their own session stores, and one
//! of them made a Branchyard branch whose next turn resumes it natively.
//! See `docs/usage.md#by-adopt`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use branchyard::{AdoptSpec, Adoption, Yard};
use branchyard_workspace::Git;
use serde::Serialize;
use serde_json::Value;

use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::usage;
use branchyard_support::time::parse_rfc3339;

/// Orca's `encodeClaudeProjectPath`: one dash per character that is not
/// an ASCII letter or digit (runs are not collapsed), a trailing slash
/// dropped. Claude Code names a project's directory this way.
pub fn encode_claude_project(path: &str) -> String {
    let trimmed = match path {
        "/" => path,
        _ => path.trim_end_matches('/'),
    };
    trimmed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Orca's `isClaudeProjectDirInScope`: the directory is `prefix`'s, or a
/// subdirectory's (`prefix-…`).
pub fn claude_dir_in_scope(dir_name: &str, prefix: &str) -> bool {
    dir_name == prefix || dir_name.starts_with(&format!("{prefix}-"))
}

/// Orca's `readCodexNonUserOrigin`, reduced to its verdict: whether a
/// Codex `session_meta` payload says the thread is not the user's own (a
/// subagent Codex spawned, or a review, compaction or guardian it ran).
pub fn codex_not_users(payload: &Value) -> bool {
    let thread_source = payload
        .get("thread_source")
        .or_else(|| payload.get("threadSource"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    if let Some(source) = thread_source {
        // The provider's own verdict outranks `source`.
        return !source.eq_ignore_ascii_case("user");
    }
    let tag = match &payload["source"] {
        Value::String(tag) => Some(tag.as_str()),
        Value::Object(map) if map.len() == 1 => map.keys().next().map(String::as_str),
        _ => None,
    };
    matches!(tag, Some("subagent" | "internal"))
}

/// Orca's `extractCodexSessionMetadataTitle`.
fn codex_title(payload: &Value) -> Option<String> {
    ["title", "thread_name", "threadName"]
        .iter()
        .filter_map(|k| payload[*k].as_str())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|t| !t.is_empty())
}

/// A session found on this machine.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Session {
    /// `claude-code` or `codex`.
    pub harness: &'static str,
    pub id: String,
    /// Its transcript.
    pub file: PathBuf,
    pub cwd: Option<String>,
    /// The git branch it recorded.
    pub git_branch: Option<String>,
    /// The commit it recorded (Codex only).
    pub commit: Option<String>,
    pub title: String,
    pub model: Option<String>,
    pub started_ms: Option<u64>,
    pub updated_ms: Option<u64>,
    pub turns: u32,
}

fn lines(path: &Path) -> impl Iterator<Item = String> {
    fs::File::open(path)
        .ok()
        .map(BufReader::new)
        .into_iter()
        .flat_map(|r| r.lines().map_while(Result::ok))
}

fn one_line(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

/// A user message's text, from a string or an array of text parts.
fn message_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text" || p["type"] == "input_text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    // Tool results and command wrappers are not what a person typed.
    let trimmed = text.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('<')).then(|| one_line(trimmed))
}

fn modified_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(branchyard_support::time::system_time_ms)
}

/// One Claude Code transcript.
pub fn claude_session(file: &Path) -> Option<Session> {
    let id = file.file_stem()?.to_str()?.to_owned();
    let mut session = Session {
        harness: "claude-code",
        id,
        file: file.to_path_buf(),
        cwd: None,
        git_branch: None,
        commit: None,
        title: String::new(),
        model: None,
        started_ms: None,
        updated_ms: None,
        turns: 0,
    };
    let mut summary = None;
    for line in lines(file) {
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record["type"] == "summary" {
            summary = record["summary"].as_str().map(one_line);
            continue;
        }
        if let Some(at) = record["timestamp"]
            .as_str()
            .and_then(|t| parse_rfc3339(t).ok())
        {
            session.started_ms = Some(session.started_ms.map_or(at, |s| s.min(at)));
            session.updated_ms = Some(session.updated_ms.map_or(at, |u| u.max(at)));
        }
        if let Some(cwd) = record["cwd"].as_str() {
            session.cwd = Some(cwd.to_owned());
        }
        if let Some(branch) = record["gitBranch"].as_str().filter(|b| !b.is_empty()) {
            session.git_branch = Some(branch.to_owned());
        }
        if let Some(id) = record["sessionId"].as_str() {
            session.id = id.to_owned();
        }
        match record["type"].as_str() {
            Some("user") if record["isSidechain"] != true && record["isMeta"] != true => {
                if let Some(text) = message_text(&record["message"]["content"]) {
                    session.turns += 1;
                    if session.title.is_empty() {
                        session.title = text;
                    }
                }
            }
            Some("assistant") => {
                if let Some(model) = record["message"]["model"].as_str() {
                    if !model.starts_with('<') {
                        session.model = Some(model.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(summary) = summary {
        session.title = summary;
    }
    session.updated_ms = session.updated_ms.or_else(|| modified_ms(file));
    (session.turns > 0).then_some(session)
}

/// One Codex rollout; `None` for a thread Codex started itself, or (read
/// no further than its first record) one whose directory `wanted` refuses.
pub fn codex_session(file: &Path, wanted: &dyn Fn(&str) -> bool) -> Option<Session> {
    let stem = file.file_stem()?.to_str()?;
    let mut session = Session {
        harness: "codex",
        // `rollout-<time>-<uuid>`: the ID is the trailing UUID.
        id: stem
            .get(stem.len().saturating_sub(36)..)
            .unwrap_or(stem)
            .to_owned(),
        file: file.to_path_buf(),
        cwd: None,
        git_branch: None,
        commit: None,
        title: String::new(),
        model: None,
        started_ms: None,
        updated_ms: None,
        turns: 0,
    };
    let mut meta_title = None;
    for line in lines(file) {
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(at) = record["timestamp"]
            .as_str()
            .and_then(|t| parse_rfc3339(t).ok())
        {
            session.started_ms = Some(session.started_ms.map_or(at, |s| s.min(at)));
            session.updated_ms = Some(session.updated_ms.map_or(at, |u| u.max(at)));
        }
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("session_meta") => {
                if codex_not_users(payload) {
                    return None;
                }
                if let Some(id) = payload["id"].as_str() {
                    session.id = id.to_owned();
                }
                session.cwd = payload["cwd"].as_str().map(str::to_owned).or(session.cwd);
                if session.cwd.as_deref().is_some_and(|cwd| !wanted(cwd)) {
                    return None;
                }
                let git = &payload["git"];
                session.commit = git["commit_hash"].as_str().map(str::to_owned);
                session.git_branch = git["branch"]
                    .as_str()
                    .or(git["current_branch"].as_str())
                    .map(str::to_owned);
                meta_title = codex_title(payload);
            }
            Some("turn_context") => {
                if let Some(cwd) = payload["cwd"].as_str() {
                    session.cwd = Some(cwd.to_owned());
                }
                if let Some(model) = payload["model"].as_str() {
                    session.model = Some(model.to_owned());
                }
            }
            Some("event_msg") if payload["type"] == "user_message" => {
                if let Some(text) = message_text(&payload["message"]) {
                    session.turns += 1;
                    if session.title.is_empty() {
                        session.title = text;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(title) = meta_title {
        session.title = title;
    }
    session.updated_ms = session.updated_ms.or_else(|| modified_ms(file));
    (session.turns > 0).then_some(session)
}

fn jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            jsonl(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `cwd` is in the repository at `root`, and not one of
/// Branchyard's own worktrees there.
fn in_repository(cwd: &str, root: &Path) -> bool {
    let cwd = canonical(Path::new(cwd));
    let root = canonical(root);
    cwd.starts_with(&root) && !cwd.starts_with(root.join(".branchyard"))
}

/// Every Claude Code and Codex session for the repository at `root`, newest
/// first, from the default logins' stores (`CLAUDE_CONFIG_DIR` or
/// `~/.claude`, `CODEX_HOME` or `~/.codex`). Read only.
pub fn sessions(root: &Path, env: &dyn Fn(&str) -> Option<String>) -> Vec<Session> {
    let mut found = Vec::new();
    if let Some(dir) = usage::default_dir("claude-code", env) {
        let projects = dir.join("projects");
        let prefixes = [
            encode_claude_project(&root.display().to_string()),
            encode_claude_project(&canonical(root).display().to_string()),
        ];
        for entry in fs::read_dir(&projects).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !prefixes.iter().any(|p| claude_dir_in_scope(&name, p)) {
                continue;
            }
            // Top-level transcripts only: subagents' are under their
            // session's directory.
            for file in fs::read_dir(entry.path()).into_iter().flatten().flatten() {
                let path = file.path();
                if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
                    found.extend(claude_session(&path));
                }
            }
        }
    }
    if let Some(dir) = usage::default_dir("codex", env) {
        let mut files = Vec::new();
        for sub in ["sessions", "archived_sessions"] {
            jsonl(&dir.join(sub), &mut files);
        }
        let ours = |cwd: &str| in_repository(cwd, root);
        found.extend(files.iter().filter_map(|f| codex_session(f, &ours)));
    }
    found.retain(|s| s.cwd.as_deref().is_some_and(|cwd| in_repository(cwd, root)));
    found.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(a.id.cmp(&b.id)));
    found.dedup_by(|a, b| a.harness == b.harness && a.id == b.id);
    found
}

/// The session `wanted` names: its ID, or a unique start of it.
pub fn find<'a>(sessions: &'a [Session], wanted: &str) -> Result<&'a Session, Failure> {
    if let Some(exact) = sessions.iter().find(|s| s.id == wanted) {
        return Ok(exact);
    }
    let matching: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.id.starts_with(wanted))
        .collect();
    match matching.as_slice() {
        [one] => Ok(one),
        [] => Err(Failure::Message(format!(
            "no Claude Code or Codex session {wanted:?} for this repository; by adopt --list \
             shows them"
        ))),
        many => Err(Failure::Message(format!(
            "{wanted:?} starts {} sessions' IDs; give more of it",
            many.len()
        ))),
    }
}

/// How a session becomes a branch: its base, an optional diff, and what to
/// tell the person.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub base: String,
    pub how: &'static str,
    pub diff: Option<Vec<u8>>,
    pub diff_files: Vec<String>,
    pub notes: Vec<String>,
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    Git::new(dir)
        .args(args)
        .run()
        .ok()
        .map(|s| s.trim().to_owned())
}

fn has_commit(root: &Path, commit: &str) -> bool {
    Git::new(root)
        .args(["cat-file", "-e"])
        .arg(format!("{commit}^{{commit}}"))
        .succeeds()
        .unwrap_or(false)
}

/// Decide the base, conservatively: the commit the session recorded (Codex)
/// if this repository has it, else the HEAD of the directory it ran in,
/// else this checkout's HEAD; and the directory's uncommitted changes to
/// tracked files, only when they are against that same base.
pub fn plan(root: &Path, session: &Session, with_diff: bool) -> Result<Plan, Failure> {
    let mut notes = Vec::new();
    let cwd = session
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir());
    let cwd_head = cwd
        .as_deref()
        .and_then(|d| git_out(d, &["rev-parse", "--verify", "HEAD"]))
        .filter(|h| has_commit(root, h));
    let (base, how) = match (&session.commit, &cwd_head) {
        (Some(commit), _) if has_commit(root, commit) => (commit.clone(), "session_commit"),
        (Some(commit), _) => {
            notes.push(format!(
                "the session recorded commit {}, which this repository does not have",
                short(commit)
            ));
            match &cwd_head {
                Some(head) => (head.clone(), "head"),
                None => (head(root)?, "head"),
            }
        }
        (None, Some(head)) => (head.clone(), "head"),
        (None, None) => {
            notes.push(match &session.cwd {
                Some(cwd) => format!("{cwd} is gone or not a checkout of this repository"),
                None => "the session recorded no directory".into(),
            });
            (head(root)?, "head")
        }
    };
    if session.harness == "claude-code" && how == "head" {
        notes.push(
            "Claude Code records no commit: the base is the HEAD of the directory it ran in \
             now, which may have moved since"
                .into(),
        );
    }
    let mut plan = Plan {
        base: base.clone(),
        how,
        diff: None,
        diff_files: Vec::new(),
        notes,
    };
    let Some(cwd) = cwd else {
        return Ok(plan);
    };
    let dirty =
        git_out(&cwd, &["status", "--porcelain", "--untracked-files=all"]).unwrap_or_default();
    if dirty.is_empty() {
        return Ok(plan);
    }
    let untracked = dirty.lines().filter(|l| l.starts_with("??")).count();
    if cwd_head.as_deref() != Some(base.as_str()) {
        plan.notes.push(format!(
            "{} has uncommitted changes, but against another commit than the base; they were \
             not carried over",
            cwd.display()
        ));
        return Ok(plan);
    }
    if !with_diff {
        plan.notes.push(format!(
            "{} has uncommitted changes; --no-diff left them out",
            cwd.display()
        ));
        return Ok(plan);
    }
    let top = git_out(&cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    let diff = Git::new(top.as_deref().unwrap_or(&cwd))
        .args(["diff", "--binary", "HEAD"])
        .run_bytes()
        .map_err(|e| Failure::Message(format!("could not read {}'s diff: {e}", cwd.display())))?;
    if !diff.is_empty() {
        plan.diff_files = git_out(&cwd, &["diff", "--name-only", "HEAD"])
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        plan.how = "head_with_diff";
        plan.diff = Some(diff);
        plan.notes.push(format!(
            "the uncommitted changes to {} tracked file{} in {} were applied: they are the \
             directory's, not necessarily all the session's",
            plan.diff_files.len(),
            if plan.diff_files.len() == 1 { "" } else { "s" },
            cwd.display()
        ));
    }
    if untracked > 0 {
        plan.notes.push(format!(
            "{untracked} untracked file{} in {} were not carried over",
            if untracked == 1 { "" } else { "s" },
            cwd.display()
        ));
    }
    Ok(plan)
}

fn head(root: &Path) -> Result<String, Failure> {
    git_out(root, &["rev-parse", "--verify", "HEAD"])
        .ok_or_else(|| Failure::Message("this repository has no commit to start from".into()))
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-cli
fn age(now_ms: u64, at: Option<u64>) -> String {
    at.map(|at| crate::render::age_text(now_ms.saturating_sub(at) / 1000))
        .unwrap_or_else(|| "-".into())
}

/// What `by adopt` was asked.
pub struct Asked<'a> {
    pub session: Option<&'a str>,
    pub name: Option<&'a str>,
    /// `--harness`: a profile of the session's harness.
    pub profile: Option<&'a str>,
    pub list: bool,
    pub with_diff: bool,
    pub json: bool,
}

/// `by adopt [--list] [SESSION] [--name N] [--harness ID] [--no-diff] [--json]`.
pub fn main(env: &Env, target: &Target, asked: &Asked<'_>) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by adopt reads the sessions on this machine and makes a local branch; it works in \
             local mode only"
                .into(),
        ));
    }
    if crate::workspace_cmd::in_harness().is_some() {
        return Err(Failure::Message(
            "by adopt is a person's command; a harness on a branch cannot adopt sessions".into(),
        ));
    }
    let yard = commands::open()?;
    let vars = |name: &str| std::env::var(name).ok();
    let found = sessions(yard.root(), &vars);
    let now = branchyard_support::time::now_ms();
    match (asked.session, asked.list) {
        (None, _) | (Some(_), true) => {
            let shown: Vec<&Session> = match asked.session {
                Some(wanted) => found.iter().filter(|s| s.id.starts_with(wanted)).collect(),
                None => found.iter().collect(),
            };
            if asked.json {
                return print(&crate::json::text(
                    &serde_json::to_value(&shown).unwrap_or_default(),
                ));
            }
            list_text(env, &yard, &shown, now)
        }
        (Some(wanted), false) => {
            let session = find(&found, wanted)?;
            let profile = match asked.profile {
                Some(profile) if usage::login_of(profile) == Some(session.harness) => profile,
                Some(profile) => {
                    return Err(Failure::Message(format!(
                        "{profile} is not a profile of {}, whose session this is",
                        session.harness
                    )))
                }
                None => session.harness,
            };
            adopt(&yard, session, asked, profile, now)
        }
    }
}

#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-cli
fn list_text(env: &Env, yard: &Yard, shown: &[&Session], now: u64) -> Outcome {
    if shown.is_empty() {
        return print(&format!(
            "no Claude Code or Codex sessions for {} (looked in the default logins' session \
             stores)\n",
            yard.root().display()
        ));
    }
    let style = env.style();
    let root = canonical(yard.root());
    let mut out = style.paint(
        crate::render::Tone::Dim,
        "SESSION   HARNESS      AGE  TURNS  WHERE  TASK\n",
    );
    for s in shown {
        let cwd = s
            .cwd
            .as_deref()
            .map(|c| {
                let rel = canonical(Path::new(c));
                match rel.strip_prefix(&root) {
                    Ok(p) if p.as_os_str().is_empty() => ".".to_owned(),
                    Ok(p) => p.display().to_string(),
                    Err(_) => c.to_owned(),
                }
            })
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{:<9} {:<12} {:>3}  {:>5}  {cwd:<5}  {}\n",
            s.id.get(..8).unwrap_or(&s.id),
            s.harness,
            age(now, s.updated_ms),
            s.turns,
            s.title
        ));
    }
    out.push_str(&style.paint(
        crate::render::Tone::Dim,
        "\nby adopt SESSION [--name NAME] makes one a branch; its next turn (by send) resumes it\n",
    ));
    print(&out)
}

/// Place a copy of a Claude Code transcript where Claude Code looks for the
/// sessions of `worktree`, so `--resume <id>` finds it there. The original
/// is not touched.
fn place_claude_transcript(session: &Session, worktree: &Path) -> Result<PathBuf, Failure> {
    let vars = |name: &str| std::env::var(name).ok();
    let dir = usage::default_dir("claude-code", &vars)
        .ok_or_else(|| Failure::Message("no home directory for Claude Code's store".into()))?;
    let project = dir.join("projects").join(encode_claude_project(
        &canonical(worktree).display().to_string(),
    ));
    fs::create_dir_all(&project)?;
    let target = project.join(format!("{}.jsonl", session.id));
    if !target.exists() {
        fs::copy(&session.file, &target)?;
    }
    Ok(target)
}

fn adopt(yard: &Yard, session: &Session, asked: &Asked<'_>, profile: &str, now: u64) -> Outcome {
    let as_json = asked.json;
    let mut plan = plan(yard.root(), session, asked.with_diff)?;
    if session
        .updated_ms
        .is_some_and(|u| now.saturating_sub(u) < 120_000)
    {
        plan.notes.push(
            "the session was active in the last two minutes: if it is still open, the branch \
             and it will diverge from here"
                .into(),
        );
    }
    let prompt = match session.title.trim() {
        "" => format!("Continue {} session {}", session.harness, session.id),
        title => title.to_owned(),
    };
    let adoption = Adoption {
        harness: session.harness.to_owned(),
        session: session.id.clone(),
        source: session.file.display().to_string(),
        cwd: session.cwd.clone(),
        base: plan.base.clone(),
        how: plan.how.to_owned(),
        diff_files: plan.diff_files.clone(),
        notes: plan.notes.clone(),
    };
    let branch = yard.adopt(AdoptSpec {
        name: asked.name.map(str::to_owned),
        prompt,
        harness: profile.to_owned(),
        base: plan.base.clone(),
        diff: plan.diff.take(),
        adoption,
    })?;
    let info = branch.info().clone();
    let mut placed = None;
    if session.harness == "claude-code" {
        placed = Some(place_claude_transcript(session, &info.worktree)?);
    }
    if as_json {
        return print(&crate::json::text(&serde_json::json!({
            "branch": info.name,
            "worktree": info.worktree,
            "harness": session.harness,
            "session": session.id,
            "base": plan.base,
            "how": plan.how,
            "diff_files": plan.diff_files,
            "notes": plan.notes,
            "transcript_placed": placed,
        })));
    }
    let how = match plan.how {
        "session_commit" => "the commit the session recorded",
        "head_with_diff" => "the HEAD of the directory it ran in, with its uncommitted changes",
        _ => "the HEAD of the directory it ran in",
    };
    let mut text = format!(
        "adopted {} session {} as {} at {} ({how})\n",
        session.harness,
        session.id,
        info.name,
        short(&plan.base)
    );
    for note in &plan.notes {
        text.push_str(&format!("  note: {note}\n"));
    }
    if let Some(placed) = placed {
        text.push_str(&format!(
            "  its transcript was copied to {} so Claude Code resumes it in the new worktree\n",
            placed.display()
        ));
    }
    text.push_str(&format!(
        "next: by send {} \"…\" resumes the session in {}\n",
        info.name,
        info.worktree.display()
    ));
    print(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_paths_encode_as_orca_does() {
        assert_eq!(
            encode_claude_project("/home/ada/src/app/"),
            "-home-ada-src-app"
        );
        assert_eq!(
            encode_claude_project("/home/ada/.claude"),
            "-home-ada--claude"
        );
        assert_eq!(encode_claude_project("/"), "-");
        assert!(claude_dir_in_scope("-r-app", "-r-app"));
        assert!(claude_dir_in_scope("-r-app-web", "-r-app"));
        assert!(!claude_dir_in_scope("-r-appdyne", "-r-app"));
    }

    /// The vendored sources still have the shape this port follows.
    #[test]
    fn orcas_sources_are_the_ones_ported() {
        let encoding =
            include_str!("../../../vendor/orca/src/main/ai-vault/claude-project-dir-encoding.ts");
        assert!(encoding.contains("return trimmed.replace(/[^a-zA-Z0-9]/g, '-')"));
        assert!(encoding.contains("projectDirName.startsWith(`${prefix}-`)"));
        let origin = include_str!(
            "../../../vendor/orca/src/main/ai-vault/session-scanner-codex-non-user-origin.ts"
        );
        assert!(origin.contains("new Set(['subagent', 'internal'])"));
    }

    #[test]
    fn codex_origins_are_read_as_orca_reads_them() {
        assert!(!codex_not_users(&json!({"source": "cli"})));
        assert!(!codex_not_users(&json!({})));
        assert!(codex_not_users(
            &json!({"source": {"subagent": {"thread_spawn": {}}}})
        ));
        assert!(codex_not_users(&json!({"source": "internal"})));
        // A stated thread_source wins over `source`.
        assert!(!codex_not_users(
            &json!({"source": "subagent", "thread_source": "user"})
        ));
        assert!(codex_not_users(&json!({"thread_source": "review"})));
        assert_eq!(
            codex_title(&json!({"thread_name": "  fix   the parser "})).as_deref(),
            Some("fix the parser")
        );
    }
}
