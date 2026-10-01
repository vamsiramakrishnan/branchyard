//! Triggers and schedules over HTTP: wire types and client methods; see
//! `docs/triggers.md`. Kept in its own module, like [`crate::storage_api`],
//! so this feature merges easily alongside unrelated work on the crate.
//!
//! A trigger is a durable server object that creates an ordinary task
//! when it fires: on a cron schedule, at an interval, or when a signed
//! webhook from GitHub, Slack, Linear or any JSON sender arrives at its
//! own URL. Its task is a [`TaskRequest`] whose prompt (and branch name)
//! may hold `{{event.*}}` placeholders.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::TaskRequest;
use crate::{encode, Client, Error};

/// When a trigger fires.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum When {
    /// A five-field cron expression (minute, hour, day of month, month,
    /// day of week), or `@hourly`, `@daily`, `@weekly`, `@monthly`,
    /// `@yearly`, read in `timezone` (an IANA name; default `UTC`).
    Cron {
        expr: String,
        #[serde(default = "utc", skip_serializing_if = "is_utc")]
        timezone: String,
    },
    /// Every `seconds` (at least 60), from when the trigger was created or
    /// last enabled.
    Interval { seconds: u64 },
    /// A webhook delivery to the trigger's own URL, from `source`.
    Event { source: EventSource },
}

fn utc() -> String {
    "UTC".into()
}

fn is_utc(tz: &String) -> bool {
    tz == "UTC"
}

impl When {
    pub fn is_schedule(&self) -> bool {
        !matches!(self, When::Event { .. })
    }
}

/// Who sends a trigger's webhooks, which decides how a delivery is
/// verified and read.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// `X-Hub-Signature-256`; issues, issue_comment, pull_request,
    /// check_suite.
    Github,
    /// `X-Slack-Signature` with `X-Slack-Request-Timestamp`; the Events
    /// API's `app_mention`, and its URL verification challenge.
    Slack,
    /// `Linear-Signature`, with `webhookTimestamp`; issue create and
    /// update.
    Linear,
    /// `X-Branchyard-Signature: sha256=<hex>` over any JSON object.
    Generic,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSource::Github => "github",
            EventSource::Slack => "slack",
            EventSource::Linear => "linear",
            EventSource::Generic => "generic",
        }
    }
}

impl std::str::FromStr for EventSource {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "github" => Ok(EventSource::Github),
            "slack" => Ok(EventSource::Slack),
            "linear" => Ok(EventSource::Linear),
            "generic" => Ok(EventSource::Generic),
            other => Err(format!(
                "{other:?} is not an event source; use github, slack, linear or generic"
            )),
        }
    }
}

/// Field matchers on the normalized event, checked when the trigger is
/// created. Every non-empty field must match (any one of its values);
/// empty matches anything.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conditions {
    /// The event's kind, such as `issues.labeled`, or a prefix and `*`
    /// (`pull_request.*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kind: Vec<String>,
    /// `owner/name` (GitHub), a Linear team key, compared ignoring case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo: Vec<String>,
    /// One of the event's labels, ignoring case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub label: Vec<String>,
    /// Who caused the event (GitHub login, Slack user ID, Linear name),
    /// ignoring case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub author: Vec<String>,
    /// The branch the event is about (a pull request's head, a check
    /// suite's branch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branch: Vec<String>,
    /// Text the event's title or body contains, ignoring case, such as a
    /// mention.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_contains: Vec<String>,
}

impl Conditions {
    pub fn is_empty(&self) -> bool {
        self == &Conditions::default()
    }
}

/// A shell command run in a fresh worktree of the repository before the
/// trigger fires: exit 0 fires it, anything else skips the run with the
/// reason. Runs only where the server's operator allows prechecks.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Precheck {
    pub command: String,
    /// 1 to 600; default 60.
    #[serde(default = "precheck_timeout")]
    pub timeout_seconds: u64,
}

fn precheck_timeout() -> u64 {
    60
}

/// Route the task through the repository's fleet table at fire time, as
/// `by run --auto` does (without failover).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// The task's kind (`bugfix`, `docs`, ...); classified from the
    /// rendered prompt when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// How a trigger behaves over time.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerPolicy {
    /// Disable the trigger after this many failed runs in a row; 0 never.
    #[serde(default = "pause_after")]
    pub pause_after_failures: u32,
    /// A scheduled run missed by at most this long (the server was down)
    /// fires once when the scheduler is back; older ones are recorded as
    /// missed.
    #[serde(default = "catch_up")]
    pub catch_up_seconds: u64,
    /// How old a Slack or Linear delivery's own timestamp may be.
    #[serde(default = "replay_window")]
    pub replay_window_seconds: u64,
}

fn pause_after() -> u32 {
    3
}

fn catch_up() -> u64 {
    3600
}

fn replay_window() -> u64 {
    300
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        TriggerPolicy {
            pause_after_failures: pause_after(),
            catch_up_seconds: catch_up(),
            replay_window_seconds: replay_window(),
        }
    }
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// `POST /v1/triggers`: a new trigger.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    /// 1 to 63 of `a-z`, `0-9`, `.`, `_` and `-`; unique in the tenant.
    pub name: String,
    /// A served repository.
    pub repo: String,
    pub when: When,
    #[serde(default, skip_serializing_if = "Conditions::is_empty")]
    pub conditions: Conditions,
    /// The task to create. `prompt` and `name` may hold placeholders:
    /// `{{event.title}}`, `{{event.payload.issue.number}}`,
    /// `{{trigger.name}}`, `{{scheduled_at}}`.
    pub task: TaskRequest,
    /// Route through the fleet table instead of naming a harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck: Option<Precheck>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default)]
    pub policy: TriggerPolicy,
    /// The webhook signing secret: one you choose for GitHub or a generic
    /// sender, Slack's app signing secret, Linear's webhook secret.
    /// Generated (and returned once) when an event trigger omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// A trigger as the server keeps it. Its secret is never shown again.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub when: When,
    #[serde(default, skip_serializing_if = "Conditions::is_empty")]
    pub conditions: Conditions,
    pub task: TaskRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck: Option<Precheck>,
    pub policy: TriggerPolicy,
    pub enabled: bool,
    /// Why it is disabled: by whom, or after which failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    pub consecutive_failures: u32,
    /// The next scheduled time, for an enabled schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_ms: Option<u64>,
    /// Where an event trigger's sender posts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    pub has_secret: bool,
    pub tenant: String,
    pub created_by: String,
    pub created_at_ms: u64,
}

/// `201` from `POST /v1/triggers`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerCreated {
    pub trigger: Trigger,
    /// The webhook secret, when the server generated it: shown once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// `GET /v1/triggers`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerList {
    pub triggers: Vec<Trigger>,
}

/// `POST /v1/triggers/{t}/secret`: set the webhook secret, or generate
/// one when `secret` is omitted.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// The answer to [`SecretRequest`]: the generated secret, shown once.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// `POST /v1/triggers/{t}/test`: evaluate without creating anything.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerTestRequest {
    /// A delivery body as the trigger's source would send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
    /// GitHub's `X-GitHub-Event` for `event`; inferred from its shape when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    /// Also run the precheck, where the server allows prechecks.
    #[serde(default)]
    pub run_precheck: bool,
}

/// An event from any source, in one shape.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerEvent {
    /// `github`, `slack`, `linear` or `generic`.
    pub source: String,
    /// Such as `issues.opened`, `issue_comment.created`,
    /// `pull_request.synchronize`, `check_suite.failure`, `app_mention`,
    /// `issue.create`.
    pub kind: String,
    /// The event's ID, from the signed bytes only: Slack's `event_id`, a
    /// generic body's `id`, else a hash of the body. With the trigger's,
    /// the key that keeps a redelivery from firing twice.
    pub id: String,
    /// The sender's delivery ID header (`X-GitHub-Delivery`,
    /// `Linear-Delivery`, `X-Branchyard-Event-Id`), for looking a delivery
    /// up at the sender; not signed, so not part of the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The delivery's body as sent.
    #[serde(default)]
    pub payload: Value,
}

/// What a precheck did; Orca's `AutomationPrecheckResult`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrecheckResult {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The last 4000 characters of each stream.
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PrecheckResult {
    /// Exit 0, in time, without an error.
    pub fn passed(&self) -> bool {
        !self.timed_out && self.error.is_none() && self.exit_code == Some(0)
    }
}

/// The answer to a test: what a delivery (or the schedule) would do.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerTest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<TriggerEvent>,
    /// Whether the conditions match.
    pub matched: bool,
    /// Why it would not fire, when it would not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck: Option<PrecheckResult>,
    pub would_fire: bool,
    /// The task it would create, rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskRequest>,
    /// Routed at fire time, through the fleet table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteSpec>,
    /// The idempotency key the run would use.
    pub key: String,
}

/// What became of one firing (a delivery, or a scheduled time).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Recorded, not yet fired: a worker serving the repository fires it.
    Pending,
    /// A task was admitted; `operation` names it.
    Fired,
    /// The event did not match the conditions.
    SkippedCondition,
    /// The precheck did not pass.
    SkippedPrecheck,
    /// The trigger was disabled or removed before it fired.
    SkippedDisabled,
    /// No task could be admitted; counts toward pausing.
    Failed,
    /// Scheduled times older than the catch-up window, never fired.
    Missed,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Fired => "fired",
            RunState::SkippedCondition => "skipped_condition",
            RunState::SkippedPrecheck => "skipped_precheck",
            RunState::SkippedDisabled => "skipped_disabled",
            RunState::Failed => "failed",
            RunState::Missed => "missed",
        }
    }

    pub fn parse(text: &str) -> Option<RunState> {
        [
            RunState::Pending,
            RunState::Fired,
            RunState::SkippedCondition,
            RunState::SkippedPrecheck,
            RunState::SkippedDisabled,
            RunState::Failed,
            RunState::Missed,
        ]
        .into_iter()
        .find(|s| s.as_str() == text)
    }
}

/// How a fired run's task ended.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// The operation succeeded and no branch it made failed.
    pub ok: bool,
    pub detail: String,
}

/// One firing of a trigger.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerRun {
    pub id: String,
    pub trigger: String,
    /// `event:<id>`, `schedule:<ms>` or `missed:<ms>`: unique per trigger.
    pub key: String,
    pub state: RunState,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_ms: Option<u64>,
    /// For `missed`: how many scheduled times, the last at `last_missed_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_missed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<TriggerEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck: Option<PrecheckResult>,
    /// The task's operation, once fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// How the task ended, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

/// `GET /v1/triggers/{t}/runs`, newest first.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerRuns {
    pub runs: Vec<TriggerRun>,
}

/// The answer to a webhook delivery at `POST /v1/triggers/{id}/fire`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FireAck {
    /// The run this delivery recorded, or the one it repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<TriggerRun>,
    /// A redelivery of an event already recorded: nothing new happened.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub duplicate: bool,
    /// Why nothing was recorded (a ping, an event kind no adapter reads,
    /// a disabled trigger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<String>,
}

/// `DELETE /v1/triggers/{t}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerRemoved {
    pub removed: String,
}

/// `POST /v1/triggers/{t}/enable` and `/disable`: no fields.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerToggle {}

impl Client {
    /// Create a trigger. An event trigger without a secret gets a
    /// generated one, returned here once.
    pub fn create_trigger(&self, spec: &TriggerSpec, key: &str) -> Result<TriggerCreated, Error> {
        self.post("/v1/triggers", spec, key)
    }

    /// The caller's tenant's triggers.
    pub fn triggers(&self) -> Result<Vec<Trigger>, Error> {
        Ok(self.get::<TriggerList>("/v1/triggers")?.triggers)
    }

    /// One trigger, by name or ID.
    pub fn trigger(&self, name: &str) -> Result<Trigger, Error> {
        self.get(&format!("/v1/triggers/{}", encode(name)))
    }

    pub fn remove_trigger(&self, name: &str) -> Result<TriggerRemoved, Error> {
        self.delete(&format!("/v1/triggers/{}", encode(name)))
    }

    /// Enable it again (after a pause, too): its failure count starts over,
    /// and a schedule's next time is counted from now.
    pub fn enable_trigger(&self, name: &str, key: &str) -> Result<Trigger, Error> {
        self.post(
            &format!("/v1/triggers/{}/enable", encode(name)),
            &TriggerToggle {},
            key,
        )
    }

    pub fn disable_trigger(&self, name: &str, key: &str) -> Result<Trigger, Error> {
        self.post(
            &format!("/v1/triggers/{}/disable", encode(name)),
            &TriggerToggle {},
            key,
        )
    }

    /// Set the webhook secret, or generate one with `None`.
    pub fn set_trigger_secret(
        &self,
        name: &str,
        secret: Option<String>,
        key: &str,
    ) -> Result<SecretSet, Error> {
        self.post(
            &format!("/v1/triggers/{}/secret", encode(name)),
            &SecretRequest { secret },
            key,
        )
    }

    /// What the trigger would do, creating nothing.
    pub fn test_trigger(
        &self,
        name: &str,
        request: &TriggerTestRequest,
        key: &str,
    ) -> Result<TriggerTest, Error> {
        self.post(&format!("/v1/triggers/{}/test", encode(name)), request, key)
    }

    /// Its most recent runs, newest first.
    pub fn trigger_runs(&self, name: &str, limit: usize) -> Result<Vec<TriggerRun>, Error> {
        Ok(self
            .get::<TriggerRuns>(&format!("/v1/triggers/{}/runs?limit={limit}", encode(name)))?
            .runs)
    }
}
