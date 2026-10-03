//! Triggers and schedules: durable objects that create ordinary tasks on
//! a cron schedule, at an interval, or when a signed webhook arrives. See
//! `docs/triggers.md`.
//!
//! - [`store`]: triggers and their runs in the same database as the
//!   operation registry (the data directory's SQLite, or PostgreSQL).
//! - [`engine`]: the dispatcher every server and `by worker` runs: it
//!   claims due schedule times and pending runs atomically, runs the
//!   precheck, and admits the task through the server's own admission
//!   path with an idempotency key of the trigger and the event or
//!   scheduled time, so nothing fires twice.
//! - [`routes`]: the HTTP API, and the unauthenticated, signed webhook
//!   endpoint `POST /v1/triggers/{id}/fire`.
//! - [`events`], [`email`], [`cron`], [`template`], [`precheck`],
//!   [`target`]: the parts, each testable alone.

pub mod cron;
pub mod dispatch;
pub mod email;
pub mod engine;
pub mod events;
pub mod precheck;
pub mod routes;
pub mod store;
pub mod target;
pub mod template;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard_client::triggers::{
    Conditions, EventSource, Trigger, TriggerEvent, TriggerSpec, When,
};
use serde::{Deserialize, Serialize};

use crate::config::{Principal, WorkspaceScripts};

/// What a server's configuration says about triggers.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The server's URL as webhook senders reach it, for each event
    /// trigger's `webhook_url`. Default: `http(s)://<listen>`.
    pub public_url: Option<String>,
    /// Which served repositories' triggers may run a precheck: the
    /// operator's decision, like `allow_workspace_scripts`.
    pub allow_prechecks: WorkspaceScripts,
    /// How often the dispatcher looks for due schedules and pending runs
    /// besides when it is woken.
    pub tick: Duration,
    /// The clock schedules are computed by. Tests set a manual one.
    pub clock: Clock,
    /// Run the dispatcher at all. Off only in tests that drive it alone.
    pub dispatch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            public_url: None,
            allow_prechecks: WorkspaceScripts::Denied,
            tick: Duration::from_secs(1),
            clock: Clock::system(),
            dispatch: true,
        }
    }
}

/// Milliseconds since the Unix epoch, from the system or a test's hand.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Clock({})", self.now())
    }
}

impl Clock {
    pub fn system() -> Clock {
        Clock(Arc::new(crate::ops::now_ms))
    }

    /// A clock that reads `time`, which the caller moves.
    pub fn manual(time: Arc<AtomicU64>) -> Clock {
        Clock(Arc::new(move || time.load(Ordering::SeqCst)))
    }

    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

/// A trigger as stored: its spec (without the secret), who created it,
/// and its state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredTrigger {
    pub id: String,
    pub tenant: String,
    pub spec: TriggerSpec,
    /// The principal that created it: every run acts as this principal,
    /// as a worker acts as an operation's admitting principal.
    pub principal: Principal,
    pub created_at_ms: u64,
    /// Not serialized into the body: its own column.
    #[serde(skip)]
    pub secret: Option<String>,
    #[serde(skip)]
    pub enabled: bool,
    #[serde(skip)]
    pub paused_reason: Option<String>,
    #[serde(skip)]
    pub failures: u32,
    #[serde(skip)]
    pub next_due_ms: Option<u64>,
}

impl StoredTrigger {
    /// The wire form; `base_url` makes an event trigger's webhook URL.
    pub fn info(&self, base_url: &str) -> Trigger {
        let spec = &self.spec;
        Trigger {
            id: self.id.clone(),
            name: spec.name.clone(),
            repo: spec.repo.clone(),
            when: spec.when.clone(),
            conditions: spec.conditions.clone(),
            task: spec.task.clone(),
            route: spec.route.clone(),
            precheck: spec.precheck.clone(),
            policy: spec.policy.clone(),
            enabled: self.enabled,
            paused_reason: self.paused_reason.clone(),
            consecutive_failures: self.failures,
            next_due_ms: self.next_due_ms,
            webhook_url: (!spec.when.is_schedule()).then(|| webhook_url(base_url, &self.id)),
            has_secret: self.secret.is_some(),
            tenant: self.tenant.clone(),
            created_by: self.principal.name.clone(),
            created_at_ms: self.created_at_ms,
        }
    }

    /// The source of an event trigger.
    pub fn source(&self) -> Option<EventSource> {
        match self.spec.when {
            When::Event { source } => Some(source),
            _ => None,
        }
    }
}

/// `BASE/v1/triggers/ID/fire`.
pub fn webhook_url(base_url: &str, id: &str) -> String {
    format!("{}/v1/triggers/{id}/fire", base_url.trim_end_matches('/'))
}

/// A fresh ID with `prefix`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", &branchyard_client::new_key()[..20])
}

/// A fresh webhook secret: 32 random bytes as hex.
pub fn new_secret() -> String {
    format!(
        "{}{}",
        branchyard_client::new_key().replace('-', ""),
        branchyard_client::new_key().replace('-', "")
    )
}

/// The parsed schedule of `when`, or `None` for an event trigger.
pub enum Schedule {
    Cron(cron::Cron),
    Every(u64),
}

impl Schedule {
    pub fn of(when: &When) -> Result<Option<Schedule>, String> {
        match when {
            When::Cron { expr, timezone } => {
                Ok(Some(Schedule::Cron(cron::Cron::parse(expr, timezone)?)))
            }
            When::Interval { seconds } => Ok(Some(Schedule::Every(seconds * 1000))),
            When::Event { .. } => Ok(None),
        }
    }

    /// The first time strictly after `after_ms`; an interval counts from
    /// `anchor_ms`, its previous due time.
    pub fn next_after(&self, after_ms: u64, anchor_ms: u64) -> Option<u64> {
        match self {
            Schedule::Cron(cron) => cron.next_after(after_ms),
            Schedule::Every(every) => {
                if anchor_ms > after_ms {
                    return Some(anchor_ms);
                }
                let steps = (after_ms - anchor_ms) / every + 1;
                Some(anchor_ms + steps * every)
            }
        }
    }
}

/// The least interval a trigger may have, and the most.
pub const MIN_INTERVAL_SECONDS: u64 = 60;
pub const MAX_INTERVAL_SECONDS: u64 = 366 * 24 * 3600;

/// Whether `name` may name a trigger: 1 to 63 of `a-z`, `0-9`, `.`, `_`
/// and `-`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    crate::store::valid_label(name)
}

/// Check a new trigger's spec, as far as it can be checked without the
/// server: names, the schedule, conditions against the source, the
/// templates, the route's kind, the precheck and the policy. Returns the
/// spec with its precheck normalized.
pub fn validate(mut spec: TriggerSpec) -> Result<TriggerSpec, String> {
    if !valid_name(&spec.name) {
        return Err(format!(
            "{:?} is not a trigger name (1 to 63 of a-z, 0-9, '.', '_' and '-', starting with \
             a letter or digit)",
            spec.name
        ));
    }
    if spec.repo.trim().is_empty() {
        return Err("repo is empty".into());
    }
    if let When::Event { source } = &spec.when {
        if source.is_email() && spec.conditions.sender.is_empty() {
            return Err(format!(
                "an email trigger needs a sender allowlist: conditions.sender (--if \
                 sender=alice@example.com or --if sender=@example.com); without one, anyone \
                 who can send {} mail could start a task",
                source.as_str()
            ));
        }
    }
    let event = match &spec.when {
        When::Cron { .. } => {
            Schedule::of(&spec.when)?;
            false
        }
        When::Interval { seconds } => {
            if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(seconds) {
                return Err(format!(
                    "an interval is {MIN_INTERVAL_SECONDS} seconds to a year; {seconds} is not"
                ));
            }
            false
        }
        When::Event { .. } => true,
    };
    check_conditions(&spec.conditions, &spec.when)?;
    let task = &spec.task;
    if task.prompt.trim().is_empty() {
        return Err("the task's prompt is empty".into());
    }
    template::check(&task.prompt, event, "the task's prompt")?;
    if let Some(name) = &task.name {
        template::check(name, event, "the task's name")?;
    }
    if task.harness.is_some() && !task.harnesses.is_empty() {
        return Err("give the task a harness or harnesses, not both".into());
    }
    if let Some(route) = &spec.route {
        if task.harness.is_some() || !task.harnesses.is_empty() {
            return Err("a routed trigger names no harness: the fleet table picks one".into());
        }
        if let Some(kind) = &route.kind {
            kind.parse::<branchyard::TaskKind>()
                .map_err(|e| format!("route.kind: {e}"))?;
        }
    }
    if let Some(precheck) = &spec.precheck {
        if precheck.timeout_seconds > precheck::MAX_TIMEOUT_SECONDS {
            return Err(format!(
                "precheck.timeout_seconds is at most {}",
                precheck::MAX_TIMEOUT_SECONDS
            ));
        }
        spec.precheck =
            Some(precheck::normalize(precheck).ok_or("the precheck's command is empty")?);
    }
    let window = spec.policy.replay_window_seconds;
    if !(1..=86_400).contains(&window) {
        return Err("policy.replay_window_seconds is 1 to 86400".into());
    }
    if spec.policy.catch_up_seconds > 7 * 86_400 {
        return Err("policy.catch_up_seconds is at most a week (604800)".into());
    }
    match (&spec.secret, event) {
        (Some(_), false) => return Err("a schedule takes no webhook secret".into()),
        (Some(secret), true) if secret.trim().is_empty() || secret.contains(['\r', '\n']) => {
            return Err("the webhook secret is empty or spans lines".into())
        }
        _ => {}
    }
    Ok(spec)
}

fn check_conditions(conditions: &Conditions, when: &When) -> Result<(), String> {
    if conditions.is_empty() {
        return Ok(());
    }
    let When::Event { source } = when else {
        return Err("conditions match events; a schedule has none".into());
    };
    let fields = [
        ("kind", &conditions.kind),
        ("repo", &conditions.repo),
        ("label", &conditions.label),
        ("author", &conditions.author),
        ("branch", &conditions.branch),
        ("text_contains", &conditions.text_contains),
        ("sender", &conditions.sender),
        ("recipient", &conditions.recipient),
        ("subject_contains", &conditions.subject_contains),
    ];
    for (field, values) in fields {
        if let Some(blank) = values.iter().find(|v| v.trim().is_empty()) {
            return Err(format!("conditions.{field} has an empty value {blank:?}"));
        }
    }
    for kind in &conditions.kind {
        if !events::known_kind(*source, kind) {
            return Err(format!(
                "conditions.kind: {kind:?} is not an event {} sends triggers (see \
                 docs/triggers.md)",
                source.as_str()
            ));
        }
    }
    if *source == EventSource::Slack && !conditions.branch.is_empty() {
        return Err("conditions.branch: Slack events have no branch".into());
    }
    let email_only = [
        ("sender", &conditions.sender),
        ("recipient", &conditions.recipient),
        ("subject_contains", &conditions.subject_contains),
    ];
    match source.is_email() {
        false => {
            if let Some((field, _)) = email_only.iter().find(|(_, v)| !v.is_empty()) {
                return Err(format!(
                    "conditions.{field} matches emails; {} sends none",
                    source.as_str()
                ));
            }
        }
        true => {
            for (field, values) in [
                ("sender", &conditions.sender),
                ("recipient", &conditions.recipient),
            ] {
                if let Some(bad) = values.iter().find(|v| !email::valid_entry(v)) {
                    return Err(format!(
                        "conditions.{field}: {bad:?} is neither an address nor a @domain"
                    ));
                }
            }
            if !conditions.branch.is_empty() {
                return Err("conditions.branch: an email has no branch".into());
            }
        }
    }
    Ok(())
}

/// The sender allowlist of an email trigger, checked before a delivery is
/// recorded: `Ok` for any other source.
pub fn sender_allowed(conditions: &Conditions, event: &TriggerEvent) -> Result<(), String> {
    let Some(message) = &event.email else {
        return Ok(());
    };
    match email::allowed(&conditions.sender, &message.from) {
        true => Ok(()),
        false => Err(format!(
            "sender {} is not on this trigger's allowlist",
            message.from
        )),
    }
}

/// `Ok` when `event` matches every condition, else which one it missed.
pub fn matches(conditions: &Conditions, event: &TriggerEvent) -> Result<(), String> {
    let lower = |s: &str| s.to_lowercase();
    if !conditions.kind.is_empty()
        && !conditions.kind.iter().any(|k| match k.strip_suffix('*') {
            Some(prefix) => event.kind.starts_with(prefix),
            None => &event.kind == k,
        })
    {
        return Err(format!(
            "kind {} is not {}",
            event.kind,
            conditions.kind.join(" or ")
        ));
    }
    let one_of = |field: &str, wanted: &[String], have: Option<&str>| -> Result<(), String> {
        if wanted.is_empty() {
            return Ok(());
        }
        match have {
            Some(v) if wanted.iter().any(|w| lower(w) == lower(v)) => Ok(()),
            Some(v) => Err(format!("{field} {v} is not {}", wanted.join(" or "))),
            None => Err(format!("the event has no {field}")),
        }
    };
    one_of("repo", &conditions.repo, event.repo.as_deref())?;
    one_of("author", &conditions.author, event.author.as_deref())?;
    if !conditions.branch.is_empty()
        && !event
            .branch
            .as_deref()
            .is_some_and(|b| conditions.branch.iter().any(|w| w == b))
    {
        return Err(format!(
            "branch {} is not {}",
            event.branch.as_deref().unwrap_or("(none)"),
            conditions.branch.join(" or ")
        ));
    }
    if !conditions.label.is_empty()
        && !event
            .labels
            .iter()
            .any(|l| conditions.label.iter().any(|w| lower(w) == lower(l)))
    {
        return Err(format!(
            "no label {} (it has {})",
            conditions.label.join(" or "),
            match event.labels.is_empty() {
                true => "none".to_owned(),
                false => event.labels.join(", "),
            }
        ));
    }
    if event.email.is_some() || !conditions.sender.is_empty() {
        sender_allowed(conditions, event)?;
        if !conditions.sender.is_empty() && event.email.is_none() {
            return Err("the event is not an email".into());
        }
    }
    if !conditions.recipient.is_empty() {
        let to = event
            .email
            .as_ref()
            .map(email::recipients)
            .unwrap_or_default();
        if !to.iter().any(|a| email::allowed(&conditions.recipient, a)) {
            return Err(format!(
                "no recipient {} (it went to {})",
                conditions.recipient.join(" or "),
                match to.is_empty() {
                    true => "nobody it names".to_owned(),
                    false => to.join(", "),
                }
            ));
        }
    }
    if !conditions.subject_contains.is_empty() {
        let subject = lower(event.title.as_deref().unwrap_or(""));
        if !conditions
            .subject_contains
            .iter()
            .any(|w| subject.contains(&lower(w)))
        {
            return Err(format!(
                "its subject does not contain {}",
                conditions.subject_contains.join(" or ")
            ));
        }
    }
    if !conditions.text_contains.is_empty() {
        let haystack = lower(&format!(
            "{}\n{}",
            event.title.as_deref().unwrap_or(""),
            event.text.as_deref().unwrap_or("")
        ));
        if !conditions
            .text_contains
            .iter()
            .any(|w| haystack.contains(&lower(w)))
        {
            return Err(format!(
                "its text does not contain {}",
                conditions.text_contains.join(" or ")
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_client::api::TaskRequest;
    use branchyard_client::triggers::{Precheck, RouteSpec, TriggerPolicy};

    fn spec(when: When) -> TriggerSpec {
        TriggerSpec {
            name: "nightly".into(),
            repo: "app".into(),
            when,
            conditions: Conditions::default(),
            task: TaskRequest {
                prompt: "Update dependencies".into(),
                ..TaskRequest::default()
            },
            route: None,
            precheck: None,
            enabled: true,
            policy: TriggerPolicy::default(),
            secret: None,
        }
    }

    fn github() -> When {
        When::Event {
            source: EventSource::Github,
        }
    }

    #[test]
    fn validation_refuses_what_could_never_fire_well() {
        let cron = When::Cron {
            expr: "0 3 * * *".into(),
            timezone: "UTC".into(),
        };
        assert!(validate(spec(cron.clone())).is_ok());
        let mut s = spec(cron.clone());
        s.name = "Bad Name".into();
        assert!(validate(s).unwrap_err().contains("trigger name"));
        let s = spec(When::Cron {
            expr: "0 3 * *".into(),
            timezone: "UTC".into(),
        });
        assert!(validate(s).unwrap_err().contains("five"));
        let s = spec(When::Interval { seconds: 5 });
        assert!(validate(s).unwrap_err().contains("interval"));
        let mut s = spec(cron.clone());
        s.task.prompt = "Fix {{event.title}}".into();
        assert!(validate(s).unwrap_err().contains("needs an event"));
        let mut s = spec(cron.clone());
        s.conditions.label = vec!["agent".into()];
        assert!(validate(s).unwrap_err().contains("schedule has none"));
        let mut s = spec(github());
        s.conditions.kind = vec!["push".into()];
        assert!(validate(s).unwrap_err().contains("conditions.kind"));
        let mut s = spec(github());
        s.conditions.label = vec![" ".into()];
        assert!(validate(s).unwrap_err().contains("empty value"));
        let mut s = spec(github());
        s.route = Some(RouteSpec {
            kind: Some("chores".into()),
        });
        assert!(validate(s).unwrap_err().contains("route.kind"));
        let mut s = spec(github());
        s.route = Some(RouteSpec::default());
        s.task.harness = Some("codex".into());
        assert!(validate(s).unwrap_err().contains("names no harness"));
        let mut s = spec(github());
        s.precheck = Some(Precheck {
            command: "   ".into(),
            timeout_seconds: 60,
        });
        assert!(validate(s).unwrap_err().contains("precheck"));
        let mut s = spec(cron);
        s.secret = Some("x".into());
        assert!(validate(s).unwrap_err().contains("no webhook secret"));
        let mut s = spec(github());
        s.precheck = Some(Precheck {
            command: " make check ".into(),
            timeout_seconds: 0,
        });
        assert_eq!(
            validate(s).unwrap().precheck,
            Some(Precheck {
                command: "make check".into(),
                timeout_seconds: 1
            })
        );
    }

    #[test]
    fn email_triggers_need_a_sender_allowlist_and_email_fields_need_email() {
        let postmark = When::Event {
            source: EventSource::Postmark,
        };
        let err = validate(spec(postmark.clone())).unwrap_err();
        assert!(err.contains("sender allowlist"), "{err}");
        let mut s = spec(postmark.clone());
        s.conditions.sender = vec!["alice@example.com".into(), "@corp.example".into()];
        s.conditions.recipient = vec!["@by.example".into()];
        s.conditions.subject_contains = vec!["deploy".into()];
        s.conditions.kind = vec!["email.received".into()];
        assert!(validate(s.clone()).is_ok());
        s.conditions.sender.push("not an address".into());
        assert!(validate(s)
            .unwrap_err()
            .contains("neither an address nor a @domain"));
        let mut s = spec(postmark);
        s.conditions.sender = vec!["@corp.example".into()];
        s.conditions.kind = vec!["issues.opened".into()];
        assert!(validate(s).unwrap_err().contains("conditions.kind"));
        let mut s = spec(github());
        s.conditions.sender = vec!["@corp.example".into()];
        assert!(validate(s).unwrap_err().contains("matches emails"));
    }

    #[test]
    fn email_conditions_match_sender_recipient_and_subject() {
        use branchyard_client::triggers::EmailMessage;
        let e = TriggerEvent {
            source: "mailgun".into(),
            kind: "email.received".into(),
            id: "m1".into(),
            author: Some("bob@corp.example".into()),
            title: Some("Deploy FAILED".into()),
            email: Some(EmailMessage {
                from: "bob@corp.example".into(),
                to: vec!["ops@by.example".into()],
                envelope_to: vec!["agent@by.example".into()],
                subject: "Deploy FAILED".into(),
                ..EmailMessage::default()
            }),
            ..TriggerEvent::default()
        };
        let mut c = Conditions {
            sender: vec!["@corp.example".into()],
            recipient: vec!["agent@by.example".into()],
            subject_contains: vec!["failed".into()],
            ..Conditions::default()
        };
        assert_eq!(matches(&c, &e), Ok(()));
        assert_eq!(sender_allowed(&c, &e), Ok(()));
        c.recipient = vec!["@elsewhere.example".into()];
        assert!(matches(&c, &e).unwrap_err().contains("no recipient"));
        c.recipient.clear();
        c.subject_contains = vec!["succeeded".into()];
        assert!(matches(&c, &e).unwrap_err().contains("subject"));
        c.subject_contains.clear();
        c.sender = vec!["alice@corp.example".into()];
        assert!(sender_allowed(&c, &e)
            .unwrap_err()
            .contains("not on this trigger's allowlist"));
        assert!(matches(&c, &e).is_err());
    }

    #[test]
    fn conditions_match_every_field_given() {
        let e = TriggerEvent {
            source: "github".into(),
            kind: "issue_comment.created".into(),
            id: "1".into(),
            repo: Some("Acme/App".into()),
            author: Some("alice".into()),
            title: Some("Crash".into()),
            text: Some("Hey @Branchyard, please look".into()),
            labels: vec!["bug".into()],
            ..TriggerEvent::default()
        };
        let mut c = Conditions {
            kind: vec!["issue_comment.*".into()],
            repo: vec!["acme/app".into()],
            label: vec!["BUG".into()],
            author: vec!["Alice".into(), "bob".into()],
            text_contains: vec!["@branchyard".into()],
            ..Conditions::default()
        };
        assert_eq!(matches(&c, &e), Ok(()));
        c.text_contains = vec!["@someone-else".into()];
        assert!(matches(&c, &e).unwrap_err().contains("does not contain"));
        c.text_contains.clear();
        c.label = vec!["agent".into()];
        assert!(matches(&c, &e).unwrap_err().contains("no label agent"));
        c.label.clear();
        c.branch = vec!["main".into()];
        assert!(matches(&c, &e).unwrap_err().contains("branch"));
        c.branch.clear();
        c.kind = vec!["issues.opened".into()];
        assert!(matches(&c, &e).unwrap_err().contains("kind"));
    }

    #[test]
    fn intervals_count_from_their_anchor() {
        let every = Schedule::Every(60_000);
        assert_eq!(every.next_after(1_000, 0), Some(60_000));
        assert_eq!(every.next_after(60_000, 0), Some(120_000));
        assert_eq!(every.next_after(10, 500), Some(500));
    }
}
