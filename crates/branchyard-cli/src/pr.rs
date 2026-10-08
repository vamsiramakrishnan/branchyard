//! From a branch to a pull request, through `gh`: `by run --issue`
//! ([`issue_task`]), `by pr` ([`publish`]), `by pr --watch` ([`watch`]) and
//! the merge-readiness line of `by show` ([`readiness`]). See
//! `docs/pull-requests.md`.
//!
//! What happened is recorded on the branch's own event log as
//! [`Activity::PullRequest`] (an issue linked, a check run, a push, the
//! pull request opened or updated, an observation, feedback delivered), and
//! [`state`] folds it back, so a later `by pr`, a restarted watch and
//! `by show` all see the same pull request and never deliver one piece of
//! feedback twice.
//!
//! [`publish`] and [`readiness`] take no terminal and print nothing, so
//! `by watch` can bind keys to them.

use branchyard_support::time::now_ms;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use branchyard::{
    Activity, Branch, BranchInfo, BranchStatus, CheckRun, CiSummary, IssueLink,
    PullRequestActivity, PullRequestObservation, PullRequestRef, Pushed, RecordedEvent, Yard,
};
use branchyard_workspace::Git;
use serde_json::{json, Value};

use crate::args::{PrArgs, TaskArgs};
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::gh::{Gh, GhError};
use crate::json;
use crate::render::{Style, Tone};

impl From<GhError> for Failure {
    fn from(error: GhError) -> Self {
        Failure::Message(error.to_string())
    }
}

/// Bytes of a failed CI log sent back into a branch.
pub const LOG_TAIL_BYTES: usize = 6000;
/// Lines of a failed CI log sent back into a branch.
pub const LOG_TAIL_LINES: usize = 80;
/// How long `--watch` waits for a steer into a running turn to be taken.
const STEER_WAIT: Duration = Duration::from_secs(10);

// Issues.

/// The start of a prompt made from an issue: `Resolve <Tracker> issue `;
/// see [`issue_prompt`].
const ISSUE_HEADER: &str = "Resolve ";

/// An issue as `gh issue view` (or a tracker's API, `crate::trackers`)
/// gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub labels: Vec<String>,
    /// `None` for GitHub; else `linear`, `jira` or `gitlab`.
    pub tracker: Option<crate::trackers::Tracker>,
    /// The tracker's own reference (`ENG-123`), when not GitHub's `#n`.
    pub key: Option<String>,
}

impl Issue {
    pub fn link(&self) -> IssueLink {
        IssueLink {
            number: self.number,
            url: self.url.clone(),
            title: self.title.clone(),
            tracker: self.tracker.map(|t| t.id().to_owned()),
            key: self.key.clone(),
        }
    }

    fn reference(&self) -> String {
        reference(self.key.as_deref(), self.number)
    }

    fn tracker_name(&self) -> &'static str {
        self.tracker
            .map_or("GitHub", crate::trackers::Tracker::name)
    }
}

impl From<crate::trackers::Fetched> for Issue {
    fn from(f: crate::trackers::Fetched) -> Issue {
        Issue {
            number: f.number,
            title: f.title,
            body: f.body,
            url: f.url,
            labels: f.labels,
            tracker: Some(f.tracker),
            key: Some(f.key),
        }
    }
}

/// `#12`, or a tracker's key as it is.
fn reference(key: Option<&str>, number: u64) -> String {
    match key {
        Some(key) => key.to_owned(),
        None => format!("#{number}"),
    }
}

/// How an issue link reads in the log: `#42`, `ENG-123`.
pub fn link_reference(link: &IssueLink) -> String {
    reference(link.key.as_deref(), link.number)
}

/// What a branch started from, recorded once it exists: an issue, or a
/// pull request whose head it continues (`--pr`).
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Issue(IssueLink),
    PullRequest(PullRequestRef),
}

/// `--issue`'s value as `gh issue view` takes it: a URL as given, `#12` or
/// `12` as `12`.
pub fn issue_ref(text: &str) -> Result<String, Failure> {
    let text = text.trim();
    if text.starts_with("https://") || text.starts_with("http://") {
        return Ok(text.to_owned());
    }
    let number = text.strip_prefix('#').unwrap_or(text);
    match number.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n.to_string()),
        _ => Err(Failure::Message(format!(
            "--issue takes a GitHub issue URL, #N or N, or linear:KEY, jira:KEY,              gitlab:GROUP/PROJECT#N or a Linear, Jira or GitLab issue URL, not {text:?}"
        ))),
    }
}

/// Fetch an issue with `gh issue view`.
pub fn fetch_issue(gh: &Gh, reference: &str) -> Result<Issue, Failure> {
    let value: Value = gh.json(
        &[
            "issue",
            "view",
            reference,
            "--json",
            "number,title,body,url,labels",
        ],
        false,
    )?;
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| Failure::Message(format!("gh gave no number for issue {reference}")))?;
    Ok(Issue {
        number,
        title: text(&value["title"]),
        body: text(&value["body"]),
        url: text(&value["url"]),
        labels: value["labels"]
            .as_array()
            .map(|labels| labels.iter().map(|l| text(&l["name"])).collect())
            .unwrap_or_default(),
        tracker: None,
        key: None,
    })
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// The prompt for an issue: a header naming it, its URL and labels, its
/// text, and `extra` instructions after it.
pub fn issue_prompt(issue: &Issue, extra: &str) -> String {
    let mut prompt = format!(
        "{ISSUE_HEADER}{} issue {}: {}\n{}\n",
        issue.tracker_name(),
        issue.reference(),
        issue.title,
        issue.url
    );
    if !issue.labels.is_empty() {
        prompt.push_str(&format!("Labels: {}\n", issue.labels.join(", ")));
    }
    let body = issue.body.trim();
    prompt.push('\n');
    prompt.push_str(if body.is_empty() {
        "(The issue has no description.)"
    } else {
        body
    });
    prompt.push('\n');
    if !extra.trim().is_empty() {
        prompt.push_str(&format!("\nAdditional instructions:\n{}\n", extra.trim()));
    }
    prompt
}

/// The issue a prompt from [`issue_prompt`] names: the durable fallback
/// for a branch whose `issue_linked` event was never recorded (a spawn
/// inside a harness, a run on a server).
pub fn issue_from_prompt(prompt: &str) -> Option<IssueLink> {
    let mut lines = prompt.lines();
    let rest = lines.next()?.strip_prefix(ISSUE_HEADER)?;
    let (tracker, rest) = rest.split_once(" issue ")?;
    let (reference, title) = rest.split_once(": ")?;
    let url = lines.next()?.trim();
    if !url.starts_with("http") {
        return None;
    }
    let (tracker, key, number) = match tracker {
        "GitHub" => (None, None, reference.strip_prefix('#')?.parse().ok()?),
        "Linear" | "Jira" | "GitLab" => (
            Some(tracker.to_ascii_lowercase()),
            Some(reference.to_owned()),
            reference.rsplit(['-', '#']).next()?.parse().ok()?,
        ),
        _ => return None,
    };
    Some(IssueLink {
        number,
        url: url.to_owned(),
        title: title.to_owned(),
        tracker,
        key,
    })
}

/// `issue-<n>-<slug of its title>`; a Linear or Jira issue is named by its
/// key (`eng-123-<slug>`).
pub fn issue_branch_name(issue: &Issue) -> String {
    let stem = match (issue.tracker, &issue.key) {
        (Some(crate::trackers::Tracker::Linear | crate::trackers::Tracker::Jira), Some(key)) => {
            branchyard::slug(key)
        }
        _ => format!("issue-{}", issue.number),
    };
    let slug = branchyard::slug(&issue.title);
    match slug.as_str() {
        "task" if issue.title.trim().is_empty() => stem,
        _ => format!("{stem}-{slug}"),
    }
}

/// Fetch a Linear, Jira or GitLab issue, with credentials from the
/// environment or, when configured, through the connector gateway.
fn fetch_tracked(
    reference: &crate::trackers::IssueRef,
    yard: Option<&Yard>,
) -> Result<Issue, Failure> {
    let cwd = std::env::current_dir()?;
    let env = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &env)
        .map_err(Failure::Message)?
        .unwrap_or_default();
    let gateway = match yard {
        Some(yard) => crate::gateway_cmd::configured(yard)?,
        None => None,
    };
    let call = |tool: &str, arguments: Value| -> Result<Value, String> {
        let gateway = gateway.as_ref().ok_or("no connector gateway")?;
        let connector = tool
            .split_once("__")
            .map_or(reference.tracker.id(), |(c, _)| c);
        let grant = branchyard::connectors::GrantEntry::parse(&format!("{connector}:read"))
            .map_err(|e: String| e)?;
        let token = gateway
            .person_token_granted(vec![grant], Duration::from_secs(300))
            .map_err(|e| e.to_string())?;
        crate::trackers::gateway_call(&gateway.url, &token, tool, arguments)
    };
    let sources = crate::trackers::Sources {
        env: &env,
        config: &config.trackers,
        gateway: gateway
            .is_some()
            .then_some(&call as &dyn Fn(&str, Value) -> Result<Value, String>),
    };
    crate::trackers::fetch(reference, &sources)
        .map(Issue::from)
        .map_err(Failure::Message)
}

/// For `run`, `fan` and `spawn` with `--issue` or `--pr`: fetch the issue
/// (or the pull request) and return the prompt and task options to use,
/// with the branch named after it unless `--name` was given, and what to
/// link once the branch exists. Without either, the prompt and options
/// unchanged.
pub fn issue_task(
    prompt: &str,
    task: &TaskArgs,
    yard: Option<&Yard>,
) -> Result<(String, TaskArgs, Option<Link>), Failure> {
    if let Some(number) = task.pr {
        return pr_task(prompt, task, number, yard);
    }
    let Some(given) = &task.issue else {
        return Ok((prompt.to_owned(), task.clone(), None));
    };
    let issue = match crate::trackers::parse(given).map_err(Failure::Message)? {
        Some(tracked) => fetch_tracked(&tracked, yard)?,
        None => {
            let reference = issue_ref(given)?;
            let dir = std::env::current_dir()?;
            let gh = Gh::new(&dir, None);
            gh.ready()?;
            fetch_issue(&gh, &reference)?
        }
    };
    let mut task = task.clone();
    task.issue = None;
    if task.name.is_none() {
        let name = issue_branch_name(&issue);
        task.name = Some(match yard {
            Some(yard) => free_name(yard, &name)?,
            None => name,
        });
    }
    eprintln!(
        "by: {} issue {}: {} ({})",
        issue.tracker_name(),
        issue.reference(),
        issue.title,
        issue.url
    );
    Ok((
        issue_prompt(&issue, prompt),
        task,
        Some(Link::Issue(issue.link())),
    ))
}

/// `--pr N`: start from pull request N's head commit, fetched from
/// `origin` when it is not here yet. The prompt names the pull request;
/// when its head is a branch of this repository (not a fork), `by pr`
/// later pushes to it and updates the pull request.
fn pr_task(
    prompt: &str,
    task: &TaskArgs,
    number: u64,
    yard: Option<&Yard>,
) -> Result<(String, TaskArgs, Option<Link>), Failure> {
    let Some(yard) = yard else {
        return Err(Failure::Message(
            "--pr starts from a pull request's head commit fetched into this repository; it \
             works in local mode only, and not inside a harness"
                .into(),
        ));
    };
    if task.base.is_some() {
        return Err(Failure::Message(
            "--pr starts from the pull request's head; it takes no --base".into(),
        ));
    }
    let gh = Gh::new(yard.root(), None);
    gh.ready()?;
    let value: Value = gh.json(
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "number,title,body,url,headRefName,headRefOid,baseRefName,isCrossRepository,state",
        ],
        false,
    )?;
    let head = text(&value["headRefOid"]);
    if head.len() < 7 || !head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Failure::Message(format!(
            "gh gave no head commit for pull request #{number}"
        )));
    }
    let have = |commit: &str| {
        Git::new(yard.root())
            .args(["cat-file", "-e"])
            .arg(format!("{commit}^{{commit}}"))
            .succeeds()
            .unwrap_or(false)
    };
    if !have(&head) {
        // GitHub keeps every pull request's head as refs/pull/N/head, fork
        // or not.
        Git::new(yard.root())
            .args(["fetch", "--no-tags", "--quiet", "origin"])
            .arg(format!("refs/pull/{number}/head"))
            .run()
            .map_err(|e| {
                Failure::Message(format!(
                    "could not fetch pull request #{number}'s head from origin: {e}"
                ))
            })?;
        if !have(&head) {
            return Err(Failure::Message(format!(
                "fetched pull request #{number}, but its head {head} is not here"
            )));
        }
    }
    let title = text(&value["title"]);
    let url = text(&value["url"]);
    let head_ref = text(&value["headRefName"]);
    let cross = value["isCrossRepository"].as_bool().unwrap_or(false);
    let mut task = task.clone();
    task.pr = None;
    task.base = Some(head.clone());
    if task.name.is_none() {
        let slug = branchyard::slug(&title);
        let name = match slug.as_str() {
            "task" if title.trim().is_empty() => format!("pr-{number}"),
            _ => format!("pr-{number}-{slug}"),
        };
        task.name = Some(free_name(yard, &name)?);
    }
    let mut text_prompt = format!(
        "Continue GitHub pull request #{number}: {title}\n{url}\nIts head, {head_ref} at {}, is \
         this branch's base.\n\n",
        short(&head)
    );
    let body = text(&value["body"]);
    text_prompt.push_str(match body.trim() {
        "" => "(The pull request has no description.)",
        body => body,
    });
    text_prompt.push('\n');
    if !prompt.trim().is_empty() {
        text_prompt.push_str(&format!("\nAdditional instructions:\n{}\n", prompt.trim()));
    }
    eprintln!(
        "by: pull request #{number}: {title} ({url}), from {}",
        short(&head)
    );
    let link = match cross {
        // A fork's branch is not ours to push to.
        true => {
            eprintln!(
                "by: its head is in a fork; by pr will open a new pull request rather than push \
                 to the fork"
            );
            None
        }
        false => Some(Link::PullRequest(PullRequestRef {
            number,
            url,
            head: head_ref,
            base: Some(text(&value["baseRefName"])).filter(|b| !b.is_empty()),
            draft: false,
        })),
    };
    Ok((text_prompt, task, link))
}

/// `name`, or `name-2`, `name-3`, ... if a branch has it, as automatic
/// names are made unique.
fn free_name(yard: &Yard, name: &str) -> Result<String, Failure> {
    let taken: BTreeSet<String> = yard.branches()?.into_iter().map(|b| b.name).collect();
    let exists = |candidate: &str| {
        taken.contains(candidate)
            || Git::new(yard.root())
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("refs/heads/by/{candidate}"))
                .succeeds()
                .unwrap_or(false)
    };
    if !exists(name) {
        return Ok(name.to_owned());
    }
    (2..1000)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| !exists(candidate))
        .ok_or_else(|| Failure::Message(format!("no free branch name like {name}")))
}

/// Record on `branch` what it started from.
pub fn link_issue(branch: &Branch, link: &Link) -> Result<(), Failure> {
    branch.record_pull_request(match link {
        Link::Issue(issue) => PullRequestActivity::IssueLinked(issue.clone()),
        Link::PullRequest(pr) => PullRequestActivity::Started(pr.clone()),
    })?;
    Ok(())
}

// What the log says.

/// A branch's pull-request state, folded from its events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrState {
    pub issue: Option<IssueLink>,
    /// The last check run, with when.
    pub check: Option<(u64, CheckRun)>,
    pub pushed: Option<Pushed>,
    pub pull_request: Option<PullRequestRef>,
    /// The last observation, with when.
    pub observed: Option<(u64, PullRequestObservation)>,
    /// Feedback keys delivered into the branch.
    pub delivered: BTreeSet<String>,
    /// How many times feedback was delivered.
    pub rounds: u32,
    /// Review threads the watch answered and resolved, or tried to.
    pub resolved_threads: BTreeSet<String>,
}

impl PrState {
    /// Whether anything about a pull request was recorded.
    pub fn is_empty(&self) -> bool {
        self.check.is_none() && self.pushed.is_none() && self.pull_request.is_none()
    }
}

/// Fold a branch's events (and, for the issue, its prompt).
pub fn state(info: &BranchInfo, events: &[RecordedEvent]) -> PrState {
    let mut state = PrState {
        issue: issue_from_prompt(&info.prompt),
        ..PrState::default()
    };
    for event in events {
        let Activity::PullRequest(activity) = &event.activity else {
            continue;
        };
        match activity.as_ref() {
            PullRequestActivity::IssueLinked(link) => state.issue = Some(link.clone()),
            PullRequestActivity::Started(pr) => state.pull_request = Some(pr.clone()),
            PullRequestActivity::Checked(run) => state.check = Some((event.at_ms, run.clone())),
            PullRequestActivity::Pushed(pushed) => state.pushed = Some(pushed.clone()),
            PullRequestActivity::Opened(pr) | PullRequestActivity::Updated(pr) => {
                state.pull_request = Some(pr.clone())
            }
            PullRequestActivity::Observed(observation) => {
                state.observed = Some((event.at_ms, observation.clone()))
            }
            PullRequestActivity::FeedbackDelivered { keys, .. } => {
                state.delivered.extend(keys.iter().cloned());
                state.rounds += 1;
            }
            PullRequestActivity::FeedbackUndelivered { keys, .. } => {
                for key in keys {
                    state.delivered.remove(key);
                }
                state.rounds = state.rounds.saturating_sub(1);
            }
            PullRequestActivity::WatchStopped { .. } => {}
            PullRequestActivity::ThreadsResolved { threads, .. } => {
                state
                    .resolved_threads
                    .extend(threads.iter().map(|t| t.id.clone()));
            }
        }
    }
    state
}

// Merge readiness.

/// Whether a branch's pull request can be merged, from the last recorded
/// check, push and observation; no network.
#[derive(Clone, Debug, PartialEq)]
pub struct Readiness {
    /// `ready`, `not_ready`, `merged`, `closed` or `unobserved`.
    pub verdict: &'static str,
    /// What stands in the way, one phrase each.
    pub blockers: Vec<String>,
    /// The parts, in order: check, pull request, CI, threads, mergeability,
    /// review.
    pub parts: Vec<String>,
    pub observed_at_ms: Option<u64>,
    pub json: Value,
}

/// The local check's standing against the current candidate.
fn check_state(info: &BranchInfo, state: &PrState) -> (&'static str, String) {
    let candidate = info.candidate.as_ref().map(|c| c.commit.as_str());
    match &state.check {
        None => ("not_run", "check not run".into()),
        Some((_, run)) if Some(run.commit.as_str()) != candidate => (
            "stale",
            format!("check ran on {}, not the candidate", short(&run.commit)),
        ),
        Some((_, run)) if run.passed => ("passed", "check passed".into()),
        Some((_, run)) if run.timed_out => ("failed", "check timed out".into()),
        Some(_) => ("failed", "check failed".into()),
    }
}

/// Merge readiness for `by show` and `by watch`; `None` when the branch
/// never went towards a pull request.
pub fn readiness(info: &BranchInfo, state: &PrState) -> Option<Readiness> {
    if state.is_empty() {
        return None;
    }
    let mut blockers = Vec::new();
    let mut parts = Vec::new();
    let (check, check_text) = check_state(info, state);
    if check != "passed" {
        blockers.push(check_text.clone());
    }
    parts.push(check_text);
    let observed = state.observed.as_ref().filter(|(_, o)| {
        state
            .pull_request
            .as_ref()
            .is_none_or(|pr| pr.number == o.number)
    });
    let verdict = match (&state.pull_request, observed) {
        (None, _) => {
            parts.push("no pull request".into());
            blockers.push("no pull request".into());
            "not_ready"
        }
        (Some(pr), None) => {
            parts.push(format!("PR #{} not observed yet", pr.number));
            "unobserved"
        }
        (Some(_), Some((_, o))) => {
            let draft = if o.draft { " (draft)" } else { "" };
            parts.push(format!("PR #{} {}{draft}", o.number, o.state));
            if o.draft {
                blockers.push("draft".into());
            }
            let ci = &o.ci;
            let mut ci_parts = Vec::new();
            if ci.passed > 0 {
                ci_parts.push(format!("{} passed", ci.passed));
            }
            if ci.failed > 0 {
                ci_parts.push(format!("{} failed ({})", ci.failed, ci.failing.join(", ")));
                blockers.push(format!(
                    "{} CI check{} failed",
                    ci.failed,
                    plural(ci.failed)
                ));
            }
            if ci.pending > 0 {
                ci_parts.push(format!("{} pending", ci.pending));
                blockers.push("CI pending".into());
            }
            if ci.skipped > 0 {
                ci_parts.push(format!("{} skipped", ci.skipped));
            }
            parts.push(match ci_parts.is_empty() {
                true => "no CI checks".into(),
                false => format!("CI {}", ci_parts.join(", ")),
            });
            parts.push(format!(
                "{} unresolved thread{}",
                o.unresolved_threads,
                plural(o.unresolved_threads)
            ));
            if o.unresolved_threads > 0 {
                blockers.push(format!(
                    "{} unresolved thread{}",
                    o.unresolved_threads,
                    plural(o.unresolved_threads)
                ));
            }
            match o.mergeable.as_deref() {
                Some("conflicting") => {
                    parts.push("conflicts".into());
                    blockers.push("conflicts with its base".into());
                }
                Some("mergeable") => parts.push("mergeable".into()),
                _ => parts.push("mergeability unknown".into()),
            }
            match o.merge_state.as_deref() {
                Some("behind") => blockers.push("behind its base".into()),
                Some("blocked") if o.review_decision.is_none() => {
                    blockers.push("blocked by branch protection".into())
                }
                _ => {}
            }
            match o.review_decision.as_deref() {
                Some("approved") => parts.push("approved".into()),
                Some("changes_requested") => {
                    parts.push("changes requested".into());
                    blockers.push("changes requested".into());
                }
                Some("review_required") => {
                    parts.push("review required".into());
                    blockers.push("review required".into());
                }
                _ => {}
            }
            match o.state.as_str() {
                "merged" => "merged",
                "closed" => "closed",
                _ if blockers.is_empty() => "ready",
                _ => "not_ready",
            }
        }
    };
    let (_, run) = state.check.clone().unzip();
    let json = json!({
        "verdict": verdict,
        "blockers": blockers,
        "local_check": {
            "state": check,
            "commit": run.as_ref().map(|r| r.commit.clone()),
            "argv": run.as_ref().map(|r| r.argv.clone()),
        },
        "issue": state.issue,
        "pushed": state.pushed,
        "pull_request": state.pull_request,
        "observation": observed.map(|(_, o)| o),
        "observed_at_ms": observed.map(|(at, _)| at),
        "feedback_rounds": state.rounds,
    });
    Some(Readiness {
        verdict,
        blockers,
        parts,
        observed_at_ms: observed.map(|(at, _)| *at),
        json,
    })
}

fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The `merge readiness` line of `by show`.
pub fn readiness_text(readiness: &Readiness, now_ms: u64, style: Style) -> String {
    let (word, tone) = match readiness.verdict {
        "ready" => ("ready to merge".to_owned(), Tone::Green),
        "merged" => ("merged".to_owned(), Tone::Green),
        "closed" => ("closed".to_owned(), Tone::Dim),
        "unobserved" => (
            "unknown (by show --refresh asks GitHub)".to_owned(),
            Tone::Yellow,
        ),
        _ => (
            format!("not ready: {}", readiness.blockers.join(", ")),
            Tone::Yellow,
        ),
    };
    let when = match readiness.observed_at_ms {
        Some(at) => format!(
            " (observed {} ago)",
            crate::render::age_text(now_ms.saturating_sub(at) / 1000)
        ),
        None => String::new(),
    };
    format!(
        "{} · {}{when}",
        style.paint(tone, &word),
        readiness.parts.join(" · ")
    )
}

/// Merge readiness for `by show`, as its JSON value (`null` when there is
/// none) and its text line.
pub fn show_readiness(
    info: &BranchInfo,
    events: &[RecordedEvent],
    style: Style,
) -> (Value, Option<String>) {
    let state = state(info, events);
    match readiness(info, &state) {
        None => (Value::Null, None),
        Some(r) => {
            let line = readiness_text(&r, now_ms(), style);
            (r.json, Some(line))
        }
    }
}

// The log line.

/// `by log`'s text for a pull-request activity, and its tone.
pub fn log_line(activity: &PullRequestActivity) -> (String, Tone) {
    match activity {
        PullRequestActivity::IssueLinked(link) => (
            format!(
                "issue {} linked: {} ({})",
                link_reference(link),
                link.title,
                link.url
            ),
            Tone::Cyan,
        ),
        PullRequestActivity::Started(pr) => (
            format!(
                "started from pull request #{}'s head ({}): {}",
                pr.number, pr.head, pr.url
            ),
            Tone::Cyan,
        ),
        PullRequestActivity::Checked(run) => {
            let outcome = match (run.passed, run.timed_out) {
                (true, _) => "passed",
                (false, true) => "timed out",
                (false, false) => "failed",
            };
            (
                format!(
                    "check `{}` {outcome} on {}",
                    run.argv.join(" "),
                    short(&run.commit)
                ),
                if run.passed { Tone::Green } else { Tone::Red },
            )
        }
        PullRequestActivity::Pushed(p) => (
            format!(
                "pushed {} to {} {}{}",
                short(&p.commit),
                p.remote,
                p.remote_branch,
                if p.forced { " (forced)" } else { "" }
            ),
            Tone::Cyan,
        ),
        PullRequestActivity::Opened(pr) => (
            format!(
                "pull request #{} opened{}: {}",
                pr.number,
                if pr.draft { " as a draft" } else { "" },
                pr.url
            ),
            Tone::Green,
        ),
        PullRequestActivity::Updated(pr) => (
            format!("pull request #{} updated: {}", pr.number, pr.url),
            Tone::Cyan,
        ),
        PullRequestActivity::Observed(o) => (
            format!("pull request #{} {}", o.number, observation_text(o)),
            Tone::Dim,
        ),
        PullRequestActivity::FeedbackDelivered { keys, via, summary } => (
            format!(
                "pull request feedback sent into the branch ({via}), {} piece{}: {}",
                keys.len(),
                plural(keys.len() as u32),
                summary.join("; ")
            ),
            Tone::Bold,
        ),
        PullRequestActivity::FeedbackUndelivered { keys, reason } => (
            format!(
                "pull request feedback not delivered ({} piece{}): {reason}",
                keys.len(),
                plural(keys.len() as u32)
            ),
            Tone::Yellow,
        ),
        PullRequestActivity::WatchStopped { reason } => (
            format!("stopped watching the pull request: {reason}"),
            Tone::Dim,
        ),
        PullRequestActivity::ThreadsResolved { commit, threads } => {
            let done: Vec<&str> = threads
                .iter()
                .filter(|t| t.resolved)
                .map(|t| t.path.as_str())
                .collect();
            let failed = threads.len() - done.len();
            let mut text = format!(
                "{} review thread{} addressed in {} resolved",
                done.len(),
                plural(done.len() as u32),
                short(commit)
            );
            if !done.is_empty() {
                text.push_str(&format!(" ({})", done.join(", ")));
            }
            if failed > 0 {
                text.push_str(&format!(", {failed} not"));
            }
            (
                text,
                if failed > 0 {
                    Tone::Yellow
                } else {
                    Tone::Green
                },
            )
        }
    }
}

/// `open, CI 2 passed, 1 failed (test); 1 unresolved thread; mergeable`.
pub fn observation_text(o: &PullRequestObservation) -> String {
    let ci = &o.ci;
    let mut ci_parts = vec![format!("{} passed", ci.passed)];
    if ci.failed > 0 {
        ci_parts.push(format!("{} failed ({})", ci.failed, ci.failing.join(", ")));
    }
    if ci.pending > 0 {
        ci_parts.push(format!("{} pending", ci.pending));
    }
    let mut text = format!(
        "{}{}, CI {}; {} unresolved thread{}",
        o.state,
        if o.draft { " (draft)" } else { "" },
        ci_parts.join(", "),
        o.unresolved_threads,
        plural(o.unresolved_threads)
    );
    if let Some(m) = &o.mergeable {
        text.push_str(&format!("; {m}"));
    }
    if let Some(r) = &o.review_decision {
        text.push_str(&format!("; review {}", r.replace('_', " ")));
    }
    text
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

// Publishing: check, push, open or update.

/// What [`publish`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct Published {
    pub check: Option<CheckRun>,
    pub pushed: Pushed,
    pub pull_request: PullRequestRef,
    pub created: bool,
}

/// Why [`publish`] stopped.
#[derive(Debug)]
pub enum PublishError {
    /// The branch's check failed on its candidate (recorded); nothing was
    /// pushed.
    CheckFailed(CheckRun),
    Other(Failure),
}

impl From<Failure> for PublishError {
    fn from(error: Failure) -> Self {
        PublishError::Other(error)
    }
}

impl From<branchyard::Error> for PublishError {
    fn from(error: branchyard::Error) -> Self {
        PublishError::Other(error.into())
    }
}

impl From<GhError> for PublishError {
    fn from(error: GhError) -> Self {
        PublishError::Other(error.into())
    }
}

impl From<PublishError> for Failure {
    fn from(error: PublishError) -> Self {
        match error {
            PublishError::CheckFailed(run) => Failure::Message(format!(
                "the check `{}` failed on {}'s candidate {}, so nothing was pushed; pass \
                 --allow-failing-check to push anyway. Its output ended:\n{}",
                run.argv.join(" "),
                "the branch",
                short(&run.commit),
                tail_lines(&run.output_tail, 20)
            )),
            PublishError::Other(failure) => failure,
        }
    }
}

/// Refuse a branch that should not become a pull request yet.
fn publishable(info: &BranchInfo, allow_not_ready: bool) -> Result<(), Failure> {
    let refuse = |why: String| Err(Failure::Message(why));
    match &info.status {
        BranchStatus::Ready => Ok(()),
        BranchStatus::Running => refuse(format!(
            "{} is running; wait for its turn to end (by log --follow {})",
            info.name, info.name
        )),
        BranchStatus::Merged { target, .. } => refuse(format!(
            "{} was merged into {target} locally; push {target} instead",
            info.name
        )),
        _ if info.candidate.is_none() => refuse(format!("{} has no candidate to push", info.name)),
        _ if allow_not_ready => Ok(()),
        status => refuse(format!(
            "{} is {}, not ready; pass --allow-not-ready to push its candidate anyway",
            info.name,
            crate::render::status_text(status).0
        )),
    }
}

/// `by pr` without `--watch`: check the candidate, push it, and open the
/// branch's pull request or update the open one, recording each step.
/// Prints nothing; `gh` must be ready (see [`Gh::ready`]).
pub fn publish(branch: &Branch, gh: &Gh, args: &PrArgs) -> Result<Published, PublishError> {
    let info = branch.info().clone();
    publishable(&info, args.allow_not_ready)?;
    let before = state(&info, &branch.events()?);
    let candidate = info
        .candidate
        .clone()
        .ok_or_else(|| Failure::Message(format!("{} has no candidate", info.name)))?;
    let check = match (args.no_check, &before.check) {
        (true, _) => None,
        (false, Some((_, run))) if run.commit == candidate.commit => Some(run.clone()),
        (false, _) => {
            let run = branch.verify_candidate()?;
            if let Some(run) = &run {
                branch.record_pull_request(PullRequestActivity::Checked(run.clone()))?;
            }
            run
        }
    };
    if let Some(run) = &check {
        if !run.passed && !args.allow_failing_check {
            return Err(PublishError::CheckFailed(run.clone()));
        }
    }
    let head = args
        .head
        .clone()
        .or_else(|| before.pull_request.as_ref().map(|pr| pr.head.clone()))
        .unwrap_or_else(|| info.git_branch.clone());
    let pushed = branch.push_candidate(&args.git_remote, &head, args.force)?;
    branch.record_pull_request(PullRequestActivity::Pushed(pushed.clone()))?;

    let existing: Value = gh.json(
        &[
            "pr",
            "list",
            "--head",
            &head,
            "--state",
            "open",
            "--json",
            "number,url,isDraft,baseRefName",
            "--limit",
            "1",
        ],
        false,
    )?;
    let existing = existing.as_array().and_then(|list| list.first()).cloned();
    let diffstat = diffstat(branch.yard().root(), &info.base, &candidate.commit);
    let mut after = before.clone();
    after.check = check.clone().map(|run| (0, run));
    after.pushed = Some(pushed.clone());
    let (pull_request, created) = match existing {
        Some(pr) => {
            let number = pr["number"].as_u64().unwrap_or_default();
            let url = text(&pr["url"]);
            let body = body(&info, &after, &diffstat, repo_of(&url));
            let number_text = number.to_string();
            let mut argv = vec!["pr", "edit", &number_text, "--body-file", "-"];
            if let Some(title) = &args.title {
                argv.extend(["--title", title]);
            }
            if let Some(base) = &args.base {
                argv.extend(["--base", base]);
            }
            gh.run(&argv, Some(&body))?;
            let pr = PullRequestRef {
                number,
                url,
                head: head.clone(),
                base: args
                    .base
                    .clone()
                    .or_else(|| Some(text(&pr["baseRefName"])).filter(|b| !b.is_empty())),
                draft: pr["isDraft"].as_bool().unwrap_or(false),
            };
            branch.record_pull_request(PullRequestActivity::Updated(pr.clone()))?;
            (pr, false)
        }
        None => {
            let title = args.title.clone().unwrap_or_else(|| title(&info, &after));
            let body_text = body(&info, &after, &diffstat, None);
            let mut argv = vec![
                "pr",
                "create",
                "--head",
                &head,
                "--title",
                &title,
                "--body-file",
                "-",
            ];
            if let Some(base) = &args.base {
                argv.extend(["--base", base]);
            }
            if args.draft {
                argv.push("--draft");
            }
            let out = gh.run(&argv, Some(&body_text))?;
            let url = out
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| line.starts_with("http"))
                .ok_or_else(|| {
                    Failure::Message(format!("gh pr create printed no URL: {}", out.trim()))
                })?
                .to_owned();
            let number = pr_number(&url).ok_or_else(|| {
                Failure::Message(format!("cannot read a pull request number from {url}"))
            })?;
            // The issue may live in another repository than the pull
            // request; then it is closed by its full name.
            let fixed = body(&info, &after, &diffstat, repo_of(&url));
            if fixed != body_text {
                gh.run(
                    &["pr", "edit", &number.to_string(), "--body-file", "-"],
                    Some(&fixed),
                )?;
            }
            let pr = PullRequestRef {
                number,
                url,
                head: head.clone(),
                base: args.base.clone(),
                draft: args.draft,
            };
            branch.record_pull_request(PullRequestActivity::Opened(pr.clone()))?;
            (pr, true)
        }
    };
    Ok(Published {
        check,
        pushed,
        pull_request,
        created,
    })
}

/// `https://HOST/OWNER/REPO/pull/N` → N.
fn pr_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// `https://HOST/OWNER/REPO/...` → (HOST, OWNER, REPO).
fn repo_of(url: &str) -> Option<(String, String, String)> {
    let rest = url.split_once("://")?.1;
    let mut parts = rest.split('/');
    let host = parts.next()?.to_owned();
    let owner = parts.next()?.to_owned();
    let repo = parts.next()?.to_owned();
    (!owner.is_empty() && !repo.is_empty()).then_some((host, owner, repo))
}

/// `git diff --stat` of the candidate against the base, at most 60 lines.
fn diffstat(root: &Path, base: &str, commit: &str) -> String {
    match Git::new(root)
        .args(["diff", "--stat=100", "--no-color", base, commit, "--"])
        .run()
    {
        Ok(out) => tail_lines(&out, 60).to_owned(),
        Err(_) => String::new(),
    }
}

/// The line in a pull request's body that names its issue: a closing
/// keyword where the tracker acts on one (GitHub; Linear's GitHub
/// integration closes `ENG-123` on merge), a reference otherwise (Jira
/// links a key it sees but closes nothing; a GitHub pull request cannot
/// close a GitLab issue).
pub fn closing_line(issue: &IssueLink, same_repo: bool) -> String {
    let key = link_reference(issue);
    match issue.tracker.as_deref() {
        Some("linear") => format!("Closes {key}\n"),
        Some("jira") => format!("Refs {key} ({})\n", issue.url),
        Some(_) => format!("Related: {key} ({})\n", issue.url),
        None => match (same_repo, repo_of(&issue.url)) {
            (false, Some((_, owner, repo))) => format!("Closes {owner}/{repo}#{}\n", issue.number),
            _ => format!("Closes #{}\n", issue.number),
        },
    }
}

/// The default title: the issue's, or the prompt's first line.
fn title(info: &BranchInfo, state: &PrState) -> String {
    if let Some(issue) = &state.issue {
        return issue.title.clone();
    }
    let line = info
        .prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(&info.name);
    truncate(line, 72)
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
    }
}

/// The last `n` lines of `text`.
fn tail_lines(text: &str, n: usize) -> &str {
    let text = text.trim_end();
    let mut at = text.len();
    for _ in 0..n {
        match text[..at].rfind('\n') {
            Some(i) => at = i,
            None => return text,
        }
    }
    &text[at + 1..]
}

/// The pull request's body, regenerated on each push.
pub fn body(
    info: &BranchInfo,
    state: &PrState,
    diffstat: &str,
    pr_repo: Option<(String, String, String)>,
) -> String {
    let mut body = String::from("## Task\n\n");
    let prompt: Vec<&str> = info.prompt.trim().lines().collect();
    for line in prompt.iter().take(40) {
        body.push_str(&format!("> {line}\n").replace("> \n", ">\n"));
    }
    if prompt.len() > 40 {
        body.push_str(&format!("> … ({} more lines)\n", prompt.len() - 40));
    }
    if let Some(issue) = &state.issue {
        let same_repo = match (repo_of(&issue.url), &pr_repo) {
            (Some((h1, o1, r1)), Some((h2, o2, r2))) => h1 == *h2 && o1 == *o2 && r1 == *r2,
            _ => true,
        };
        body.push('\n');
        body.push_str(&closing_line(issue, same_repo));
    }
    body.push_str("\n## Branchyard\n\n| | |\n|---|---|\n");
    body.push_str(&format!(
        "| Branch | `{}` on {} |\n",
        info.git_branch, info.harness
    ));
    body.push_str(&format!("| Turns | {} |\n", info.turns));
    body.push_str(&format!(
        "| Cost | {} |\n",
        match info.cost_usd {
            Some(usd) => format!("${usd:.2}, the harness's estimate"),
            None => "not reported by the harness".into(),
        }
    ));
    let check = match &state.check {
        None => "not run".to_owned(),
        Some((_, run)) => format!(
            "`{}` {} on `{}`",
            run.argv.join(" "),
            match (run.passed, run.timed_out) {
                (true, _) => "passed",
                (false, true) => "timed out",
                (false, false) => "**failed**",
            },
            short(&run.commit)
        ),
    };
    body.push_str(&format!("| Check | {check} |\n"));
    if let Some(c) = &info.candidate {
        body.push_str(&format!(
            "| Candidate | `{}`: {} file{} changed, +{} −{} |\n",
            short(&c.commit),
            c.files_changed,
            plural(c.files_changed),
            c.insertions,
            c.deletions
        ));
    }
    if !diffstat.trim().is_empty() {
        body.push_str(&format!(
            "\n<details><summary>Files changed</summary>\n\n```\n{}\n```\n\n</details>\n",
            diffstat.trim_end()
        ));
    }
    body.push_str(
        "\n<sub>Opened by Branchyard's `by pr`. This body is regenerated on each push; \
         edits to it are replaced.</sub>\n",
    );
    body
}

// Observing a pull request and its feedback.

/// One piece of feedback to send into the branch, delivered once by `key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    pub key: String,
    /// One line, for the log.
    pub summary: String,
    /// The text given to the harness.
    pub text: String,
    /// For a failed CI check, its link, whose run's failed log
    /// [`failed_log`] adds to `text` before delivery.
    pub log: Option<String>,
}

const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
     repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
     reviewThreads(first: 100) { nodes { id isResolved path line comments(first: 50) { \
     nodes { id author { login } body path line url } } } } } } }";

/// A review thread as the watch last observed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    /// Its GraphQL node ID; empty when `gh` gave none.
    pub id: String,
    pub path: Option<String>,
    pub resolved: bool,
    /// The feedback keys of its comments with a body (`thread:<comment>`).
    pub keys: Vec<String>,
}

/// What [`observe`] saw.
pub struct Observed {
    pub observation: PullRequestObservation,
    /// Feedback among it, without the log of failed runs (see
    /// [`failed_log`]).
    pub feedback: Vec<Feedback>,
    pub threads: Vec<ReviewThread>,
}

/// Ask `gh` about `pr`: its state, checks, reviews, comments and review
/// threads, and the feedback among them.
pub fn observe(gh: &Gh, pr: &PullRequestRef) -> Result<Observed, Failure> {
    let number = pr.number.to_string();
    let view: Value = gh.json(
        &[
            "pr",
            "view",
            &number,
            "--json",
            "number,url,state,isDraft,headRefOid,reviewDecision,mergeable,mergeStateStatus,\
             reviews,comments",
        ],
        false,
    )?;
    let checks: Value = match gh.json(
        &[
            "pr",
            "checks",
            &number,
            "--json",
            "name,state,bucket,link,workflow",
        ],
        true,
    ) {
        Ok(checks) => checks,
        // `gh pr checks` fails when a pull request has no checks at all.
        Err(GhError::Failed { stderr, .. }) if stderr.contains("no checks") => json!([]),
        Err(error) => return Err(error.into()),
    };
    let threads = match repo_of(&pr.url) {
        Some((host, owner, name)) => {
            let owner_arg = format!("owner={owner}");
            let name_arg = format!("name={name}");
            let number_arg = format!("number={}", pr.number);
            let query_arg = format!("query={THREADS_QUERY}");
            let mut argv = vec!["api", "graphql"];
            if host != "github.com" {
                argv.extend(["--hostname", &host]);
            }
            argv.extend([
                "-F",
                &owner_arg,
                "-F",
                &name_arg,
                "-F",
                &number_arg,
                "-f",
                &query_arg,
            ]);
            let value: Value = gh.json(&argv, false)?;
            value["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    let lower = |v: &Value| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
    };
    let head_commit = view["headRefOid"].as_str().map(str::to_owned);
    let mut ci = CiSummary::default();
    let mut feedback = Vec::new();
    for check in checks.as_array().into_iter().flatten() {
        let name = text(&check["name"]);
        match check["bucket"].as_str().unwrap_or_default() {
            "pass" => ci.passed += 1,
            "fail" | "cancel" => {
                ci.failed += 1;
                ci.failing.push(name.clone());
                if check["bucket"] == "fail" {
                    let workflow = text(&check["workflow"]);
                    let on = head_commit.as_deref().map_or("its head", short);
                    let key = format!("ci:{name}:{}", head_commit.as_deref().unwrap_or("unknown"));
                    let link = text(&check["link"]);
                    let about = match workflow.is_empty() {
                        true => String::new(),
                        false => format!(" (workflow {workflow})"),
                    };
                    feedback.push(Feedback {
                        key,
                        summary: format!("CI check {name} failed on {on}"),
                        text: format!("CI check \"{name}\"{about} failed on {on}: {link}"),
                        log: Some(link),
                    });
                }
            }
            "pending" => ci.pending += 1,
            "skipping" => ci.skipped += 1,
            _ => ci.pending += 1,
        }
    }
    for review in view["reviews"].as_array().into_iter().flatten() {
        let body = text(&review["body"]);
        let state = text(&review["state"]);
        if body.trim().is_empty() || !matches!(state.as_str(), "CHANGES_REQUESTED" | "COMMENTED") {
            continue;
        }
        let author = text(&review["author"]["login"]);
        let what = match state.as_str() {
            "CHANGES_REQUESTED" => "requested changes",
            _ => "reviewed",
        };
        feedback.push(Feedback {
            key: format!("review:{}", text(&review["id"])),
            summary: format!("review by @{author}"),
            text: format!("@{author} {what}:\n{}", quote(&body)),
            log: None,
        });
    }
    for comment in view["comments"].as_array().into_iter().flatten() {
        let body = text(&comment["body"]);
        if body.trim().is_empty() {
            continue;
        }
        let author = text(&comment["author"]["login"]);
        feedback.push(Feedback {
            key: format!("comment:{}", text(&comment["id"])),
            summary: format!("comment by @{author}"),
            text: format!("@{author} commented:\n{}", quote(&body)),
            log: None,
        });
    }
    let mut unresolved = 0;
    let mut review_threads = Vec::new();
    for thread in &threads {
        let resolved = thread["isResolved"].as_bool().unwrap_or(false);
        let keys: Vec<String> = thread["comments"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| !text(&c["body"]).trim().is_empty())
            .map(|c| format!("thread:{}", text(&c["id"])))
            .collect();
        review_threads.push(ReviewThread {
            id: text(&thread["id"]),
            path: thread["path"].as_str().map(str::to_owned),
            resolved,
            keys,
        });
        if resolved {
            continue;
        }
        unresolved += 1;
        for comment in thread["comments"]["nodes"].as_array().into_iter().flatten() {
            let body = text(&comment["body"]);
            if body.trim().is_empty() {
                continue;
            }
            let author = text(&comment["author"]["login"]);
            let place = match (comment["path"].as_str(), comment["line"].as_u64()) {
                (Some(path), Some(line)) => format!(" on {path}:{line}"),
                (Some(path), None) => format!(" on {path}"),
                _ => String::new(),
            };
            feedback.push(Feedback {
                key: format!("thread:{}", text(&comment["id"])),
                summary: format!("review comment by @{author}{place}"),
                text: format!("Review comment by @{author}{place}:\n{}", quote(&body)),
                log: None,
            });
        }
    }
    let observation = PullRequestObservation {
        number: view["number"].as_u64().unwrap_or(pr.number),
        url: Some(text(&view["url"]))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| pr.url.clone()),
        state: lower(&view["state"]).unwrap_or_else(|| "unknown".into()),
        draft: view["isDraft"].as_bool().unwrap_or(false),
        head_commit,
        review_decision: lower(&view["reviewDecision"]),
        mergeable: lower(&view["mergeable"]),
        merge_state: lower(&view["mergeStateStatus"]),
        ci,
        unresolved_threads: unresolved,
    };
    Ok(Observed {
        observation,
        feedback,
        threads: review_threads,
    })
}

fn quote(body: &str) -> String {
    body.trim()
        .lines()
        .map(|line| format!("> {line}").trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The failed steps' log of the GitHub Actions run a check links to,
/// bounded to its last [`LOG_TAIL_LINES`] lines and [`LOG_TAIL_BYTES`]
/// bytes; `None` for a check that is not an Actions run.
pub fn failed_log(gh: &Gh, link: &str) -> Option<String> {
    let run = link
        .split("/actions/runs/")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|id| !id.is_empty())?;
    let log = gh.run(&["run", "view", run, "--log-failed"], None).ok()?;
    let mut tail = tail_lines(&log, LOG_TAIL_LINES).to_owned();
    if tail.len() > LOG_TAIL_BYTES {
        let mut cut = tail.len() - LOG_TAIL_BYTES;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = tail[cut..].to_owned();
    }
    Some(tail)
}

/// The message given to the harness for `feedback` on `pr`.
pub fn feedback_prompt(pr: &PullRequestRef, feedback: &[Feedback]) -> String {
    let mut text = format!(
        "Feedback on pull request #{} ({}) for this branch:\n",
        pr.number, pr.url
    );
    for (n, item) in feedback.iter().enumerate() {
        text.push_str(&format!("\n{}. {}\n", n + 1, item.text.trim_end()));
    }
    text.push_str(
        "\nAddress it on this branch. When this turn ends, the branch is pushed to the pull \
         request again.\n",
    );
    text
}

// Commands.

/// `by pr`.
pub fn main(env: &Env, target: &Target, branch: &str, args: &PrArgs) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Sdk(branchyard::Error::Unsupported(
            "by pr works in local mode only: it pushes from this repository with your git \
             remote and gh login, and a server's branches are in the server's repository. \
             Run by pr on the server's host"
                .into(),
        )));
    }
    if std::env::var_os(branchyard::ENV_BRANCH).is_some_and(|v| !v.is_empty()) {
        return Err(Failure::Sdk(branchyard::Error::Denied(
            "by pr pushes with your git and GitHub credentials, so it does not run inside a \
             harness; the person or meta-harness that started the branch runs it"
                .into(),
        )));
    }
    let yard = commands::open()?;
    let branch = yard.branch(branch)?;
    let gh = Gh::new(yard.root(), args.gh_repo.as_deref());
    // A running branch with a pull request is watched as it is; what its
    // turn makes is pushed when it ends.
    let known = state(branch.info(), &branch.events()?).pull_request;
    if args.watch && branch.info().status == BranchStatus::Running && known.is_some() {
        gh.ready()?;
        return watch(env, target, &yard, &branch.info().name, &gh, args);
    }
    publishable(branch.info(), args.allow_not_ready)?;
    gh.ready()?;
    let published = publish(&branch, &gh, args)?;
    if args.json {
        return print(&json::text(&json!({
            "branch": branch.info().name,
            "created": published.created,
            "pull_request": published.pull_request,
            "pushed": published.pushed,
            "check": published.check,
        })));
    }
    print(&published_text(&published))?;
    if args.watch {
        return watch(env, target, &yard, &branch.info().name, &gh, args);
    }
    Ok(())
}

fn published_text(p: &Published) -> String {
    let mut text = String::new();
    if let Some(run) = &p.check {
        text.push_str(&format!(
            "check `{}` {} on {}\n",
            run.argv.join(" "),
            if run.passed { "passed" } else { "failed" },
            short(&run.commit)
        ));
    }
    text.push_str(&format!(
        "pushed {} to {} {}\n",
        short(&p.pushed.commit),
        p.pushed.remote,
        p.pushed.remote_branch
    ));
    text.push_str(&format!(
        "{} pull request #{}: {}\n",
        if p.created { "opened" } else { "updated" },
        p.pull_request.number,
        p.pull_request.url
    ));
    text
}

/// `by show --refresh`: observe the branch's pull request now and record
/// what changed.
pub fn refresh(yard: &Yard, name: &str) -> Outcome {
    let branch = yard.branch(name)?;
    let state = state(branch.info(), &branch.events()?);
    let Some(pr) = &state.pull_request else {
        return Err(Failure::Message(format!(
            "{name} has no pull request to refresh; open one with by pr {name}"
        )));
    };
    let gh = Gh::new(yard.root(), None);
    gh.ready()?;
    let observed = observe(&gh, pr)?;
    record_observation(&branch, &state, observed.observation)?;
    Ok(())
}

/// Record `observation` unless it is what was last observed; whether it
/// was new.
fn record_observation(
    branch: &Branch,
    state: &PrState,
    observation: PullRequestObservation,
) -> Result<bool, Failure> {
    if state.observed.as_ref().map(|(_, o)| o) == Some(&observation) {
        return Ok(false);
    }
    branch.record_pull_request(PullRequestActivity::Observed(observation))?;
    Ok(true)
}

/// `by pr --watch`: follow the pull request until it is merged or closed.
/// Each poll first pushes a candidate not pushed yet (after a turn), then
/// observes the pull request; new feedback (a failed CI check with its
/// log, a review, a comment, an unresolved review comment, or the local
/// check failing on a new candidate) is sent into the branch once, by
/// steering its running turn or as a new turn. Polls back off from
/// `--interval` to ten times it while nothing changes.
fn watch(env: &Env, target: &Target, yard: &Yard, name: &str, gh: &Gh, args: &PrArgs) -> Outcome {
    let quiet_max = args.interval * 10;
    let mut interval = args.interval;
    let mut rounds = 0u32;
    print(&format!(
        "watching the pull request; polling every {:?}, backing off to {:?}\n",
        args.interval, quiet_max
    ))?;
    let stop = |branch: &Branch, reason: &str| -> Outcome {
        branch.record_pull_request(PullRequestActivity::WatchStopped {
            reason: reason.to_owned(),
        })?;
        print(&format!("stopped watching: {reason}\n"))
    };
    loop {
        let branch = yard.branch(name)?;
        let state = state(branch.info(), &branch.events()?);
        let mut feedback = Vec::new();
        let mut active = false;
        let mut pushed = None;
        match push_if_new(&branch, gh, args, &state)? {
            PushOutcome::Pushed { from, to } => {
                active = true;
                pushed = from.map(|from| (from, to));
            }
            PushOutcome::CheckFailed(item) => feedback.push(item),
            PushOutcome::Nothing => {}
        }
        let branch = yard.branch(name)?;
        let state = self::state(branch.info(), &branch.events()?);
        let pr = state
            .pull_request
            .clone()
            .ok_or_else(|| Failure::Message(format!("{name} has no pull request")))?;
        let Observed {
            observation,
            feedback: observed,
            threads,
        } = observe(gh, &pr)?;
        if let (Some((from, to)), false) = (&pushed, args.no_resolve) {
            resolve_addressed(yard, &branch, gh, &pr, &state, &threads, (from, to))?;
        }
        if record_observation(&branch, &state, observation.clone())? {
            active = true;
            print(&format!(
                "pull request #{} {}\n",
                pr.number,
                observation_text(&observation)
            ))?;
        }
        match observation.state.as_str() {
            "merged" => return stop(&branch, "the pull request was merged"),
            "closed" => return stop(&branch, "the pull request was closed"),
            _ => {}
        }
        feedback.extend(observed);
        feedback.retain(|f| !state.delivered.contains(&f.key));
        if !feedback.is_empty() {
            if args.max_rounds.is_some_and(|max| rounds >= max) {
                return stop(
                    &branch,
                    &format!("--max-rounds {rounds} reached with feedback left"),
                );
            }
            let mut runs_read = Vec::new();
            for item in feedback.iter_mut() {
                let Some(link) = item.log.take() else {
                    continue;
                };
                // Several failed jobs of one run share its log.
                if runs_read.contains(&link) {
                    continue;
                }
                if let Some(log) = failed_log(gh, &link) {
                    item.text
                        .push_str(&format!("\nThe failed steps' log ends:\n```\n{log}\n```"));
                }
                runs_read.push(link);
            }
            deliver(env, target, yard, name, &pr, &feedback, &args.task)?;
            rounds += 1;
            // The next pass pushes what the turn made.
            interval = args.interval;
            continue;
        }
        if args.max_rounds.is_some_and(|max| rounds >= max) {
            return stop(&branch, &format!("--max-rounds {rounds} reached"));
        }
        if active {
            interval = args.interval;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(quiet_max);
    }
}

/// What [`push_if_new`] did.
enum PushOutcome {
    Nothing,
    /// The candidate `to` was pushed over `from`, the commit pushed
    /// before (none on a first push).
    Pushed {
        from: Option<String>,
        to: String,
    },
    /// The check failed on the new candidate: this feedback says so.
    CheckFailed(Feedback),
}

/// Push and update the pull request when the branch settled on a
/// candidate that was not pushed yet.
fn push_if_new(
    branch: &Branch,
    gh: &Gh,
    args: &PrArgs,
    state: &PrState,
) -> Result<PushOutcome, Failure> {
    let info = branch.info();
    let Some(candidate) = &info.candidate else {
        return Ok(PushOutcome::Nothing);
    };
    if info.status != BranchStatus::Ready
        || state.pushed.as_ref().map(|p| p.commit.as_str()) == Some(candidate.commit.as_str())
    {
        return Ok(PushOutcome::Nothing);
    }
    let mut update = args.clone();
    update.title = None;
    update.draft = false;
    match publish(branch, gh, &update) {
        Ok(published) => {
            print(&published_text(&published))?;
            Ok(PushOutcome::Pushed {
                from: state.pushed.as_ref().map(|p| p.commit.clone()),
                to: published.pushed.commit,
            })
        }
        Err(PublishError::CheckFailed(run)) => {
            print(&format!(
                "check `{}` failed on {}; not pushed\n",
                run.argv.join(" "),
                short(&run.commit)
            ))?;
            Ok(PushOutcome::CheckFailed(Feedback {
                key: format!("check:{}", run.commit),
                summary: format!("local check failed on {}", short(&run.commit)),
                text: format!(
                    "The branch's check `{}` failed on its candidate {}, so it was not \
                     pushed. Its output ended:\n```\n{}\n```",
                    run.argv.join(" "),
                    short(&run.commit),
                    tail_lines(&run.output_tail, 40)
                ),
                log: None,
            }))
        }
        Err(PublishError::Other(failure)) => Err(failure),
    }
}

/// The review threads to answer and resolve after `to` was pushed over
/// `from`: unresolved ones, not tried before, every comment of which was
/// delivered as feedback, on a file the push changed.
pub fn addressed_threads<'a>(
    state: &PrState,
    threads: &'a [ReviewThread],
    changed: &BTreeSet<String>,
) -> Vec<&'a ReviewThread> {
    threads
        .iter()
        .filter(|t| !t.resolved && !t.id.is_empty() && !t.keys.is_empty())
        .filter(|t| !state.resolved_threads.contains(&t.id))
        .filter(|t| t.keys.iter().all(|k| state.delivered.contains(k)))
        .filter(|t| t.path.as_ref().is_some_and(|p| changed.contains(p)))
        .collect()
}

/// After a push, answer "Addressed in <commit>" in each review thread the
/// watch fed back whose file the pushed commits changed, resolve it, and
/// record what happened, so no thread is tried twice. A failure is
/// recorded and said, and the watch carries on.
fn resolve_addressed(
    yard: &Yard,
    branch: &Branch,
    gh: &Gh,
    pr: &PullRequestRef,
    state: &PrState,
    threads: &[ReviewThread],
    (from, to): (&str, &str),
) -> Outcome {
    let changed: BTreeSet<String> = Git::new(yard.root())
        .args(["diff", "--name-only", "--no-renames", from, to, "--"])
        .run()
        .map(|out| out.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let threads = addressed_threads(state, threads, &changed);
    if threads.is_empty() {
        return Ok(());
    }
    let host = repo_of(&pr.url)
        .map(|(host, _, _)| host)
        .filter(|host| host != "github.com");
    let reply = format!("Addressed in {to}.");
    let mut results = Vec::new();
    for thread in threads {
        let path = thread.path.clone().unwrap_or_default();
        let replied =
            crate::pr_threads::reply_to_review_thread(gh, host.as_deref(), &thread.id, &reply);
        let resolved = replied.as_ref().map_err(|e| e.to_string()).and_then(|()| {
            crate::pr_threads::resolve_review_thread(gh, host.as_deref(), &thread.id, true)
                .map_err(|e| e.to_string())
        });
        let result = branchyard::ResolvedThread {
            id: thread.id.clone(),
            path: path.clone(),
            replied: replied.is_ok(),
            resolved: resolved == Ok(true),
            error: match resolved {
                Ok(true) => None,
                Ok(false) => Some("GitHub did not report the thread resolved".into()),
                Err(error) => Some(error),
            },
        };
        match &result.error {
            None => print(&format!(
                "resolved the review thread on {path}, addressed in {}
",
                short(to)
            ))?,
            Some(error) => eprintln!("by: could not resolve the review thread on {path}: {error}"),
        }
        results.push(result);
    }
    branch.record_pull_request(PullRequestActivity::ThreadsResolved {
        commit: to.to_owned(),
        threads: results,
    })?;
    Ok(())
}

/// Send `feedback` into the branch: into its running turn by steering, or
/// as a new turn with `by send`'s path, recording the keys as delivered
/// first and taking them back if the send could not start. Returns once
/// the turn that has it ended.
fn deliver(
    env: &Env,
    target: &Target,
    yard: &Yard,
    name: &str,
    pr: &PullRequestRef,
    feedback: &[Feedback],
    task: &TaskArgs,
) -> Outcome {
    let text = feedback_prompt(pr, feedback);
    let keys: Vec<String> = feedback.iter().map(|f| f.key.clone()).collect();
    let summary: Vec<String> = feedback.iter().map(|f| f.summary.clone()).collect();
    print(&format!(
        "sending {} piece{} of feedback into {name}: {}\n",
        feedback.len(),
        plural(feedback.len() as u32),
        summary.join("; ")
    ))?;
    let branch = yard.branch(name)?;
    if branch.info().status == BranchStatus::Running {
        let steer = yard
            .steer_as(name, &text, "by pr --watch")
            .and_then(|s| yard.wait_steer(name, s.id, STEER_WAIT));
        if let Ok(steer) = steer {
            if matches!(
                steer.state,
                branchyard::SteerState::Written | branchyard::SteerState::Accepted
            ) {
                branch.record_pull_request(PullRequestActivity::FeedbackDelivered {
                    keys,
                    via: "steer".into(),
                    summary,
                })?;
                print(&format!("steered into {name}'s running turn\n"))?;
                return wait_settled(yard, name);
            }
        }
        // Not taken: wait for the turn to end and start one.
        wait_settled(yard, name)?;
    }
    branch.record_pull_request(PullRequestActivity::FeedbackDelivered {
        keys: keys.clone(),
        via: "send".into(),
        summary,
    })?;
    match commands::send(
        env,
        target,
        name,
        commands::Prompt::Text(&text),
        task,
        false,
        false,
    ) {
        Ok(()) => Ok(()),
        // The turn ran and failed; the feedback reached it.
        Err(Failure::Reported) => Err(Failure::Message(format!(
            "{name}'s turn with the feedback failed; stopped watching (by log {name})"
        ))),
        Err(failure) => {
            branch.record_pull_request(PullRequestActivity::FeedbackUndelivered {
                keys,
                reason: failure.to_string(),
            })?;
            Err(failure)
        }
    }
}

/// Wait until `name` has no running turn.
fn wait_settled(yard: &Yard, name: &str) -> Outcome {
    let start = Instant::now();
    let mut said = false;
    loop {
        let info = yard.branch(name)?.info().clone();
        if info.status != BranchStatus::Running {
            return Ok(());
        }
        if !said && start.elapsed() > Duration::from_secs(2) {
            said = true;
            eprintln!("by: waiting for {name}'s running turn to end");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::CandidateInfo;
    use std::path::PathBuf;

    fn info(prompt: &str) -> BranchInfo {
        BranchInfo {
            name: "b".into(),
            git_branch: "by/b".into(),
            worktree: PathBuf::from("/w"),
            prompt: prompt.into(),
            harness: "codex".into(),
            profile: "codex-app-server".into(),
            session: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
            base: "base".into(),
            candidate: Some(CandidateInfo {
                commit: "c".repeat(40),
                files_changed: 2,
                insertions: 3,
                deletions: 1,
            }),
            status: BranchStatus::Ready,
            turns: 2,
            cost_usd: Some(0.25),
            created_at: 0,
            stalled: false,
            superseded_by: None,
            model: None,
        }
    }

    fn issue() -> Issue {
        Issue {
            number: 12,
            title: "Parser: crash on empty input!".into(),
            body: "It panics.\n".into(),
            url: "https://github.com/acme/widgets/issues/12".into(),
            labels: vec!["bug".into(), "parser".into()],
            tracker: None,
            key: None,
        }
    }

    fn at(at_ms: u64, activity: PullRequestActivity) -> RecordedEvent {
        RecordedEvent {
            at_ms,
            activity: Activity::PullRequest(Box::new(activity)),
        }
    }

    #[test]
    fn issue_references_prompts_and_names() {
        assert_eq!(issue_ref("#12").unwrap(), "12");
        assert_eq!(issue_ref(" 12 ").unwrap(), "12");
        let url = "https://github.com/acme/widgets/issues/12";
        assert_eq!(issue_ref(url).unwrap(), url);
        for bad in ["#", "0", "twelve", "-3", "#1x"] {
            assert!(issue_ref(bad).is_err(), "{bad}");
        }
        let prompt = issue_prompt(&issue(), "");
        assert_eq!(
            prompt,
            "Resolve GitHub issue #12: Parser: crash on empty input!\n\
             https://github.com/acme/widgets/issues/12\nLabels: bug, parser\n\nIt panics.\n"
        );
        assert_eq!(issue_from_prompt(&prompt), Some(issue().link()));
        let extra = issue_prompt(&issue(), "  use the new API ");
        assert!(extra.ends_with("It panics.\n\nAdditional instructions:\nuse the new API\n"));
        assert_eq!(issue_from_prompt("fix the parser"), None);
        assert_eq!(
            issue_from_prompt("Resolve GitHub issue #x: t\nhttps://e"),
            None
        );
        assert_eq!(
            issue_branch_name(&issue()),
            "issue-12-parser-crash-on-empty-input"
        );
        let untitled = Issue {
            title: " ".into(),
            ..issue()
        };
        assert_eq!(issue_branch_name(&untitled), "issue-12");
    }

    #[test]
    fn other_trackers_prompt_name_and_close_by_their_own_keys() {
        let linear = Issue {
            number: 123,
            title: "Parser panics".into(),
            body: String::new(),
            url: "https://linear.app/acme/issue/ENG-123".into(),
            labels: Vec::new(),
            tracker: Some(crate::trackers::Tracker::Linear),
            key: Some("ENG-123".into()),
        };
        let prompt = issue_prompt(&linear, "");
        assert!(prompt.starts_with(
            "Resolve Linear issue ENG-123: Parser panics\nhttps://linear.app/acme/issue/ENG-123\n"
        ));
        assert_eq!(issue_from_prompt(&prompt), Some(linear.link()));
        assert_eq!(issue_branch_name(&linear), "eng-123-parser-panics");
        let gitlab = Issue {
            key: Some("acme/widgets#12".into()),
            tracker: Some(crate::trackers::Tracker::GitLab),
            number: 12,
            ..linear.clone()
        };
        assert_eq!(issue_branch_name(&gitlab), "issue-12-parser-panics");
        assert_eq!(
            issue_from_prompt(&issue_prompt(&gitlab, "")).map(|l| l.key),
            Some(Some("acme/widgets#12".into()))
        );
        let jira = IssueLink {
            tracker: Some("jira".into()),
            key: Some("PROJ-7".into()),
            ..linear.link()
        };
        assert_eq!(closing_line(&linear.link(), true), "Closes ENG-123\n");
        assert_eq!(
            closing_line(&jira, true),
            "Refs PROJ-7 (https://linear.app/acme/issue/ENG-123)\n"
        );
        assert!(closing_line(&gitlab.link(), true).starts_with("Related: acme/widgets#12 ("));
        assert_eq!(closing_line(&issue().link(), true), "Closes #12\n");
        assert_eq!(
            log_line(&PullRequestActivity::IssueLinked(linear.link())).0,
            "issue ENG-123 linked: Parser panics (https://linear.app/acme/issue/ENG-123)"
        );
    }

    #[test]
    fn state_folds_the_log_and_undelivered_feedback_comes_back() {
        let pr = PullRequestRef {
            number: 7,
            url: "https://github.com/acme/widgets/pull/7".into(),
            head: "by/b".into(),
            base: None,
            draft: false,
        };
        let events = vec![
            at(1, PullRequestActivity::Opened(pr.clone())),
            at(
                2,
                PullRequestActivity::FeedbackDelivered {
                    keys: vec!["ci:test:1".into(), "comment:2".into()],
                    via: "send".into(),
                    summary: Vec::new(),
                },
            ),
            at(
                3,
                PullRequestActivity::FeedbackDelivered {
                    keys: vec!["thread:3".into()],
                    via: "send".into(),
                    summary: Vec::new(),
                },
            ),
            at(
                4,
                PullRequestActivity::FeedbackUndelivered {
                    keys: vec!["thread:3".into()],
                    reason: "running".into(),
                },
            ),
        ];
        let state = state(&info(&issue_prompt(&issue(), "")), &events);
        assert_eq!(state.pull_request, Some(pr));
        assert_eq!(state.issue, Some(issue().link()));
        assert_eq!(
            state.delivered.iter().collect::<Vec<_>>(),
            ["ci:test:1", "comment:2"]
        );
        assert_eq!(state.rounds, 1);
    }

    #[test]
    fn readiness_names_what_blocks_a_merge() {
        let branch = info("p");
        assert_eq!(readiness(&branch, &PrState::default()), None);
        let check = |commit: &str, passed: bool| CheckRun {
            commit: commit.into(),
            argv: vec!["cargo".into(), "test".into()],
            passed,
            timed_out: false,
            output_tail: String::new(),
        };
        let mut state = PrState {
            check: Some((1, check(&"c".repeat(40), true))),
            ..PrState::default()
        };
        let r = readiness(&branch, &state).unwrap();
        assert_eq!(r.verdict, "not_ready");
        assert_eq!(r.blockers, ["no pull request"]);
        state.pull_request = Some(PullRequestRef {
            number: 7,
            url: "u".into(),
            head: "by/b".into(),
            base: None,
            draft: false,
        });
        state.check = Some((1, check("old", true)));
        let r = readiness(&branch, &state).unwrap();
        assert_eq!(r.verdict, "unobserved");
        assert_eq!(r.parts[0], "check ran on old, not the candidate");
        state.check = Some((1, check(&"c".repeat(40), true)));
        let mut observation = PullRequestObservation {
            number: 7,
            url: "u".into(),
            state: "open".into(),
            mergeable: Some("mergeable".into()),
            merge_state: Some("clean".into()),
            review_decision: Some("approved".into()),
            ci: CiSummary {
                passed: 3,
                skipped: 1,
                ..CiSummary::default()
            },
            ..PullRequestObservation::default()
        };
        state.observed = Some((5_000, observation.clone()));
        let r = readiness(&branch, &state).unwrap();
        assert_eq!(r.verdict, "ready");
        assert_eq!(
            readiness_text(&r, 65_000, Style { color: false }),
            "ready to merge · check passed · PR #7 open · CI 3 passed, 1 skipped · \
             0 unresolved threads · mergeable · approved (observed 1m ago)"
        );
        assert_eq!(r.json["observed_at_ms"], 5_000);
        observation.draft = true;
        observation.merge_state = Some("behind".into());
        state.observed = Some((5_000, observation.clone()));
        assert_eq!(
            readiness(&branch, &state).unwrap().blockers,
            ["draft", "behind its base"]
        );
        observation.state = "merged".into();
        state.observed = Some((5_000, observation));
        assert_eq!(readiness(&branch, &state).unwrap().verdict, "merged");
    }

    #[test]
    fn the_body_names_the_task_issue_turns_cost_check_and_files() {
        let branch = info(&issue_prompt(&issue(), ""));
        let state = PrState {
            issue: Some(issue().link()),
            check: Some((
                1,
                CheckRun {
                    commit: "c".repeat(40),
                    argv: vec!["cargo".into(), "test".into()],
                    passed: true,
                    timed_out: false,
                    output_tail: String::new(),
                },
            )),
            ..PrState::default()
        };
        let text = body(&branch, &state, " a.rs | 2 +-\n", None);
        for expected in [
            "## Task\n\n> Resolve GitHub issue #12: Parser: crash on empty input!\n",
            ">\n> It panics.\n",
            "\nCloses #12\n",
            "| Branch | `by/b` on codex |",
            "| Turns | 2 |",
            "| Cost | $0.25, the harness's estimate |",
            "| Check | `cargo test` passed on `cccccccccc` |",
            "| Candidate | `cccccccccc`: 2 files changed, +3 −1 |",
            "```\n a.rs | 2 +-\n```",
        ] {
            assert!(text.contains(expected), "{expected:?} missing from\n{text}");
        }
        let elsewhere = Some(("github.com".into(), "acme".into(), "gadgets".into()));
        let text = body(&branch, &state, "", elsewhere);
        assert!(text.contains("\nCloses acme/widgets#12\n"), "{text}");
        assert!(!text.contains("<details>"));
    }

    #[test]
    fn only_delivered_threads_on_changed_files_are_resolved_once() {
        let thread = |id: &str, path: &str, resolved: bool, keys: &[&str]| ReviewThread {
            id: id.into(),
            path: Some(path.into()),
            resolved,
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        };
        let threads = [
            thread("T1", "a.rs", false, &["thread:C1"]),
            // A reply not delivered yet: the reviewer is still talking.
            thread("T2", "a.rs", false, &["thread:C2", "thread:C3"]),
            thread("T3", "b.rs", false, &["thread:C4"]),
            thread("T4", "a.rs", true, &["thread:C5"]),
            thread("", "a.rs", false, &["thread:C6"]),
            thread("T5", "a.rs", false, &["thread:C7"]),
            thread("T6", "a.rs", false, &[]),
        ];
        let mut state = PrState::default();
        for key in ["C1", "C2", "C4", "C5", "C6", "C7"] {
            state.delivered.insert(format!("thread:{key}"));
        }
        // T5 was tried after an earlier push.
        state.resolved_threads.insert("T5".into());
        let changed: BTreeSet<String> = ["a.rs".to_owned()].into();
        let ids: Vec<&str> = addressed_threads(&state, &threads, &changed)
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ids, ["T1"]);
        assert!(addressed_threads(&state, &threads, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn urls_logs_and_prompts() {
        assert_eq!(pr_number("https://github.com/a/b/pull/42"), Some(42));
        assert_eq!(pr_number("https://github.com/a/b/pull/42/"), Some(42));
        assert_eq!(pr_number("nope"), None);
        assert_eq!(
            repo_of("https://ghe.example/a/b/pull/1"),
            Some(("ghe.example".into(), "a".into(), "b".into()))
        );
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
        assert_eq!(truncate("abcdef", 3), "abc…");
        let pr = PullRequestRef {
            number: 7,
            url: "https://x/pull/7".into(),
            head: "h".into(),
            base: None,
            draft: false,
        };
        let feedback = vec![Feedback {
            key: "k".into(),
            summary: "s".into(),
            text: "CI check \"t\" failed.".into(),
            log: None,
        }];
        assert_eq!(
            feedback_prompt(&pr, &feedback),
            "Feedback on pull request #7 (https://x/pull/7) for this branch:\n\n\
             1. CI check \"t\" failed.\n\nAddress it on this branch. When this turn ends, the \
             branch is pushed to the pull request again.\n"
        );
    }
}
