//! `by trigger add|list|show|test|enable|disable|rm|runs|secret`: tasks
//! started on a schedule or by a webhook. See docs/triggers.md.
//!
//! Locally, `by trigger` reads and writes the trigger store of the server
//! this repository would run (`by serve`'s data directory, or its
//! `--database`, from `[serve] config` when branchyard.toml names one).
//! Triggers fire only where a dispatcher runs: `by serve` or `by worker` on
//! that store. With `--remote`, the same actions go through the server's
//! API, as the token's principal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use branchyard_client::api::{BudgetSpec, PolicySpec, TaskRequest};
use branchyard_client::new_key;
use branchyard_client::triggers::{
    Busy, Conditions, Deliver, EventSource, Precheck, RouteSpec, RunState, Trigger, TriggerCreated,
    TriggerPolicy, TriggerRun, TriggerSpec, TriggerTest, TriggerTestRequest, When,
};
use branchyard_server::config::Principal;
use branchyard_server::triggers::store::TriggerStore;
use branchyard_server::triggers::{self, StoredTrigger};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::commands::{print, Env, Failure, Outcome, Target};

pub const TRIGGER_EXAMPLES: &str = "\
Examples:
  by trigger add nightly --cron '0 3 * * 1-5' --tz Europe/Berlin \\
      --prompt 'Update dependencies and fix what breaks' --harness codex --check 'cargo test' --yes
  by trigger add triage --on github --if kind=issues.labeled --if label=agent \\
      --prompt 'Resolve GitHub issue #{{event.number}}: {{event.title}}\\n{{event.url}}\\n\\n{{event.text}}' \\
      --branch-name 'issue-{{event.number}}' --auto --yes
  by trigger test triage --event issue.json       # conditions, precheck and the task, creating nothing
  by trigger runs triage                          # fired, skipped (and why), failed, missed
  by trigger enable triage                        # after it paused itself
Triggers fire where `by serve` or `by worker` runs. See docs/triggers.md.";

/// `by trigger`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum TriggerAction {
    /// Create a trigger
    Add(Box<AddArgs>),
    /// Every trigger, with its schedule or webhook and state
    #[command(alias = "ls")]
    List,
    /// One trigger in full
    Show { name: String },
    /// What the trigger would do with an event (or at its next time), creating nothing
    Test {
        name: String,
        /// A delivery body, as the trigger's sender would post it
        #[arg(long, value_name = "FILE")]
        event: Option<PathBuf>,
        /// GitHub's event type for --event (default: from the body's shape)
        #[arg(long, value_name = "TYPE")]
        event_type: Option<String>,
        /// Also run the precheck, in a fresh worktree
        #[arg(long)]
        precheck: bool,
    },
    /// Enable it again: after a pause too, with its failure count reset
    Enable { name: String },
    /// Stop it firing until enabled
    Disable { name: String },
    /// Remove it and its runs
    Rm { name: String },
    /// Its recent runs, newest first
    Runs {
        name: String,
        /// How many
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Set the webhook secret from a file (Slack's or Linear's signing secret), or generate one
    Secret {
        name: String,
        #[arg(long, value_name = "FILE")]
        secret_file: Option<PathBuf>,
    },
}

/// `by trigger add`.
#[derive(Args, Clone, Debug, PartialEq)]
#[command(group(clap::ArgGroup::new("when").required(true).args(["cron", "every", "on"])))]
pub struct AddArgs {
    /// 1 to 63 of a-z, 0-9, '.', '_' and '-'
    pub name: String,
    /// The task; {{event.title}}, {{event.payload.issue.number}}, {{scheduled_at}}, ...
    #[arg(long)]
    pub prompt: String,
    /// A five-field cron expression, or @hourly, @daily, @weekly, @monthly
    #[arg(long, value_name = "EXPR", help_heading = "When")]
    pub cron: Option<String>,
    /// The cron expression's time zone (default UTC)
    #[arg(long, value_name = "ZONE", requires = "cron", help_heading = "When")]
    pub tz: Option<String>,
    /// Fire every DURATION (30m, 2h, 1d; at least a minute)
    #[arg(long, value_name = "DURATION", value_parser = duration, help_heading = "When")]
    pub every: Option<Duration>,
    /// Fire on webhooks from github, slack, linear or generic, or on email through postmark,
    /// mailgun or sendgrid (which need --if sender=...)
    #[arg(long, value_name = "SOURCE", help_heading = "When")]
    pub on: Option<EventSource>,
    /// Only events whose FIELD matches: kind, repo, label, author, branch, text (contains);
    /// for email also sender and recipient (an address or @domain) and subject (contains);
    /// repeatable, all must hold, the same field twice is either
    #[arg(long = "if", value_name = "FIELD=VALUE", help_heading = "When")]
    pub conditions: Vec<String>,
    /// Run this shell command in a fresh worktree first; exit 0 fires, anything else skips
    #[arg(long, value_name = "CMD", help_heading = "When")]
    pub precheck: Option<String>,
    /// Seconds the precheck may take (1 to 600; default 60)
    #[arg(
        long,
        value_name = "SECS",
        requires = "precheck",
        help_heading = "When"
    )]
    pub precheck_timeout: Option<u64>,
    /// The harness to run
    #[arg(long, help_heading = "Task")]
    pub harness: Option<String>,
    /// Route through the fleet table when it fires (docs/fleet.md)
    #[arg(long, conflicts_with = "harness", help_heading = "Task")]
    pub auto: bool,
    /// The task's kind for routing, instead of the classifier's
    #[arg(long, value_name = "KIND", requires = "auto", help_heading = "Task")]
    pub kind: Option<String>,
    /// The branch name, with placeholders (default: <trigger>-<issue number or time>)
    #[arg(long, value_name = "TEMPLATE", help_heading = "Task")]
    pub branch_name: Option<String>,
    /// Continue this branch run after run instead of creating one per run (placeholders
    /// allowed: one branch per Slack channel with slack-{{event.channel}}); the first run
    /// creates it
    #[arg(
        long,
        value_name = "TEMPLATE",
        conflicts_with = "branch_name",
        help_heading = "Task"
    )]
    pub to: Option<String>,
    /// With --to, while that branch runs a turn: queue the prompt for after it (default),
    /// steer it into the turn, or skip the run
    #[arg(long, value_name = "MODE", value_parser = busy, requires = "to", help_heading = "Task")]
    pub busy: Option<Busy>,
    /// Branch from this ref
    #[arg(long, help_heading = "Task")]
    pub base: Option<String>,
    /// Check to pass before merging, such as "cargo test"
    #[arg(long, value_name = "CMD", help_heading = "Task")]
    pub check: Option<String>,
    /// Stop once the harness's cost estimate exceeds X dollars
    #[arg(long, value_name = "X", help_heading = "Task")]
    pub budget_usd: Option<f64>,
    /// Stop after N turns
    #[arg(long, value_name = "N", help_heading = "Task")]
    pub max_turns: Option<u32>,
    /// Interrupt the turn after N minutes
    #[arg(long, value_name = "N", help_heading = "Task")]
    pub max_minutes: Option<f64>,
    /// A connector grant for the branch (docs/connectors.md); repeatable
    #[arg(long = "connector", value_name = "GRANT", value_parser = branchyard::connectors::GrantEntry::parse, help_heading = "Task")]
    pub connectors: Vec<branchyard::connectors::GrantEntry>,
    /// A worker label the task needs; repeatable
    #[arg(long = "require-label", value_name = "LABEL", help_heading = "Task")]
    pub require_labels: Vec<String>,
    /// Allow every tool permission request (otherwise each is denied)
    #[arg(short, long, help_heading = "Task")]
    pub yes: bool,
    /// Pause after N failed runs in a row; 0 never (default 3)
    #[arg(long, value_name = "N", help_heading = "Policy")]
    pub pause_after: Option<u32>,
    /// Fire a scheduled time missed by at most this long once (default 1h)
    #[arg(long, value_name = "DURATION", value_parser = duration, help_heading = "Policy")]
    pub catch_up: Option<Duration>,
    /// The webhook secret, from a file (default: generated and printed once)
    #[arg(long, value_name = "FILE", help_heading = "Policy")]
    pub secret_file: Option<PathBuf>,
    /// Create it disabled
    #[arg(long, help_heading = "Policy")]
    pub disabled: bool,
}

/// `queue`, `steer` or `skip`.
fn busy(text: &str) -> Result<Busy, String> {
    match text.trim() {
        "queue" => Ok(Busy::Queue),
        "steer" => Ok(Busy::Steer),
        "skip" => Ok(Busy::Skip),
        other => Err(format!("{other:?} is not queue, steer or skip")),
    }
}

/// `90s`, `30m`, `2h`, `1d`, or seconds.
fn duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let n: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 90s, 30m, 2h or 1d"))?;
    let seconds = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => {
            return Err(format!(
                "{text:?} is not a duration such as 90s, 30m, 2h or 1d"
            ))
        }
    };
    Ok(Duration::from_secs(seconds))
}

fn read_secret(path: &Path) -> Result<String, Failure> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Failure::Message(format!("{}: {e}", path.display())))?;
    let secret = text.lines().next().unwrap_or("").trim().to_owned();
    if secret.is_empty() {
        return Err(Failure::Message(format!("{} is empty", path.display())));
    }
    Ok(secret)
}

/// The spec `args` describe, for repository `repo`.
fn spec(args: &AddArgs, repo: &str) -> Result<TriggerSpec, Failure> {
    let when = match (&args.cron, args.every, args.on) {
        (Some(expr), None, None) => When::Cron {
            expr: expr.clone(),
            timezone: args.tz.clone().unwrap_or_else(|| "UTC".into()),
        },
        (None, Some(every), None) => When::Interval {
            seconds: every.as_secs(),
        },
        (None, None, Some(source)) => When::Event { source },
        _ => {
            return Err(Failure::Message(
                "give one of --cron, --every and --on".into(),
            ))
        }
    };
    let mut conditions = Conditions::default();
    for condition in &args.conditions {
        let Some((field, value)) = condition.split_once('=') else {
            return Err(Failure::Message(format!(
                "--if {condition:?}: write FIELD=VALUE, such as label=agent"
            )));
        };
        let value = value.to_owned();
        match field.trim().trim_end_matches('~') {
            "kind" => conditions.kind.push(value),
            "repo" => conditions.repo.push(value),
            "label" => conditions.label.push(value),
            "author" => conditions.author.push(value),
            "branch" => conditions.branch.push(value),
            "text" | "text_contains" => conditions.text_contains.push(value),
            "sender" | "from" => conditions.sender.push(value),
            "recipient" | "to" => conditions.recipient.push(value),
            "subject" | "subject_contains" => conditions.subject_contains.push(value),
            other => {
                return Err(Failure::Message(format!(
                    "--if {other}=...: use kind, repo, label, author, branch, text, or for an \
                     email sender, recipient or subject"
                )))
            }
        }
    }
    let check = match &args.check {
        Some(line) => Some(
            shlex::split(line)
                .filter(|argv| !argv.is_empty())
                .ok_or_else(|| Failure::Message(format!("--check {line:?} is not a command")))?,
        ),
        None => None,
    };
    let provision = match args.connectors.is_empty() {
        true => None,
        false => Some(branchyard::Provisioning {
            connectors: args.connectors.clone(),
            ..branchyard::Provisioning::default()
        }),
    };
    let mut policy = TriggerPolicy::default();
    if let Some(n) = args.pause_after {
        policy.pause_after_failures = n;
    }
    if let Some(window) = args.catch_up {
        policy.catch_up_seconds = window.as_secs();
    }
    Ok(TriggerSpec {
        name: args.name.clone(),
        repo: repo.to_owned(),
        when,
        conditions,
        task: TaskRequest {
            prompt: args.prompt.replace("\\n", "\n"),
            harness: args.harness.clone(),
            name: args.branch_name.clone(),
            base: args.base.clone(),
            budget: BudgetSpec {
                max_usd: args.budget_usd,
                max_turns: args.max_turns,
                max_seconds: args.max_minutes.map(|m| m * 60.0),
                ..BudgetSpec::default()
            },
            policy: match args.yes {
                true => PolicySpec::allow_all(),
                false => PolicySpec::default(),
            },
            check,
            provision,
            require_labels: args.require_labels.clone(),
            ..TaskRequest::default()
        },
        route: args.auto.then(|| RouteSpec {
            kind: args.kind.clone(),
        }),
        deliver: args.to.as_ref().map(|branch| Deliver {
            branch: branch.clone(),
            busy: args.busy.unwrap_or_default(),
        }),
        precheck: args.precheck.as_ref().map(|command| Precheck {
            command: command.clone(),
            timeout_seconds: args.precheck_timeout.unwrap_or(60),
        }),
        enabled: !args.disabled,
        policy,
        secret: args.secret_file.as_deref().map(read_secret).transpose()?,
    })
}

fn time(ms: u64) -> String {
    branchyard_support::time::rfc3339_secs(ms)
}

fn describe_when(when: &When) -> String {
    match when {
        When::Cron { expr, timezone } => format!("cron \"{expr}\" {timezone}"),
        When::Interval { seconds } => match seconds {
            s if s % 86_400 == 0 => format!("every {}d", s / 86_400),
            s if s % 3600 == 0 => format!("every {}h", s / 3600),
            s if s % 60 == 0 => format!("every {}m", s / 60),
            s => format!("every {s}s"),
        },
        When::Event { source } => format!("on {}", source.as_str()),
    }
}

fn state_of(t: &Trigger) -> String {
    match (t.enabled, &t.paused_reason) {
        (true, _) => "enabled".into(),
        (false, Some(why)) if why.starts_with("paused") => "paused".into(),
        (false, _) => "disabled".into(),
    }
}

fn to_json(value: &impl serde::Serialize) -> Outcome {
    print(&format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    ))
}

fn show_list(triggers: &[Trigger], json: bool) -> Outcome {
    if json {
        return to_json(&json!({ "triggers": triggers }));
    }
    if triggers.is_empty() {
        return print("no triggers\n");
    }
    let mut out = String::new();
    for t in triggers {
        let next = match (&t.next_due_ms, &t.webhook_url) {
            (Some(ms), _) => format!("next {}", time(*ms)),
            (None, Some(url)) => url.clone(),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{:<20} {:<28} {:<10} {:<9} {next}\n",
            t.name,
            describe_when(&t.when),
            t.repo,
            state_of(t)
        ));
    }
    print(&out)
}

fn show_one(t: &Trigger, json: bool) -> Outcome {
    if json {
        return to_json(t);
    }
    let mut out = format!(
        "{} ({})\n  repo:     {}\n  when:     {}\n  state:    {}",
        t.name,
        t.id,
        t.repo,
        describe_when(&t.when),
        state_of(t)
    );
    if let Some(why) = &t.paused_reason {
        out.push_str(&format!(" ({why})"));
    }
    out.push('\n');
    if let Some(next) = t.next_due_ms {
        out.push_str(&format!("  next:     {}\n", time(next)));
    }
    if let Some(url) = &t.webhook_url {
        out.push_str(&format!(
            "  webhook:  {url}{}\n",
            if t.has_secret { "" } else { " (no secret yet)" }
        ));
    }
    if !t.conditions.is_empty() {
        let c = &t.conditions;
        let mut parts = Vec::new();
        for (field, values) in [
            ("kind", &c.kind),
            ("repo", &c.repo),
            ("label", &c.label),
            ("author", &c.author),
            ("branch", &c.branch),
            ("text", &c.text_contains),
            ("sender", &c.sender),
            ("recipient", &c.recipient),
            ("subject", &c.subject_contains),
        ] {
            if !values.is_empty() {
                parts.push(format!("{field}={}", values.join("|")));
            }
        }
        out.push_str(&format!("  if:       {}\n", parts.join(" ")));
    }
    if let Some(p) = &t.precheck {
        out.push_str(&format!(
            "  precheck: {} ({}s)\n",
            p.command, p.timeout_seconds
        ));
    }
    let runs_on = match (&t.route, &t.task.harness) {
        (Some(route), _) => format!(
            "routed by the fleet table{}",
            route
                .kind
                .as_ref()
                .map(|k| format!(" as {k}"))
                .unwrap_or_default()
        ),
        (None, Some(h)) => h.clone(),
        (None, None) => "the default harness".into(),
    };
    out.push_str(&format!("  harness:  {runs_on}\n"));
    if let Some(d) = &t.deliver {
        out.push_str(&format!(
            "  deliver:  to {} run after run (created if missing; while it runs a turn: {})\n",
            d.branch,
            d.busy.as_str()
        ));
    }
    out.push_str(&format!(
        "  prompt:   {}\n",
        t.task.prompt.replace('\n', "\n            ")
    ));
    out.push_str(&format!(
        "  failures: {} in a row (pauses at {})\n  created:  {} by {}\n",
        t.consecutive_failures,
        match t.policy.pause_after_failures {
            0 => "never".to_owned(),
            n => n.to_string(),
        },
        time(t.created_at_ms),
        t.created_by
    ));
    print(&out)
}

fn show_runs(runs: &[TriggerRun], json: bool) -> Outcome {
    if json {
        return to_json(&json!({ "runs": runs }));
    }
    if runs.is_empty() {
        return print("no runs\n");
    }
    let mut out = String::new();
    for r in runs {
        let mut what = match r.state {
            RunState::Fired => format!(
                "{} {}",
                r.branches.join(", "),
                r.operation.as_deref().unwrap_or("")
            ),
            RunState::Missed => format!(
                "{} time(s) from {}",
                r.missed.unwrap_or(0),
                r.scheduled_ms.map(time).unwrap_or_default()
            ),
            _ => r.reason.clone().unwrap_or_default(),
        };
        if let Some(outcome) = &r.outcome {
            what.push_str(&format!(
                " -> {}",
                match outcome.ok {
                    true => "ok",
                    false => &outcome.detail,
                }
            ));
        }
        out.push_str(&format!(
            "{}  {:<17} {:<28} {what}\n",
            time(r.at_ms),
            r.state.as_str(),
            r.key
        ));
    }
    print(&out)
}

fn show_test(test: &TriggerTest, json: bool) -> Outcome {
    if json {
        return to_json(test);
    }
    let mut out = String::new();
    if let Some(e) = &test.event {
        out.push_str(&format!("event:     {} {} ({})\n", e.source, e.kind, e.id));
    }
    out.push_str(&format!(
        "matches:   {}\n",
        if test.matched { "yes" } else { "no" }
    ));
    if let Some(p) = &test.precheck {
        out.push_str(&format!(
            "precheck:  {} -> {}\n",
            p.command,
            match p.passed() {
                true => "passed".to_owned(),
                false => triggers::precheck::failure(p),
            }
        ));
    }
    out.push_str(&format!(
        "would fire: {}{}\n",
        if test.would_fire { "yes" } else { "no" },
        test.reason
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default()
    ));
    out.push_str(&format!("key:       {}\n", test.key));
    if let Some(task) = &test.task {
        out.push_str(&format!(
            "branch:    {}\nprompt:\n{}\n",
            task.name.as_deref().unwrap_or(""),
            task.prompt
                .lines()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Some(d) = &test.deliver {
        out.push_str(&format!(
            "delivers:  to {} (created if missing; while it runs a turn: {})\n",
            d.branch,
            d.busy.as_str()
        ));
    }
    if let Some(route) = &test.route {
        out.push_str(&format!(
            "routed:    by the fleet table when it fires{}\n",
            route
                .kind
                .as_ref()
                .map(|k| format!(", as {k}"))
                .unwrap_or_default()
        ));
    }
    print(&out)
}

fn show_created(created: &TriggerCreated, json: bool, local: bool) -> Outcome {
    if json {
        return to_json(created);
    }
    let t = &created.trigger;
    let mut out = format!("created trigger {} ({})\n", t.name, t.id);
    match (t.next_due_ms, &t.webhook_url) {
        (Some(next), _) => out.push_str(&format!("next: {}\n", time(next))),
        (None, Some(url)) => out.push_str(&format!("webhook URL: {url}\n")),
        _ => {}
    }
    if let Some(secret) = &created.secret {
        out.push_str(&format!(
            "webhook secret (shown once; give it to the sender): {secret}\n"
        ));
        // Postmark and SendGrid sign nothing: the secret goes in the URL,
        // as a Basic password or as its last segment.
        if let (When::Event { source }, Some(url)) = (&t.when, &t.webhook_url) {
            if matches!(source, EventSource::Postmark | EventSource::Sendgrid) {
                out.push_str(&format!(
                    "{} signs nothing: give it the URL with the secret as a Basic password \
                     (https://branchyard:SECRET@host/...) or as its last segment: \
                     {url}/{secret}\n",
                    source.as_str()
                ));
            }
        }
    }
    if local {
        out.push_str(
            "it fires where `by serve` or `by worker` runs on this repository's server state\n",
        );
    }
    print(&out)
}

fn test_request(
    event: &Option<PathBuf>,
    event_type: &Option<String>,
    precheck: bool,
) -> Result<TriggerTestRequest, Failure> {
    let event =
        match event {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| Failure::Message(format!("{}: {e}", path.display())))?;
                Some(serde_json::from_str(&text).map_err(|e| {
                    Failure::Message(format!("{} is not JSON: {e}", path.display()))
                })?)
            }
            None => None,
        };
    Ok(TriggerTestRequest {
        event,
        event_type: event_type.clone(),
        run_precheck: precheck,
    })
}

/// `by trigger ACTION`.
pub fn main(_env: &Env, target: &Target, action: &TriggerAction, json: bool) -> Outcome {
    match target {
        Target::Remote(remote) => remote_main(&remote.client, remote.repo.name(), action, json),
        Target::Local => Local::open()?.main(action, json),
    }
}

fn remote_main(
    client: &branchyard_client::Client,
    repo: &str,
    action: &TriggerAction,
    json: bool,
) -> Outcome {
    match action {
        TriggerAction::Add(args) => {
            let created = client.create_trigger(&spec(args, repo)?, &new_key())?;
            show_created(&created, json, false)
        }
        TriggerAction::List => show_list(&client.triggers()?, json),
        TriggerAction::Show { name } => show_one(&client.trigger(name)?, json),
        TriggerAction::Test {
            name,
            event,
            event_type,
            precheck,
        } => {
            let request = test_request(event, event_type, *precheck)?;
            show_test(&client.test_trigger(name, &request, &new_key())?, json)
        }
        TriggerAction::Enable { name } => show_one(&client.enable_trigger(name, &new_key())?, json),
        TriggerAction::Disable { name } => {
            show_one(&client.disable_trigger(name, &new_key())?, json)
        }
        TriggerAction::Rm { name } => {
            let removed = client.remove_trigger(name)?;
            match json {
                true => to_json(&removed),
                false => print(&format!("removed trigger {}\n", removed.removed)),
            }
        }
        TriggerAction::Runs { name, limit } => show_runs(&client.trigger_runs(name, *limit)?, json),
        TriggerAction::Secret { name, secret_file } => {
            let secret = secret_file.as_deref().map(read_secret).transpose()?;
            let set = client.set_trigger_secret(name, secret, &new_key())?;
            show_secret(name, set.secret, json)
        }
    }
}

fn show_secret(name: &str, generated: Option<String>, json: bool) -> Outcome {
    if json {
        return to_json(&json!({ "trigger": name, "secret": generated }));
    }
    match generated {
        Some(secret) => print(&format!(
            "new webhook secret for {name} (shown once): {secret}\n"
        )),
        None => print(&format!("set the webhook secret of {name}\n")),
    }
}

/// The trigger store of the server this repository would run.
struct Local {
    config: branchyard_server::Config,
    store: Arc<dyn TriggerStore>,
    repo: String,
    root: PathBuf,
}

fn failure(e: impl std::fmt::Display) -> Failure {
    Failure::Message(e.to_string())
}

impl Local {
    fn open() -> Result<Local, Failure> {
        let cwd = std::env::current_dir()?;
        let env = |name: &str| std::env::var(name).ok();
        let args =
            crate::defaults::apply_serve(&cwd, &env, Vec::new()).map_err(Failure::Message)?;
        let config = branchyard_server::cli::resolve(&args).map_err(Failure::Message)?;
        let here = crate::commands::open()?.root().to_path_buf();
        let (repo, root) = config
            .repos
            .iter()
            .find(|(_, path)| std::fs::canonicalize(path).ok() == std::fs::canonicalize(&here).ok())
            .or(match config.repos.as_slice() {
                [only] => Some(only),
                _ => None,
            })
            .cloned()
            .ok_or_else(|| {
                Failure::Message(format!(
                    "the server configuration does not serve {}; run by trigger in a served \
                     repository, or use --remote",
                    here.display()
                ))
            })?;
        let store = triggers::store::open(&config).map_err(Failure::Message)?;
        Ok(Local {
            config,
            store,
            repo,
            root,
        })
    }

    fn base_url(&self) -> String {
        self.config.triggers.public_url.clone().unwrap_or_else(|| {
            let scheme = if self.config.tls.is_some() {
                "https"
            } else {
                "http"
            };
            format!("{scheme}://{}", self.config.listen)
        })
    }

    fn find(&self, key: &str) -> Result<StoredTrigger, Failure> {
        let found = self.store.find(None, key).map_err(failure)?;
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(Failure::Message(format!("no trigger named {key}"))),
            several => several
                .iter()
                .find(|t| t.id == key)
                .or_else(|| {
                    several
                        .iter()
                        .find(|t| t.tenant == branchyard_server::config::DEFAULT_TENANT)
                })
                .cloned()
                .ok_or_else(|| {
                    Failure::Message(format!(
                        "{key} names triggers of several tenants; give its ID"
                    ))
                }),
        }
    }

    fn info(&self, t: &StoredTrigger) -> Trigger {
        t.info(&self.base_url())
    }

    fn reread(&self, id: &str) -> Result<Trigger, Failure> {
        let t = self
            .store
            .get(id)
            .map_err(failure)?
            .ok_or_else(|| Failure::Message("the trigger was removed".into()))?;
        Ok(self.info(&t))
    }

    fn main(&self, action: &TriggerAction, json: bool) -> Outcome {
        let now = branchyard_support::time::now_ms();
        match action {
            TriggerAction::Add(args) => self.add(args, json, now),
            TriggerAction::List => {
                let all = self.store.list(None).map_err(failure)?;
                show_list(&all.iter().map(|t| self.info(t)).collect::<Vec<_>>(), json)
            }
            TriggerAction::Show { name } => show_one(&self.info(&self.find(name)?), json),
            TriggerAction::Test {
                name,
                event,
                event_type,
                precheck,
            } => {
                let t = self.find(name)?;
                let request = test_request(event, event_type, *precheck)?;
                // At this terminal, --precheck is the person's own decision
                // to run the trigger's command, as running it by hand is.
                let scratch = self.config.data_dir.join("triggers");
                let tested = branchyard_server::triggers::routes::evaluate(
                    &t,
                    &request,
                    now,
                    precheck.then_some((scratch.as_path(), self.root.as_path())),
                )
                .map_err(Failure::Message)?;
                show_test(&tested, json)
            }
            TriggerAction::Enable { name } => {
                let t = self.find(name)?;
                let next = triggers::Schedule::of(&t.spec.when)
                    .map_err(Failure::Message)?
                    .and_then(|s| s.next_after(now, now));
                self.store
                    .set_state(&t.id, true, None, 0, next)
                    .map_err(failure)?;
                show_one(&self.reread(&t.id)?, json)
            }
            TriggerAction::Disable { name } => {
                let t = self.find(name)?;
                self.store
                    .set_state(&t.id, false, Some("disabled locally"), t.failures, None)
                    .map_err(failure)?;
                show_one(&self.reread(&t.id)?, json)
            }
            TriggerAction::Rm { name } => {
                let t = self.find(name)?;
                self.store.remove(&t.id).map_err(failure)?;
                match json {
                    true => to_json(&json!({ "removed": t.spec.name })),
                    false => print(&format!("removed trigger {}\n", t.spec.name)),
                }
            }
            TriggerAction::Runs { name, limit } => {
                let t = self.find(name)?;
                show_runs(&self.store.runs(&t.id, *limit).map_err(failure)?, json)
            }
            TriggerAction::Secret { name, secret_file } => {
                let t = self.find(name)?;
                if t.spec.when.is_schedule() {
                    return Err(Failure::Message(
                        "a schedule takes no webhook secret".into(),
                    ));
                }
                let (secret, generated) = match secret_file {
                    Some(path) => (read_secret(path)?, None),
                    None => {
                        let s = triggers::new_secret();
                        (s.clone(), Some(s))
                    }
                };
                self.store.set_secret(&t.id, &secret).map_err(failure)?;
                show_secret(&t.spec.name, generated, json)
            }
        }
    }

    fn add(&self, args: &AddArgs, json: bool, now: u64) -> Outcome {
        let mut spec = triggers::validate(spec(args, &self.repo)?).map_err(Failure::Message)?;
        if spec.precheck.is_some() && !self.config.triggers.allow_prechecks.allows(&self.repo) {
            return Err(Failure::Message(format!(
                "the server does not let {}'s triggers run prechecks: start it with \
                 by serve --allow-trigger-prechecks, or set allow_trigger_prechecks in its \
                 configuration",
                self.repo
            )));
        }
        let event = !spec.when.is_schedule();
        let (secret, generated) = match (spec.secret.take(), event) {
            (Some(s), _) => (Some(s), None),
            (None, true) => {
                let s = triggers::new_secret();
                (Some(s.clone()), Some(s))
            }
            (None, false) => (None, None),
        };
        let enabled = spec.enabled;
        let next = match enabled {
            true => triggers::Schedule::of(&spec.when)
                .map_err(Failure::Message)?
                .and_then(|s| s.next_after(now, now)),
            false => None,
        };
        let user = std::env::var("USER").unwrap_or_else(|_| "local".into());
        let t = StoredTrigger {
            id: triggers::new_id("trg"),
            tenant: branchyard_server::config::DEFAULT_TENANT.into(),
            spec,
            principal: Principal::default_for(&format!("{user} (local)")),
            created_at_ms: now,
            secret,
            enabled,
            paused_reason: (!enabled).then(|| "created disabled".to_owned()),
            failures: 0,
            next_due_ms: next,
        };
        if !self.store.create(&t).map_err(failure)? {
            return Err(Failure::Message(format!(
                "a trigger named {} exists already",
                t.spec.name
            )));
        }
        show_created(
            &TriggerCreated {
                trigger: self.info(&t),
                secret: generated,
            },
            json,
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_units() {
        assert_eq!(duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(duration("1d"), Ok(Duration::from_secs(86_400)));
        assert_eq!(duration("45"), Ok(Duration::from_secs(45)));
        assert!(duration("2w").is_err());
        assert!(duration("h").is_err());
    }

    #[test]
    fn conditions_come_from_if_flags() {
        let args = AddArgs {
            name: "triage".into(),
            prompt: "Fix {{event.title}}".into(),
            cron: None,
            tz: None,
            every: None,
            on: Some(EventSource::Github),
            conditions: vec![
                "kind=issues.labeled".into(),
                "label=agent".into(),
                "label=bot".into(),
                "text~=@by".into(),
            ],
            precheck: None,
            precheck_timeout: None,
            harness: None,
            auto: true,
            kind: Some("bugfix".into()),
            branch_name: None,
            to: None,
            busy: None,
            base: None,
            check: Some("cargo test -p x".into()),
            budget_usd: Some(2.0),
            max_turns: None,
            max_minutes: Some(30.0),
            connectors: Vec::new(),
            require_labels: Vec::new(),
            yes: true,
            pause_after: Some(5),
            catch_up: None,
            secret_file: None,
            disabled: false,
        };
        let s = spec(&args, "app").unwrap();
        assert_eq!(s.conditions.kind, ["issues.labeled"]);
        assert_eq!(s.conditions.label, ["agent", "bot"]);
        assert_eq!(s.conditions.text_contains, ["@by"]);
        assert_eq!(s.route.unwrap().kind.as_deref(), Some("bugfix"));
        assert_eq!(s.task.check.unwrap(), ["cargo", "test", "-p", "x"]);
        assert_eq!(s.task.budget.max_seconds, Some(1800.0));
        assert_eq!(s.policy.pause_after_failures, 5);
        let mut bad = args.clone();
        bad.conditions = vec!["colour=red".into()];
        assert!(spec(&bad, "app").is_err());
        // --to names the branch every run continues; --busy says what a
        // run does while it runs a turn, queue unless told otherwise.
        let mut resident = args.clone();
        resident.to = Some("slack-{{event.channel}}".into());
        let s = spec(&resident, "app").unwrap();
        assert_eq!(
            s.deliver,
            Some(Deliver {
                branch: "slack-{{event.channel}}".into(),
                busy: Busy::Queue
            })
        );
        resident.busy = Some(busy("skip").unwrap());
        assert_eq!(
            spec(&resident, "app").unwrap().deliver.unwrap().busy,
            Busy::Skip
        );
        assert!(busy("drop").is_err());
        let mut mail = args.clone();
        mail.on = Some(EventSource::Mailgun);
        mail.conditions = vec![
            "sender=@partner.example".into(),
            "from=alice@example.com".into(),
            "to=agent@by.example".into(),
            "subject=[agent]".into(),
        ];
        let s = spec(&mail, "app").unwrap();
        assert_eq!(
            s.conditions.sender,
            ["@partner.example", "alice@example.com"]
        );
        assert_eq!(s.conditions.recipient, ["agent@by.example"]);
        assert_eq!(s.conditions.subject_contains, ["[agent]"]);
        assert_eq!("sendgrid".parse::<EventSource>(), Ok(EventSource::Sendgrid));
    }
}
