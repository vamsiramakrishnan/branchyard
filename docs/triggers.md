# Triggers and schedules

A **trigger** starts an ordinary Branchyard task on its own: on a cron schedule, at an interval, or when a signed webhook from GitHub, Slack, Linear or any JSON sender arrives. It is a durable object in the server's store, beside the operation registry, and every firing goes through the same admission path as `POST /v1/repos/{repo}/tasks`: quotas, branch locks, worker labels and an idempotency key, so a redelivered webhook or a restarted scheduler never starts a second task. This is the *triggers and schedules* track of the [roadmap](roadmap.md#wave-2), learned from Manus automations (validated conditions, test runs, pausing after repeated failures), Devin automations, Claude Code routines and Cursor's and Jules's scheduled tasks; the precheck is ported from Orca.

> **Status.** Implemented and tested hermetically: the cron parser, every adapter's signature check and normalization, the store's conformance suite on SQLite and PostgreSQL 16, two to four dispatchers racing for one schedule time, a dispatcher that dies mid-fire, the HTTP API over loopback with the fake ACP agent and a manual clock, and `by trigger` locally, through `by serve`, and with `--remote`. **Nothing has received a delivery from the real GitHub, Slack or Linear**: the payloads are built from their documentation, and the signatures from their documented schemes. Email triggers are not done.

## Where triggers fire

**Triggers fire only where a dispatcher runs**: a `by serve` or `branchyard-server` process, or a `by worker`, whose store holds the trigger and which serves its repository. Each has a trigger dispatcher beside its operation dispatcher. Nothing fires while none runs; a schedule time missed meanwhile fires once when one starts, if it is within the trigger's catch-up window ([missed times](#missed-times)).

- **Local mode.** `by trigger …` without `--remote` reads and writes the store of the server this repository would run: `by serve`'s data directory (`.branchyard/server/state.db` by default) or its `--database`, as `branchyard.toml`'s `[serve] config` says. Add triggers with `by trigger add`, then keep `by serve` (or `by worker --database …`) running; webhooks need the server's listener, which `by worker` does not have.
- **Remote mode.** `by --remote URL trigger …` and the HTTP API act as the token's principal: the trigger belongs to its tenant, and every run acts as that principal, the way a worker acts as an operation's admitting principal.

## By example

```sh
# Every weekday at 03:00 in Berlin: a branch per night, its check run before merging.
by trigger add nightly --cron '0 3 * * 1-5' --tz Europe/Berlin \
    --prompt 'Update the dependencies and fix what breaks' \
    --harness codex --check 'cargo test' --budget-usd 3 --yes

# An issue labeled `agent` on GitHub: one branch per issue, routed by the fleet table.
by trigger add triage --on github --if kind=issues.labeled --if label=agent \
    --prompt 'Resolve GitHub issue #{{event.number}}: {{event.title}}\n{{event.url}}\n\n{{event.text}}' \
    --branch-name 'issue-{{event.number}}' --auto --yes
# created trigger triage (trg_…)
# webhook URL: https://by.example.com/v1/triggers/trg_…/fire
# webhook secret (shown once; give it to the sender): …

by trigger test triage --event issue.json     # conditions, precheck and the rendered task; creates nothing
by trigger list                               # name, schedule or source, repository, state, next time or URL
by trigger runs triage                        # fired (branch, operation, outcome), skipped and why, failed, missed
by trigger disable triage; by trigger enable triage
by trigger secret triage --secret-file slack-signing-secret.txt
by trigger rm triage
```

| Command | Does |
|---|---|
| `by trigger add NAME (--cron EXPR [--tz ZONE] \| --every DURATION \| --on SOURCE) --prompt TEXT` | Create a trigger. Task: `--harness H` or `--auto [--kind K]`, `--branch-name TEMPLATE`, `--base REF`, `--check CMD`, `--budget-usd X`, `--max-turns N`, `--max-minutes N`, `--connector GRANT` (repeatable), `--require-label L` (repeatable), `--yes` (allow every tool request; otherwise each is denied). When: `--if FIELD=VALUE` (repeatable), `--precheck CMD [--precheck-timeout SECS]`. Policy: `--pause-after N`, `--catch-up DURATION`, `--secret-file FILE`, `--disabled` |
| `by trigger list` (`ls`) | Every trigger |
| `by trigger show NAME` | One in full |
| `by trigger test NAME [--event FILE] [--event-type TYPE] [--precheck]` | What it would do, creating nothing ([test runs](#test-runs)) |
| `by trigger enable NAME` / `disable NAME` | Start or stop it firing; enabling resets its failure count and counts a schedule's next time from now |
| `by trigger rm NAME` | Remove it and its runs |
| `by trigger runs NAME [--limit N]` | Its runs, newest first (default 20) |
| `by trigger secret NAME [--secret-file FILE]` | Set the webhook secret from a file, or generate and print one |

Every action takes `--json`, which prints the wire types below. Durations are `90s`, `30m`, `2h`, `1d` or seconds. A trigger name is 1 to 63 of `a-z`, `0-9`, `.`, `_` and `-`, starting with a letter or digit, unique in its tenant. Locally a trigger is created in the `default` tenant, by `<user> (local)`.

## When a trigger fires

| `when` | Fires | Notes |
|---|---|---|
| `{"kind": "cron", "expr": "0 3 * * 1-5", "timezone": "Europe/Berlin"}` | At each matching minute in the time zone | `timezone` defaults to `UTC`; any IANA name the host's time-zone database knows |
| `{"kind": "interval", "seconds": 3600}` | Every `seconds`, 60 to a year | Counted from creation, or from when it was last enabled |
| `{"kind": "event", "source": "github"}` | On each delivery to its webhook URL | `github`, `slack`, `linear` or `generic` |

**Cron syntax.** Five fields, minute (0–59), hour (0–23), day of month (1–31), month (1–12 or `jan`–`dec`) and day of week (0–7 or `sun`–`sat`, 0 and 7 both Sunday); each `*`, a number, a range `a-b`, a step `*/n`, `a-b/n` or `a/n`, or a list of those. `@hourly`, `@daily` (`@midnight`), `@weekly`, `@monthly` and `@yearly` (`@annually`) are shorthands. When both day fields are restricted, a day matching either fires (Vixie cron's rule: `0 0 13 * fri` is every 13th and every Friday). Seconds, `L`, `W`, `#` and `?` are refused, and so is an expression that never matches a date (`0 0 30 2 *`). No cron crate was in `Cargo.lock`, so the parser is [our own](../crates/branchyard-server/src/triggers/cron.rs), with tests for steps, lists, names, leap days and daylight saving; time zones come from [`jiff`](https://docs.rs/jiff), already locked.

**Daylight saving.** Times are civil times in the trigger's zone. A time that does not exist because clocks moved forward fires as far past the gap's start as it was meant to be (02:30 on a night that jumps from 02:00 to 03:00 fires at 03:30); a time that happens twice because clocks moved back fires once, at its first occurrence.

### Missed times

A schedule's next time is a column of the trigger, `next_due_ms`. Each tick, every dispatcher looks for enabled schedules of the repositories it serves that are due by its clock, and claims each by moving `next_due_ms` from the value it read to the next time after now, recording the run in the same transaction ([exactly once](#exactly-once)). When it was down for a while:

- the **latest** scheduled time fires once, if it is within the catch-up window (`policy.catch_up_seconds`, default 3600, never less than 60);
- any **earlier** times within the window fire with it, as that one run;
- times **older** than the window are recorded together as one `missed` run with their count and the first and last of them, and never fire.

## Events

Each event trigger has its own URL, `<public_url>/v1/triggers/<id>/fire`, and its own secret. The endpoint takes no bearer token: the signature is the authentication. A delivery is verified, read into one **normalized event**, matched against the trigger's conditions, and recorded as a run keyed by the event's own ID.

| Source | Signature (HMAC-SHA256, hex, constant-time compare) | Replay window | Event ID | Kinds read |
|---|---|---|---|---|
| GitHub | `X-Hub-Signature-256: sha256=HMAC(secret, body)` | none: GitHub signs no timestamp | a hash of the body | `issues.<action>` (`opened`, `labeled`, …), `issue_comment.<action>`, `pull_request.<action>`, `check_suite.<conclusion>` (`check_suite.failure`) on completion; `ping` is answered and ignored |
| Slack | `X-Slack-Signature: v0=HMAC(secret, "v0:" + X-Slack-Request-Timestamp + ":" + body)` | the signed timestamp, ±`replay_window_seconds` (default 300) | `event_id` | `app_mention`; `url_verification` is answered with its `challenge` |
| Linear | `Linear-Signature: HMAC(secret, body)` | the body's `webhookTimestamp`, ±`replay_window_seconds` | a hash of the body | `issue.create`, `issue.update`, `issue.remove` |
| Generic | `X-Branchyard-Signature: sha256=HMAC(secret, body)` (the scheme of Branchyard's own [outgoing webhooks](server.md#webhooks)) | none | the body's `id`, else a hash of the body | the body's `kind`, default `event` |

**An event's ID comes only from the signed bytes.** A delivery ID header (`X-GitHub-Delivery`, `Linear-Delivery`, `X-Branchyard-Event-Id`) is not covered by the signature, so it is recorded as the event's `delivery`, for finding the delivery at its sender, but never used as the key: a captured delivery replayed under a new header is the same event, and a sender's own redelivery of the same body is too.

The normalized event has `source`, `kind`, `id`, and where the source says: `repo` (GitHub `owner/name`, Linear team key, Slack team ID), `author` (GitHub login, Slack user ID, Linear actor name), `title`, `text` (issue or pull request body, comment, mention text, Linear description), `url`, `number` (issue or pull request number, Linear identifier such as `ENG-12`), `branch` (pull request head, check suite branch), `labels`, `channel` (Slack), and `payload`, the body as sent. A generic sender sets any of them as top-level fields. GitHub's `application/x-www-form-urlencoded` content type (`payload=…`) is read too.

Answers: `202` with the pending run; `200` with a run skipped by condition, with `"duplicate": true` and the original run for a redelivery, or with `ignored` and why (a ping, an event kind no adapter reads, a disabled trigger); `200 {"challenge": …}` for Slack's URL verification; `401 invalid_signature` or `401 stale_delivery`; `404 unknown_trigger` for an unknown ID or a schedule's (the same answer, so IDs cannot be probed); `413` over `max_body_bytes`.

### Conditions

Field matchers on the normalized event, every given field must match (any one of its values), checked when the trigger is created:

| Field | `--if` | Matches |
|---|---|---|
| `kind` | `kind=issues.labeled`, `kind=pull_request.*` | The kind, exactly or by a prefix ending in `*`; refused at creation unless the source sends it |
| `repo` | `repo=acme/app` | Ignoring case |
| `label` | `label=agent` | One of the event's labels, ignoring case |
| `author` | `author=alice` | Ignoring case |
| `branch` | `branch=main` | Exactly; refused for Slack |
| `text_contains` | `text=@branchyard` | The title or text contains it, ignoring case: a mention |

A schedule takes no conditions. A delivery that does not match is recorded as a `skipped_condition` run saying which field missed (`no label agent (it has bug)`), so `by trigger runs` shows why nothing happened.

## The task

A trigger's `task` is a [`TaskRequest`](server.md#requests): `prompt`, `harness` or `harnesses`, `name`, `base`, `budget`, `policy`, `check`, `provision` (secrets by name, `model`, `effort`, `connectors`), `require_labels`, `provider`, and the opt-ins, each held to the server's rules when it fires, and also when the trigger is created through the API (`by trigger add` without `--remote` writes the store directly, so a refusal shows as the first run's `failed`). Its `prompt` and `name` may hold placeholders:

| Placeholder | Value |
|---|---|
| `{{event.title}}`, `text`, `url`, `number`, `repo`, `author`, `branch`, `channel`, `kind`, `id`, `source` | The event's field, empty when it has none |
| `{{event.labels}}` | Its labels, comma-separated |
| `{{event.payload.issue.user.login}}` | Any value of the body by path; a number indexes an array (`assignees.0.login`) |
| `{{trigger.name}}`, `{{trigger.id}}`, `{{trigger.repo}}`, `{{run.id}}` | The trigger's and the run's |
| `{{scheduled_at}}` | The scheduled time, or the arrival time for an event, RFC 3339 in UTC |

An unknown placeholder, an unclosed `{{`, or an `event.*` one on a schedule is refused at creation. The branch name is the rendered `name`, reduced to `a-z`, `0-9` and `-`; without one it is `<trigger>-<number>-<6 hex of the event ID>` for an event with a number, `<trigger>-<8 hex>` for one without, and `<trigger>-<yyyymmdd>-<hhmm>` (UTC) for a schedule. A name the repository already has makes that run fail (`branch_exists`), which counts toward [pausing](#failures-and-pausing). For the issue-prompt shape `by run --issue` uses, see [pull requests](pull-requests.md#starting-from-an-issue).

**Routing.** `"route": {"kind": "bugfix"}` (`--auto [--kind K]`) names no harness: when the trigger fires, the server routes the rendered prompt through the repository's `[fleet]` table in `branchyard.toml` as `by run --auto` does ([fleet](fleet.md)), seeded from the run's key so a run fired again picks the same, and sets the chosen harness, model and effort on the task. A candidate's `command` is not used (the server's `harness_commands` are). There is no failover: the server does not fail a routed branch over to the next candidate, as it does not for any remote task.

## Prechecks

`"precheck": {"command": "…", "timeout_seconds": 60}` (`--precheck CMD`) runs a shell command before the trigger fires, in a **fresh detached worktree** of the repository at the task's `base` (default `HEAD`), removed afterwards. Ported from Orca's automation precheck ([`precheck-runner.ts`, `automation-precheck.ts`](../vendor/orca/src/main/automations/precheck-runner.ts)):

- **Exit 0 fires** the trigger. Any other exit, a timeout, or a command that cannot start **skips** the run (`skipped_precheck`), with Orca's reason: `Precheck exited with code 1.`, `Precheck timed out after 60s.`, `Precheck failed: …`. A skip is not a failure.
- The timeout is 1 to 600 seconds (default 60). On a timeout the command's whole process group gets SIGTERM, then SIGKILL two seconds later.
- The last 4,000 characters of standard output and error are recorded with the run, with whether they were cut.
- The command runs as the server's user with its environment, plus `BRANCHYARD_TRIGGER` (the trigger's name), `BRANCHYARD_TRIGGER_RUN` (the run's key) and, for an event, `BRANCHYARD_TRIGGER_EVENT`: a file holding the normalized event as JSON.

**Trust.** A precheck is a command the server runs, so, like a repository's [`[workspace]` scripts](workspace.md#trust), it runs only where the server's operator allows it: `allow_trigger_prechecks` (`true`, or a list of served repositories) in the configuration, or `--allow-trigger-prechecks` on `by serve`. A trigger with a precheck is refused when created on a server that does not allow it (`403 precheck_not_allowed`; locally, `by trigger add` says so), and a run whose server stopped allowing it fails rather than skipping the check. `by trigger test --precheck` runs it at your terminal, as your own decision.

## Test runs

`by trigger test NAME [--event FILE]` (`POST /v1/triggers/{t}/test`) evaluates without creating anything: it reads `--event` as the trigger's source would send it (GitHub's event type from `--event-type`, else from the body's shape), checks the conditions, renders the task, and with `--precheck` runs the precheck. A schedule is rendered for its next time. It answers whether the run **would fire**, why not, the rendered task (prompt and branch name), the run key it would use, and the precheck's result.

## Runs, outcomes and failures

Every firing is a **run**: a delivery, a scheduled time, or a block of missed times.

| State | Meaning |
|---|---|
| `pending` | Recorded, not fired yet; a dispatcher serving the repository fires it |
| `fired` | A task was admitted: `operation` and `branches` name it. Once the operation ends, its `outcome` is recorded: `ok` when it succeeded and none of its branches ended `failed` or `interrupted` |
| `skipped_condition` | The event did not match |
| `skipped_precheck` | The precheck did not pass |
| `skipped_disabled` | The trigger was disabled or removed between recording and firing |
| `failed` | No task could be admitted: a quota, a branch that exists, a refused option, a repository not served, a routing error |
| `missed` | Scheduled times older than the catch-up window |

A run's `key` is unique per trigger: `event:<event id>`, `schedule:<ms>` or `missed:<ms>`. (A test's key for GitHub, Linear or an ID-less generic body hashes the event file as re-serialized, which may differ from the bytes the sender will send.)

### Failures and pausing

A run that **failed**, or a fired run whose task's **outcome** was not ok, counts as a failure; an ok outcome starts the count over; skips and missed times count neither way. After `policy.pause_after_failures` failures in a row (`--pause-after`, default 3; 0 never) the trigger **disables itself** in the same transaction that recorded the last one, with the reason: `paused after 3 failed runs in a row; the last: branch_exists: …`. A delivery to a disabled trigger is answered `ignored`, and a schedule stops. `by trigger enable` resumes it with the count at zero.

## Exactly once

- **Schedules.** A claim is a compare-and-set on `next_due_ms` (`UPDATE … WHERE id = … AND enabled AND next_due_ms = <what was read>`) in the transaction that records the run, so of any number of dispatchers on one store, one claims each time. Tested with four dispatchers ticking at once on SQLite, four handles racing on PostgreSQL, and two servers on one PostgreSQL database.
- **Deliveries.** A run's key is unique per trigger, so a redelivered webhook finds the run its first delivery recorded and nothing new happens.
- **Firing.** A claimed run is held by its dispatcher for 11 minutes (the longest precheck and a margin) under a fence, its attempt number; recording its outcome needs the fence. If the dispatcher dies, another fires the run once the hold expires, and the task's idempotency key — `trigger:<tenant>/<trigger id>` and the run's key, in the operation registry's unique index — makes the second admission return the first's operation instead of starting another. Before admitting, the dispatcher also looks the key up, so a run re-rendered differently (a routed run, a trigger edited in between) still maps to its first task.
- **Stopping.** A run that cannot be admitted because its server is shutting down is not a failure: it stays pending, held 30 seconds, after which any dispatcher fires it. A server on a data directory releases its predecessor's holds when it starts, so a run left by a crash or a restart fires at once.
- **Clocks.** Due times and holds use the dispatchers' clocks, not the database's: keep hosts on NTP. Tests inject a clock and never depend on wall-clock timing.

## Through the API

| Method and path | Does | Needs | Returns |
|---|---|---|---|
| `POST /v1/triggers` | Create one from a `TriggerSpec` | `run` on its repository | `201` `TriggerCreated` (the generated secret, once) |
| `GET /v1/triggers` | The tenant's triggers on repositories the caller may see | `read` | `TriggerList` |
| `GET /v1/triggers/{t}` | One, by name or ID | `read` | `Trigger` |
| `DELETE /v1/triggers/{t}` | Remove it and its runs | `run` | `TriggerRemoved` |
| `POST /v1/triggers/{t}/enable`, `/disable` | `{}` | `run` | `Trigger` |
| `POST /v1/triggers/{t}/secret` | `{"secret": "…"}`, or `{}` to generate one | `run` | `SecretSet` |
| `POST /v1/triggers/{t}/test` | `TriggerTestRequest` | `run` | `TriggerTest` |
| `GET /v1/triggers/{t}/runs?limit=N` | Newest first; default 20, at most 200 | `read` | `TriggerRuns` |
| `POST /v1/triggers/{id}/fire` | A webhook delivery; **no bearer token**, the signature instead | — | `FireAck` |

Another tenant's trigger, or one on a repository the caller may not see, is `404 unknown_trigger`. A second trigger of one name in a tenant is `409 trigger_exists`. A spec the server refuses is `400 invalid_request` with the reason, or the task's own refusals (`403 provider_not_allowed`, `secret_not_allowed`, …). `branchyard-client` has a method per row (`Client::create_trigger`, `triggers`, `trigger`, `remove_trigger`, `enable_trigger`, `disable_trigger`, `set_trigger_secret`, `test_trigger`, `trigger_runs`); the types are in [`schema/contract.json`](../schema/contract.json).

```json
POST /v1/triggers
{
  "name": "triage",
  "repo": "app",
  "when": { "kind": "event", "source": "github" },
  "conditions": { "kind": ["issues.labeled"], "label": ["agent"] },
  "task": {
    "prompt": "Resolve GitHub issue #{{event.number}}: {{event.title}}\n{{event.url}}\n\n{{event.text}}",
    "name": "issue-{{event.number}}",
    "harness": "codex",
    "budget": { "max_usd": 2.0 },
    "policy": { "mode": "allow" },
    "check": ["cargo", "test"],
    "provision": { "connectors": [{ "connector": "github", "mode": "read" }] }
  },
  "precheck": { "command": "test -f Cargo.toml", "timeout_seconds": 30 },
  "policy": { "pause_after_failures": 3, "catch_up_seconds": 3600, "replay_window_seconds": 300 },
  "secret": "the secret you will paste into GitHub"
}
```

```json
TriggerRun
{
  "id": "run_3f1c…", "trigger": "trg_9a0b…", "key": "event:6b1f0e2c9a4d47e8b3c5a1f09d2e7c64",
  "state": "fired", "at_ms": 1790000000000,
  "event": { "source": "github", "kind": "issues.labeled", "id": "6b1f0e2c9a4d47e8b3c5a1f09d2e7c64",
             "delivery": "72d3162e-cc78-11e3-81ab-4c9367dc0958",
             "repo": "acme/app", "author": "alice", "title": "Parser crash", "number": "42",
             "labels": ["bug", "agent"], "payload": {} },
  "operation": "op_5d2e…", "branches": ["issue-42"],
  "outcome": { "ok": true, "detail": "operation op_5d2e… succeeded: issue-42 ready" },
  "finished_at_ms": 1790000000400
}
```

## Setting up a sender

The webhook URL is `<public_url>/v1/triggers/<id>/fire`. Set `public_url` (or `--public-url`) to the address senders reach, such as your reverse proxy's `https://by.example.com`; it defaults to `http(s)://<listen>`, which only a sender on the same network can reach. GitHub, Slack and Linear need `https://` on a public address.

**GitHub.** Repository (or organization) → *Settings* → *Webhooks* → *Add webhook*. Payload URL: the trigger's webhook URL. Content type: `application/json` (`application/x-www-form-urlencoded` also works). Secret: the trigger's (`--secret-file`, or the generated one `by trigger add` printed). Events: *Let me select individual events* → Issues, Issue comments, Pull requests and/or Check suites, as the trigger's conditions read. GitHub's first delivery is a `ping`, answered `200` and ignored; its *Recent Deliveries* tab shows each answer and can redeliver one, which the trigger records once. Mentions: `--on github --if kind=issue_comment.created --if text=@your-bot`.

**Slack.** At api.slack.com, create an app; under *OAuth & Permissions* add the bot scope `app_mentions:read` and install it to the workspace. Copy *Basic Information* → *Signing Secret* into a file and give it to the trigger (`by trigger add … --on slack --secret-file FILE`, or `by trigger secret NAME --secret-file FILE` after creating it). Then *Event Subscriptions* → enable, *Request URL*: the trigger's webhook URL (Slack sends a signed `url_verification`, which the trigger answers), and *Subscribe to bot events*: `app_mention`. Slack retries a delivery not answered in three seconds; the trigger answers at once and fires afterwards, and its `event_id` deduplicates retries. Nothing is posted back to Slack: give the branch a Slack [connector](connectors.md) to reply.

**Linear.** Create the trigger first (`by trigger add … --on linear`), then in Linear *Settings* → *API* → *Webhooks* → *New webhook*: URL the trigger's webhook URL, resource type *Issues*. Linear shows the webhook's signing secret: `by trigger secret NAME --secret-file FILE`. Linear's deliveries carry `webhookTimestamp`, checked against the replay window; keep the server's clock on NTP.

**Anything else.** POST a JSON object signed like Branchyard's own webhooks:

```sh
body='{"id":"deploy-4711","kind":"deploy.failed","repo":"acme/app","text":"p95 regressed","labels":["prod"]}'
sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$(cat trigger.secret)" -r | cut -d' ' -f1)
curl -sS https://by.example.com/v1/triggers/trg_…/fire \
  -H "Content-Type: application/json" -H "X-Branchyard-Signature: sha256=$sig" -d "$body"
```

## Configuration

| Key | Flag | Meaning |
|---|---|---|
| `public_url` | `--public-url URL` | The base of every event trigger's webhook URL. Default `http(s)://<listen>` |
| `allow_trigger_prechecks` | `--allow-trigger-prechecks` (every repository) | `true`, or served repositories whose triggers may run prechecks. Off by default |

Both are in [`schema/server.config.json`](../schema/server.config.json). Every server and `by worker` runs a trigger dispatcher for the repositories it serves; it looks every second, and at once when a webhook records a run.

## Security

- **Secrets.** Each event trigger has its own secret, generated (64 hex characters, shown once) or given. HMAC needs the secret itself, so the store holds it, in the trigger's row: protect `state.db` (mode 600 in a 700 directory) or the database as you would the tokens. It is never returned by the API or `by trigger show`; `by trigger secret` replaces it, and the old one stops working at once.
- **Signatures.** Every delivery's signature is checked before its body is read, in constant time, over the exact bytes received; a missing, malformed or wrong signature is `401 invalid_signature`, and nothing is recorded. The fire endpoint is the only route without a bearer token, and it does nothing but this.
- **Replays.** Slack's and Linear's signed timestamps must be within `replay_window_seconds` (default 300) of the server's clock (`401 stale_delivery`). GitHub and generic senders sign no timestamp, so a captured delivery can be sent again at any time; since an event's ID comes from the signed body, the replay is the run its first delivery recorded, and nothing fires again. What a replay cannot be is new: without the secret, no body that was not sent can be signed. Keep the URL and secret private, and terminate TLS so a delivery cannot be captured.
- **Who a run acts as.** A trigger keeps the principal that created it (tenant, subject, scopes, repositories), and every run is admitted as that principal: its tenant's quotas apply, and its repository must still be in the tenant's allowlist. Removing a token does not remove its triggers; remove or disable them too.
- **Prechecks** run as the server's user, only where the operator allows ([trust](#prechecks)). Event fields reach the precheck as a file, never interpolated into its command line; they reach the harness only in the rendered prompt, and the task's `policy` governs what the harness may do with them. A prompt built from an issue body is text a stranger may have written: allow tools with care.
- **Unknown IDs** and schedules' IDs answer the same `404`, and bodies are bounded by `max_body_bytes`.

## What is durable

| State | Where | Survives a restart |
|---|---|---|
| Triggers, their secrets, state and next time | `DATA-DIR/state.db`'s `triggers`, or the database's `by_triggers` | Yes |
| Runs, their claims and outcomes | `trigger_runs` / `by_trigger_runs` | Yes: a pending run fires after the restart (tested), a held one once its hold expires |
| The task a run fired | The operation registry, as any task | Yes |

The PostgreSQL tables are created like the registry's: a dispatcher opening the store checks the catalog first, and makes only what is missing, each step alone in a transaction under the schema's advisory lock.

## Not yet

- Email triggers; GitHub `push`, `release` and other events; Linear comments; Slack messages other than mentions; replying to the sender.
- A GitHub App installation (one webhook for many repositories): each trigger has its own URL and secret.
- Editing a trigger in place: remove it and add it again (its runs go with it).
- Failover for a routed trigger's branch, as for any remote task; a limit on a trigger's concurrently running tasks beyond its tenant's `max_running`.
- Runs and missed blocks are kept until the trigger is removed.
- Real deliveries from GitHub, Slack and Linear: see the status note above.

## Code

| Path | Contents |
|---|---|
| [`triggers/mod.rs`](../crates/branchyard-server/src/triggers/mod.rs) | The stored trigger, validation, conditions, the clock and settings |
| [`triggers/cron.rs`](../crates/branchyard-server/src/triggers/cron.rs) | The cron parser |
| [`triggers/events.rs`](../crates/branchyard-server/src/triggers/events.rs) | Signatures and the GitHub, Slack, Linear and generic adapters |
| [`triggers/template.rs`](../crates/branchyard-server/src/triggers/template.rs) | Placeholders and branch names |
| [`triggers/precheck.rs`](../crates/branchyard-server/src/triggers/precheck.rs), [`target.rs`](../crates/branchyard-server/src/triggers/target.rs) | Ported from Orca: the precheck runner and run-target resolution |
| [`triggers/store.rs`](../crates/branchyard-server/src/triggers/store.rs) | The SQLite and PostgreSQL stores and their conformance suite |
| [`triggers/engine.rs`](../crates/branchyard-server/src/triggers/engine.rs), [`dispatch.rs`](../crates/branchyard-server/src/triggers/dispatch.rs) | Claiming, firing, settling and pausing; the dispatcher in a server or worker |
| [`triggers/routes.rs`](../crates/branchyard-server/src/triggers/routes.rs) | The HTTP API and the webhook endpoint |
| [`branchyard-client/src/triggers.rs`](../crates/branchyard-client/src/triggers.rs) | Wire types and client methods |
| [`branchyard-cli/src/trigger_cmd.rs`](../crates/branchyard-cli/src/trigger_cmd.rs) | `by trigger` |
| Tests | Unit tests in each module; [`tests/triggers.rs`](../crates/branchyard-server/tests/triggers.rs) (the server over loopback, and PostgreSQL); [`branchyard-cli/tests/triggers.rs`](../crates/branchyard-cli/tests/triggers.rs) (`by trigger` locally, through `by serve`, and remotely) |
