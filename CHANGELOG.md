# Changelog

## Unreleased

A skill's references are searchable. In a turn's tool tree, `bin/tools search`
ranks each `references/*.md` of a staged skill as a document of its own
(`SKILL/references/FILE.md`: the skill's and the file's name, the first heading
and the content words of the body), so a search lands on one paragraph of a
skill without the model reading the skill, and `bin/tools show
pptx/references/layouts.md` prints that file alone. A reference adds nothing to
the corpus statistics, so the tools keep their order among themselves. A tool
`INDEX.md` lists (chosen for the turn) scores 1.5 times one it does not, and a
reference is scored on its words alone; `--plain` ranks by BM25 alone and
`--kind reference` narrows to references. Frontmatter `keywords` are indexed.
Ids are `[kind:][source:]name[/references/FILE.md]`. `python3
tools/skill_eval.py rank` ranks every eval scenario's prompt through the script
over a tool tree without a model (top-1, top-3, mean reciprocal rank), and
`expect_skills` may name a reference; seven reference scenarios were added.
Hermetic result, 7 November 2026: top-3 98% and MRR 0.800 over 48 expected
ids, against 98% and 0.779 over 41 at the commit before; the references rank
in the top three 7 of 7 times.

The delegate skill is now a small core plus reference files. `SKILL.md` fell
from 38,969 to about 7,500 bytes, and a spawning branch's instructions from
42,875 to 11,682 characters (`spawning-branch` cap 44,500 to 12,500). The rest of
the guidance moved word for word into 14 files under
`plugins/branchyard/skills/delegate/references/`, each opening with a
`Read when:` line, and a table in the core names the file for each need. They
are installed beside the skill (plugin directory, `by init plugin`, the
standalone zip), and a turn with helpers finds them in its tool tree at
`helpers/delegate/references/`.

The console reviews a branch's diff. A **Changes** page (`#/changes/<repo>/<branch>`,
linked from the board row, the branch page and the Trajectory page) shows the
candidate against its base file by file, with a path filter, line numbers on
both sides, viewed boxes, and `j`/`k` for files, `n`/`p` for hunks and `c` to
comment. Select lines (click, drag, Shift-click or the arrow keys), write a
multiline comment, collect comments in a tray and send them as one review,
as the next turn or steered into the running turn: the branch receives one
prompt in `by review`'s format that names each file and line and quotes the
code, and the comments are recorded on its log like `by review`'s. Sent
comments show inline and are marked addressed when a later candidate changes
their lines. New routes `GET …/branches/{b}/changes`, `GET …/branches/{b}/reviews`
and `POST …/branches/{b}/reviews` (read, read and run scope; `409
stale_candidate` when the branch moved), `Branch::changes` and `Branch::reviews`
in the SDK, `Repo::changes`, `reviews` and `review` in the Rust client and
`Repo.changes`, `reviews` and `review` in the Python one. See
[Changes and review](docs/companion.md#changes-and-review).

Stacked pull requests. `by run --base BRANCH` (and `by fan --base BRANCH`) with the name of a branch starts the new branch on that branch's candidate and records the stack. `by pr` on it opens its pull request against the parent's pull request head and lists the stack in the body; once the parent's pull request has merged, `by pr` and `by pr --watch` rebase the child onto the branch it merged into (only its own commits are replayed, so a squash merge works), push it over its old head if the remote branch is still where `by pr` left it, and retarget its pull request with `gh pr edit --base`. A conflict is recorded (`restack_blocked`) and reported, never forced. `by show` shows the stack. New events `stacked`, `restacked` and `restack_blocked`; see [stacked pull requests](docs/pull-requests.md#stacked-pull-requests). Tested with a fake `gh` and real repositories; not run against GitHub.

Starting from an issue on a server. `by issue REF` shows what `by run --issue REF` would start from. `TaskRequest.issue` and `StartSessionRequest.issue` take the same references; the server resolves them by running `by issue --json` with its own `gh` login and tracker credentials, and refuses an unreachable one with `issue_unavailable` and a `next:` step. `GET /v1/repos/{repo}/issue` and `GET /v1/workspaces/{id}/issue` preview it, the console's new-session form gets "Start from an issue" (look up, see the title, start), and the Rust and Python clients gain `issue` calls. See [starting from an issue on a server](docs/pull-requests.md#by-issue-and-starting-from-an-issue-on-a-server).

A tool call in a branch's event log now says what it acted on. `tool_started` carries an optional `target`: the file a call read or changed (relative to the worktree, `tools/...` for the mounted tool tree, and only the last component of any other absolute path), the first 120 characters of a shell command with secrets replaced by `[redacted]`, the server and tool of an MCP call, or the skill a harness's own skill tool opened. Never a file's content or a call's full arguments. Claude Code, Codex, ACP agents (Gemini CLI, Qwen Code, OpenCode, Antigravity over ACP and the others, where the call's input or location names it), Amp and Pi report it; a harness that reports no input leaves it out, as do older logs, and the timeline stays content-free. `by log` prints it after the tool name, and `by log --json` and `schema/contract.json` carry it. See [tool call targets](docs/harness-integration.md#tool-call-targets).

`by run`, `fan`, `map`, `send`, `fork` and `reincarnate` take `--skill NAME` (repeatable) to stage only the chosen built-in skills and the skills they need; an unknown name is refused with the built-in skills listed. `by run --delegate` takes `--helpers ID,ID` for the harnesses its children may run, where before only the harness's own profile could be allowed to a branch you started.

The skill-discovery eval (`tools/skill_eval.py`) uses all of it: it reads targets, passes each scenario's skills and helpers, and generates the binary input files 13 scenarios name (docx, xlsx, pptx, pdf, deterministic, with the artifact toolkit's libraries) so 8 of them have file checks. The fake ACP agent reports `SH` lines as tool calls and has a `READ` directive. See [skill-eval](docs/skill-eval.md).

## 0.5.0 - 2026-11-07

A harness's own permissions and Branchyard's policy now compose: a Branchyard
approval never overrides a native denial, and a native approval never bypasses
a restriction of the branch's policy. A run whose policy restricts anything
starts only on a profile that keeps both rules where it runs, and is refused
with `unsupported` (detail `unbound_policy`, 422 from the server) otherwise.
`by` names `--isolated` when a home Branchyard prepares would keep the policy,
`--allow-unapproved-tools` to let the harness's permissions decide, and another
profile. A branch remembers the opt-out for its sends, forks and children. Claude Code's stream-json profile keeps both rules anywhere: a
`PreToolUse` hook sends every call to Branchyard, so an allow rule in the user's
or the repository's settings no longer approves on its own, and the session must
report `--permission-mode default`. Codex's app-server profile, Gemini CLI and
Qwen Code keep them in a private home: Codex gets a permission profile with a
deny entry, so an approved command is not run again outside its sandbox, and an
untrusted working tree; Gemini CLI and Qwen Code get `--approval-mode default`
and a rule file that asks about every call, and a session in another mode is
refused. Other profiles are refused under a restricting policy until a mechanism
is verified for them. Branchyard no longer writes
`/etc/claude-code/managed-settings.json` in a sandbox, which replaced an image
administrator's file. Over `antigravity-acp`, a private home now gets `ask` rules in the project file `agy acp` reads and `toolPermission: request-review`, so commands, file writes, edits and reads reach Branchyard; the `permissions` lists Branchyard's docs named for it are read only headless. Antigravity's ACP profile still cannot keep a restricting policy, since web search, scheduling and subagents run without asking. Claude Code takes `--effort` as `CLAUDE_CODE_EFFORT_LEVEL`.
Reported by @arsenyspb in
[#62](https://github.com/vamsiramakrishnan/branchyard/issues/62); the details
are in [composing with a harness's own permissions](docs/harness-integration.md#composing-with-a-harnesss-own-permissions).

Observability is rebuilt on the standard Rust stack: `tracing`,
`opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` and
`tracing-opentelemetry`, set up for every binary by the new
`branchyard-telemetry` crate. The hand-written span model, `traceparent` parser
and OTLP encoder are gone. Traces and metrics are configured by the standard
`OTEL_*` variables and exported over OTLP (HTTP with protobuf or JSON bodies,
or gRPC) from `by serve`, `by worker` and now any local `by` command, and only
when an endpoint is set. `GET /metrics` keeps every existing name and label and
gains HTTP request rate and duration by route template, refusals by error code,
operation duration, turns in flight, first-event latency and stalls, tool and
connector call durations, and the GenAI client metrics `gen_ai.client.operation.duration`
and `gen_ai.client.token.usage`. Setting `OTEL_EXPORTER_OTLP_ENDPOINT` for a
harness's own telemetry now also makes a `by` that inherits it export; see
[observability](docs/observability.md#configuration).

A message between siblings can now carry a deadline: `by message --deadline 10m`
(or `branchyard.message(..., deadline=600)`, or `deadline_ms` in the server's
`POST .../message` body and the Rust and Python clients; the MCP tool leaves it
out to stay within a leaf's tool-list cap). A message still pending when the
recipient's next turn starts after its deadline is not given to the turn: it
counts delivered, and a warning naming its id goes on the recipient's log. A
sibling with 32 undelivered peer messages now refuses another with `denied`,
naming the count and the limit.

`branchyard.toml` can now declare kinds of message siblings may send each other:
`[[messaging.kinds]]` with a `name`, an optional `schema_version` and
`description`, and an inline JSON Schema table (`[messaging.kinds.schema]`)
that a message's `--payload` must match. A yard registers them when it is opened
with its configuration, with the refusals `Yard::register_kind` makes; a bad
kind is a configuration error naming it. `by kinds [--json]` lists the built-in
and configured kinds.

`ask` takes an idempotency key like `message` does: `by ask --key KEY`, the MCP
`ask` tool's `key`, `branchyard.ask(key=)`, `key` in the server's `POST .../ask`
body and the Rust and Python clients. Asking again with the same key returns the
first question (and its answer once there is one) instead of asking twice; the
same key with other text is refused. `report`, `escalate` and `answer` do not
take a key.

A message and its entries on the sender's and the recipient's event logs are now
written in one transaction, on SQLite and on PostgreSQL, as a handoff's already
were: a failure writing either leaves neither, so no crash leaves a message that
no log names.

Connectors, skills and helpers are now one tools model: each passes through the
same states (available, allowed, ready, chosen, mounted), with one owner for
each. Tool policy lives in the store by tenant, group and person
(`GET/PUT/DELETE /v1/admin/policies`, and `GET/PUT /v1/me/policy` for a
person's own narrower choice), and a session's tools are one start field,
`tools`, checked against one catalogue (`GET /v1/tools`) with workspace defaults.
A turn mounts one searched tool tree instead of copying every skill into its
sandbox, and Branchyard's own MCP surface follows the grant. A turn's manifest
is pinned by digest and a worker holds a turn to the policy it was admitted
under. Connector calls from a sandbox go through a credential proxy inside it,
which fetches each turn's token from the server with a per-turn secret, and
egress is enforced. On Google Cloud, gateway tokens can be signed with Cloud KMS,
people can sign in through IAP or a federated OIDC issuer with their groups, and
the store can reach PostgreSQL over TLS with IAM tokens (AlloyDB). The console
gains a Tools page and one Tools control in the composer. See
[tools](docs/tools-model.md).

Harness images and launch time. `by images build` builds a toolchain image with
the harness CLIs preinstalled at pinned versions and labelled with what it holds,
installing Git when the base lacks it; `by images pin REF@DIGEST` makes a
digest-pinned image a provider's default, and the router reads what an image
holds from its labels. `by harnesses update ID --qualify` installs a release in a
private prefix, checks it, and pins it only on a pass; with `--with-model` only
this run's record counts. Every turn now records its startup phases by provider
and path, `by startup report` gives their p50 and p95, and GKE sandboxes can be
pre-booted in a warm pool. See [toolchain images](docs/images.md) and
[providers](docs/providers.md).

The console's board is drawn from each branch's timeline and kept current from
the event stream, instead of re-reading events. A new Trajectory page
(`#/trajectory/<repo>/<branch>`) shows what one agent did, step by step: a
zoomable waterfall of turns, startup phases, tool calls, permission waits,
steers, stalls and delegations, nested lanes for the helpers it delegated to
with a side-by-side comparison of siblings or `by fan` attempts, and a step's
content read only when it is opened. The timeline gained each turn's `seq`,
`startup`, `tokens` and `marks`, and the session's `session_tokens`, all without
content. See [Trajectory](docs/companion.md#trajectory).

Harness-to-harness messages go through one messaging layer: an envelope, a
registry of message kinds with JSON Schema payloads, and a pipeline of stages
(authorize, validate, persist, observe, deliver) that `ask`, `answer`,
`report`, `escalate` and peer handoffs now use, with unchanged output. Sibling
branches can message each other with a registered kind (`by message`, the MCP
`message` tool, `POST .../branches/{b}/message`, `Repo::message`,
`branchyard.message`), delivered at the recipient's next turn, idempotent under
a key and carrying the sender's trace. The store gains the additive `meta` and
`idempotency_key` message columns, which a store from before them gets when it
is opened. A session-start hook reads an unread message that carries them. See
[messaging](docs/messaging.md).

The release's checksum file no longer lists the per-job reports the release host
removes, so every name in it matches a published asset.

## 0.4.0 - 2026-11-02

A running branch reports engine liveness, and `by obligations` lists unresolved
work across branches. Recovered turns read the effect ledger before retrying
outside actions; failed calls are reported as failures rather than proof that
nothing happened. Calls from an earlier turn remain visible while a later turn
runs. Durable timers pace effect lookups using the store clock, and irreversible
calls without a lookup require a decision even when policy would allow them.
Shutdown stops active turns cleanly within its grace period. Rewinding across a
changed lockfile runs setup again, and the engine checks branch records against
their event logs.

The console shows clearer costs and decision actions. A documentation site is
built from the repository guides.

`by compute gce --service-account EMAIL` creates the worker with the
`cloud-platform` access scope, so harnesses on it reach Vertex AI, `gs://` and
Cloud KMS as that account, within its IAM roles. Workers previously got no
scopes, and their metadata server gave no usable token. `--scopes` takes a
comma-separated list or `none`. A worker made with an account by 0.4.0 or
earlier, which has no scopes, is reused as it is, and the result's
`access_scopes_note` says that its account cannot reach those services and how
to get a worker that can.

Every surface now places a branch with one table: its `stage` in branch JSON (`{"lane", "reason"}`, [what a branch needs from you](docs/concepts.md#what-a-branch-needs-from-you)). The console's lanes, `GET …/decisions`, the hook lines, the status line and bare `by` read it, so they agree. A branch waiting on its children now sits in Running, not Done. A delegated child that settled while its parent still runs or waits sits in Running too, and the console says "its parent decides". Bare `by` points at the first branch that needs you, a failed one before a ready one. A workspace session whose plan waits for approval no longer reads as failed.

`GET …/decisions` lists a branch whose plan waits for approval as a new `plan` decision (`waits: agent`) and a blocked branch as a `stop`, lists each branch at most once, and no longer lists a question from a branch whose turn ended or a settled child whose parent still works. The console's deck shows a plan card that opens the branch. An older Rust client's `decisions()` cannot parse the `plan` kind; Python and the console read kinds as strings.

The hook lines name a branch whose turn is parked on an approval, with `by approvals allow ID` and `by approvals deny ID`, and a running branch whose harness went silent past its stall window, with `by log NAME -f`. The status line and bare `by` count the first as asking and show a `stalled` count for the second.

An event a bot caused no longer fires a new event trigger by default. Each
source says when its event is a bot's: a GitHub App or `[bot]` account (never
a check suite), a Slack `bot_id` or `bot_message`, a Linear actor that is not a
user, a generic body's `"bot": true`, an email with `Auto-Submitted` other than
`no`; the event carries `bot`. A new event trigger is stored with
`conditions.bots: deny` unless it gives `--if bots=allow`, and a bot named in
`--if author=` still fires; a skipped bot event is a `skipped_condition` run
with the reason. Triggers created before this release have no `bots` and keep
allowing bots, and `by trigger list` and `show` print the value each acts on.
The bot fields are read as each source documents them; no live delivery has
confirmed them. The task a trigger's run starts records the trigger, the run
and who caused the event (`Task.trigger`, the `trigger` line of
`by task show`), and keeps it after the trigger is removed.

A trigger can observe before it fires. `by trigger add … --observe` (or
`"mode": "observe"` in a `TriggerSpec`) records each run that would fire as
`observed`, with the task it would have started, and starts nothing: no task,
no send, and none of the lifecycle of a branch it delivers to. Read the record
with `by trigger runs NAME --state observed` (`GET /v1/triggers/{t}/runs?state=`),
then let it fire with `by trigger mode NAME fire` (`POST /v1/triggers/{t}/mode`),
or from the console's **Observe only** toggle. An event observed stays observed
when redelivered after the switch. The default is `fire`. A server older than
this release cannot read a trigger stored with `mode: observe`; upgrade every
server on a shared database first. `Client::trigger_runs` takes a state.

The note a branch's next turn starts with after its engine stopped mid-turn (`<branchyard-recovered>`) now also says that an action the branch took outside its worktree before then (a push, a pull request, a comment, a message) may already have happened, and to check before repeating it. Connector calls were already recorded in the effect ledger; a command the agent ran from its own shell was not.

A server runs a request that continues existing work ahead of new work. A
send to an existing branch (`POST .../branches/{b}/send`), a plan's approval
or rejection, and a console session send are follow-ups: they are claimed
before every queued launch whatever its priority, and each process keeps one
slot beyond `--max-running` for them. A tenant at its `max_running` still
admits one follow-up while it has none unfinished. Previously such a send
waited behind every queued trigger-fired task, then for a running turn to
end, and a tenant that triggers had filled refused it with `429
quota_exceeded`. A trigger's send, a merge, an integration and a check stay
normal work, and a process now runs up to `max_running + 2` operations: one
follow-up and one console cancel or steer beyond the limit.

A branch nobody delegated (a root started with delegation, or a fork of one) can now `ask` and `escalate`, and its message goes to the person, `@person`. Its question keeps it in Needs you, with stage `question`, until someone answers it, also after its turn ends. `Yard::inbox` lists the person's messages and `Yard::answer` answers one. An answer to a branch whose turn ended does not start a turn; the branch gets it at the start of its next turn.

A root's `report-status blocked` or `failure` is now escalated to the person. Before, it was only recorded.

A fork's question or escalation now goes to the person. Before, it went to the branch it was forked from, which could not answer it.

The person answers with `by answer ID "TEXT"` and reads with `by inbox` (which marks an answered question `[answered]`, and with `--unread`, or `unread=True` in Python, lists only those still waiting), outside a harness and without `--as`; in the console's Needs you, which labels the box "Answer"; through `GET /v1/repos/{repo}/inbox` and `POST /v1/repos/{repo}/answer`; or with `Repo::person_inbox` and `Repo::answer_message` in the Rust client and `Repo.person_inbox()` and `Repo.answer_message()` in Python. When the asker's turn has ended, `by answer` and the console say it reads the answer at its next turn and how to start one (`by send NAME "<prompt>"`). Hook lines and notices put such a question to you (`- root asks you #12: "…". Answer: by answer 12 "TEXT"`). Inside a harness started without delegation, `by` still refuses, and the refusal now says how to start a branch that can use it (`by run --delegate`, or a trigger request's `delegation`).

`GET /v1/problems` lists what background work could not do that a person
can act on: a trigger that paused itself after `pause_after_failures` failed
runs in a row, a queued operation that requires labels no live worker
carries once it has waited `unclaimable_after`, a webhook whose last three
delivery attempts failed (every retry counts), a push subscription whose
last three sends failed, and an effect the latest recovery pass left
`unknown`. Each entry has `kind`, `key`, `what`, `since_ms`, `failures`
where counted, `error` and `next`, the one thing to do; a healthy server
lists nothing. A caller sees its tenant's push entries, its tenant's
trigger and operation entries on the repositories it may reach, and the
effect entries of those repositories, `admin` or not: `admin` never
crosses tenants, since a credential without `scopes` holds it. Webhook
entries, which are operator configuration, are seen only by an `admin`
that may reach the webhook's repository. Trigger and operation entries are derived from the shared store
and survive a restart; webhook, push and effect entries are what the
server process saw, start empty when it restarts, and come back after the
next failures. A push entry never shows the subscription's endpoint, and a
push send's error no longer carries it either. `Client::problems` and the
Python SDK's `Client.problems` read it, and the console's home page lists
the entries above its tiles, each with its error and the one thing to do,
and shows no strip when there are none. Previously a paused trigger showed
only in `GET /v1/triggers`, an unclaimable operation only when read by its
ID, a failing webhook or push only in the server's log, and an unknown
effect only in the effect ledger without its reason. `/healthz` is
unchanged. `by doctor` with a configured server adds a `server.problems`
row: `ok` when the server lists nothing, a warning with the count, the
oldest entry's `what` and `error` and its `next` as the fix when it lists
something, and a warning to upgrade a server that answers `404` for the
route.

Native container and GKE Agent Sandbox providers copy each turn's worktree and
private home into an owned allocation, enforce immutable image/runtime and
resource settings, and return files before whole-allocation cleanup. Container
defaults to gVisor; managed GKE also supports Kata Cloud Hypervisor MicroVMs.
The CLI exposes explicit runtime, network and Kubernetes target options. Server
and console access requires operator execution presets for the new providers.

GCE setup can install a pinned complete gVisor runtime. Microsandbox uses a
patched runtime that corrects the low-memory guest RAM layout; release delivery
verifies its archive, executable, firmware and provenance. Checkpoints bind to
their runtime identity and refuse incompatible or legacy references without
deleting them. Copied worktree return failures now fail ordinary coding turns,
and failed native cleanup remains registered for retry.

The Rust client resumes a response read interrupted by an operating-system
signal, keeping the same connection and request. Waiting for the first byte
previously returned a transport error. Timeouts and closed connections still
follow their existing error and retry rules.

The Rust client (`branchyard-client`, and the `by` hooks and CLI that use it) keeps its HTTP/1.1 connections alive: a request reuses an idle connection to the same server when its response left the connection open and its body was read to the end, instead of opening a new TCP and TLS connection for every request. A `GET` or `HEAD` that finds a connection the server had closed is sent once more on a new one; no other request is sent again because of a closed connection (a `POST` is still resent with its idempotency key, as before).

Four corrections from the 1 November qualification of the ACP harnesses.
DeepSeek Harness and Hermes run a benign shell command without asking their
client, so their profiles now say so (`tool_approvals` carries the caveat in
the compatibility matrix), the qualification runner records the permission
scenarios as declared for them, interrupts a running tool on the harness's
own word that it started rather than waiting for a request that never comes,
and fails `permission_allowed` where a harness that routes approvals let the
command run unasked. Provisioning projects the branch's policy into each
harness's own setting: `DSH_PERMISSION_MODE=read-only` for dsh unless the
policy allows the shell outright, and an `approvals.deny: ["*"]` rule in a
managed block of Hermes's `config.yaml` (a shared home is refused). The ACP
driver ends a turn `interrupted` when the agent answers a cancelled prompt
with an error instead of `cancelled` (Hermes 0.19.0), keeping the error as a
warning, and can choose a pinned model through a session's `model` config
option (`session/set_config_option`) where a profile turns it on. OpenCode
takes a `ZAI_API_KEY` secret as the `ZHIPU_API_KEY` its Z.ai providers read.
Each qualification report records the model the harness named in its usage
(`model_reported`) beside the one asked for.

The Python SDK's `Client` keeps one HTTP connection per thread and reuses it for ordinary calls (`http.client`), where it opened a new TCP and TLS connection for every call. A kept connection that has idled more than 5 seconds, was made before a `fork`, or has been closed by the server is replaced before the next request is sent, so an unkeyed `POST` or `DELETE` is not sent on a connection the client can already see is closed (a close still in flight, or a proxy with a very short idle timeout, can still race). A proxy from the environment and the event stream are unchanged. A `GET`, a `HEAD` or a keyed request answered with a redirect is sent again through `urllib`, which follows it as it always did (a `POST` answered 301, 302 or 303 becomes a `GET`; a 307 or 308 is raised as an error); any other request answered with a redirect is not sent again (the server may already have acted on it) and raises the status as an error.

The console's second pass gives it depth and density. Surfaces have three
elevations (the page, raised panels and cards, and the overlay for the one
object a page asks about), each a background, a border and a shadow in light
and dark, with tinted neutrals and a darker sidebar. Home leads with five
tiles (in flight, need you, ready, merged today, spent) and draws each branch's
turns on a clock that fits the work: a bar per turn from its own events, a tick
per tool call, the amber wait, the merge tick, each row's cost, the done
branches folded, and under the board the day's decisions and each branch's
spend. The deck's card has a tinted header band, its evidence as labelled
sections and its answers as a footer, the next card peeking under it, and the
day's decisions (from `GET …/approvals?all=true`, the merges and the ledger)
where the empty folds were. A branch reads as a report with the prompt as its
headline, legible margin notes joined to their paragraphs, a receipt of
labelled figures, a diff header that stays in view and counts on every tab.
Empty yards, decks, branches and workspaces say what to do. The README
screenshots add dark variants of the board and the deck, from a demo whose
branches run Claude Code's stream-json profile through a script that reports
cost.

Office files open in a full-width console viewer: DOCX pages load as you scroll,
PPTX slides have thumbnails and keyboard navigation, and XLSX workbooks have
sheet tabs, column widths, merged cells, formatted cached values with formula
tooltips, frozen headers and <!-- fact:artifact_preview.row_page -->200<!-- /fact -->-row pagination. Print view renders the selected
sheet through LibreOffice and Poppler. Page previews are cached individually
and bounded to <!-- fact:artifact_preview.page_cap -->64<!-- /fact --> pages, with download guidance for longer originals.

An HTML output of a workspace session can be seen and used from the console.
It previews as a page image like a DOCX (headless Chromium inside the
converter's Bubblewrap namespace, no network, the source read-only, so `GET
…/preview` answers `passed` with one page instead of `422 preview_unsupported`),
and **Open** runs the page in a sandboxed iframe: served from `/app/frame/{ticket}`
(minted by `POST …/artifacts/{artifact}/frame`) with `Content-Security-Policy:
sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline';
style-src 'unsafe-inline'; img-src data:; frame-ancestors 'self'`, framed with
`sandbox="allow-scripts"` and no `allow-same-origin`, so it reaches neither the
console's storage nor its token. The rest of the console keeps
`X-Frame-Options: DENY`; its policy gains `frame-src 'self'`.
`BRANCHYARD_PREVIEW_CHROMIUM` names the browser; the default is the one under
`PLAYWRIGHT_BROWSERS_PATH` or `/opt/pw-browsers`.

The console becomes a dispatcher's desk. Needs you is a deck: one decision
at a time, shaped for what it is (an approval, a question, a merge, a stop,
a stall), the question at the top, the evidence in the middle and the
answers at the bottom, taken from the keyboard; an allow or a deny is held
for eight seconds with Undo before it is sent, and a merge is a hold, not a
dialog. Home is a board: every open branch on one clock, the last hour to
scale, a wait on you filled amber from the moment the turn parked, with a
steer box under a running row. A branch page reads as a report: each tool
call is a note in the margin beside the paragraph it preceded, and each
turn ends with a receipt.

For that, a branch record now says what its running turn is parked on:
`awaiting` names the approval ask, what it asks and since when, read from
the pending asks whenever the branch is listed or looked up, and `by ls`
shows `waiting for approval: …`. The server adds `GET …/decisions`
(`DecisionList`, `Repo::decisions`): what waits for a person in the order to
take it, parked agents first, oldest wait first. Nothing in it expires.

The ground plan says which files are in two hands: `GET …/ground`
(`Ground`, `Repo::ground`, `app.ground()`) lists every path an open branch
changed with the open branches that changed it, read from their diffs when
asked; a branch whose diff cannot be read is named in `skipped`. A branch
page gains a Ground tab ("also in app-2" or "only here") and a merge card
names the files another open branch also holds.

In the deck, Enter in a question's answer box sends the answer (Shift+Enter adds a
line); it used to add a line, so only the Answer button sent.

Knowledge work reaches the deck. `GET /v1/projects/{project}/decisions`
(`Client::project_decisions`, `Client.project_decisions`) lists a project's
workspace outputs waiting to be accepted (`output`) and its sessions that
failed, were interrupted or never started (`session`) as `Decision`s, and
Needs you shows them beside the repository's: an output card mounts the
preview, with Download original and a held Accept, instead of a diff; the
project page's board draws its open sessions on one clock; the tab title
counts the cards.

Each turn's receipt now says what that turn cost. The engine records a
`turn_cost` event (`Activity::TurnCost`) when a turn's metering has settled:
what the branch's cost grew by, so a harness's running session total counts
only the part the turn added. A turn nothing measured records none, and the
branch's `cost_usd` stays its total. `by log`, `by show`'s checkpoints, the
event page and Python's `FeedEntry.turn_cost` carry it.

Reading an operation, or a fork, no longer fails with `404 unknown_branch`
for a branch that was removed meanwhile: a fork's removed parent, or a goal
judge's scratch branch removed between two reads of the operation that ran
it. The authorization walk ends at a branch that is gone.

The console renders an agent's reply instead of printing it: headings, lists,
quotes, pipe tables, emphasis, inline code and web links become elements, a
fenced block is code under a header with a Copy button, and a JSON block shows
its keys and values with the text one click away. Nothing from the server is
parsed as HTML, and a relative link target stays text. Each reply sits under a
head that names the harness and marks it as the agent's, and each tool call
carries an icon for its verb.

The page gains depth and the same system in both themes: three surfaces, three
shadows, one radius scale and motion that only moves and fades, still under
reduced motion. State is a tinted pill in the strip and on each turn's outcome,
the open branch carries its lane's colour, the changed files show how much each
adds and removes, the composer says which key sends, the filter shows its `/`
shortcut, and a loading conversation shows quiet bars rather than a sentence.
Text in the sidebar, strip and conversation is larger on a phone.

The README's console screenshots are regenerated for this console: the branch page
with margin notes and a receipt, and the workspace with its table preview.

DOCX and PPTX previews work under the default Bubblewrap backend. The namespace
set `SAL_USE_VCLPLUGIN=gen`, LibreOffice's X11 backend, which overrides
`--headless`, finds no display and exits 0 without a PDF, so every Office page
preview failed with "LibreOffice completed without producing the requested
PDF" while CSV and XLSX tables rendered. The namespace now sets `svp`, the
display-less backend the Docker backend already used; isolation is unchanged.
When a converter exits cleanly without a PDF, the failure message quotes its
first diagnostic line.

Antigravity runs over ACP by default. `antigravity-acp` launches `agy acp`,
the CLI's ACP server, so a branch on Antigravity can be cancelled
(`session/cancel` ends the turn `cancelled`), its shell and file tools ask
Branchyard before they run (`session/request_permission`, answered from the
branch's policy, so `--allow-unapproved-tools` is no longer needed), `--model`
is honoured through `session/set_model` against the models the agent lists,
and each turn reports its tokens. The headless stream stays as
`antigravity-stream-json`, with its permission rules. The ACP driver gained
the two unstable methods behind per-profile flags (`acp_models`, `acp_usage`),
off for every other agent. Checked against Antigravity CLI 1.4.12 with model
calls, recorded as fixtures, and live-qualified 9 of 9.

## 0.3.1 - 2026-10-31

Operators can name the environments console sessions run in.
`execution_presets` in the server configuration lists each environment's
provider, harnesses, models, secrets by name, tenants and worker labels, and
`require_workspace_execution_presets` makes workspace sessions choose one. The
browser sends a preset ID, and the server resolves the rest and validates it
through the existing checks. A session on a remote preset never falls back to
local execution, and members see placeholders where an error would name a
preset's paths, endpoints or secrets. `GET /v1/execution-presets` reports each
environment's readiness from configuration and offline checks only, and the
manager MCP starts sessions on a preset.

The console guides a workspace start: choose sources, an environment, a harness
and model, and the outputs to collect, then follow several sessions side by
side, each with its own draft and outputs. Sources open in the inspector like
outputs, outputs are grouped by session and turn, and only the current turn's
outputs can be accepted. Source titles may be any Unicode text, sent with
`X-Source-Title-Encoded`, and downloads keep the title. XLSX and CSV previews no
longer warn about columns they did not cut, a session's first message shows
what the person typed, and follow-ups in a session that collects outputs keep
collecting them.

A repository's own skills in `.agents/skills/` reach a branch once a person
trusts that set with `by skills trust` or a server operator allows them. Every
turn records the skills it ran with, and copied sandboxes record their ready
time and transfer sizes. `by up`, `by doctor`, `by setup` and
`by substrate check` report remote readiness as seven separate checks and never
treat a found binary as ready to run; `by up --plan --smoke` adds one bounded,
read-only call to the control plane.

CI runs the test jobs under cargo-nextest and splits the PostgreSQL and coverage
jobs into partitions. Pull requests plan releases instead of building every
target, and main rehearses the build. The README says why Branchyard exists and
shows the console.

The evidence for this release is hermetic: fake agents, fake providers and
recorded fixtures. No live model, GKE or Substrate run was made for it.

## 0.3.0 - 2026-10-29

One desktop console now handles coding and knowledge work. Projects contain
workspaces and named native sessions. People can upload sources, follow
conversations, preview CSV and Office outputs, download originals and accept
current results. `by serve --app --knowledge` starts without a repository.

Workspace access combines token scopes, membership and the session creator's
identity. Queued actions recheck exact device credentials. Runtime and pull-request
services use the existing durable queue; manager MCP carries workspace revisions
for lifecycle actions. AGY tool approval compatibility requires an explicit
acknowledgment and operator permission.

Remote inputs have one bounded metadata index linked from the native prompt.
Copied providers stage selected files before their initial upload. Concurrent
session starts retry rolled-back metadata conflicts, and follow-up turns retain
the validated session policy. The pre-push hook clears inherited Git repository
context before running checks in scratch repositories.

Live checks covered parallel document work on GCE and a captured CSV deliverable
in a GKE Substrate gVisor actor. The GKE helper offers a Docker-free agentgateway
path with a compatible immutable gateway image. Temporary cloud resources were
verified removed. See the [validation record](docs/console-validation-2026-10-29.md)
for versions, setup failures and the limits on retained runtimes, microVMs and
other harness/model combinations.

## 0.2.1 - 2026-10-29

Operation lookups recheck the caller's current repository grants, including
idempotency-key reconciliation and waiting requests. A tenant match alone does
not expose an operation from another repository.

The README now follows installation, an AGY task, review and merge, document
work and remote setup. Package descriptions cover coding and knowledge work.
A shared vector icon, wordmarks and sharing card give the CLI's companion and
repository a consistent identity. The CLI lists readiness with setup and doctor.

The companion keeps search focus during updates, distinguishes loading, empty,
failed and filtered lists, and keeps full status reasons and navigation usable
on small screens.

## 0.2.0 - 2026-10-29

Branchyard now supports cooperative harness teams for coding and knowledge work,
with frozen handoffs, result contracts and parallel deck assembly that preserve
each harness's native planning and delegation.

Document skills create and check editable Office and PDF artifacts. Knowledge
skills capture exact source bytes, extract tables, clean CSV data with explicit
policies, calculate quantities with units, check citation integrity and render
layout previews. Design skills cover composition, charts and diagrams. Small
folder indexes progressively disclose instructions and references. Reused
upstream material is pinned with licenses and installed-content checks.

Managed file capture supports declared ignored outputs on copied providers,
producer fencing, bounded receipts and recovery. Operator model/effort changes
retain stored credential sources. Failed waiting JSON sends return a failing
shell status. Native identity uses Google ADC and exact Google ID-token bindings;
the merged Anvil companion adds protected upstream MCP authentication.

Bar and column charts include zero, and local evidence citations retain checked
file hashes and locators. Python 3.9 remains supported for the SDK and most
artifact operations; patched PDF table extraction requires Python 3.10 or newer.

Installation remains a single shell command using the latest GitHub release.
See [knowledge skills](docs/knowledge-skills.md),
[atomic knowledge validation](docs/atomic-knowledge-validation-2026-10-29.md), and
[remote validation](docs/remote-validation-2026-10-28.md) for commands, evidence and
limits. Quotation matching does not establish factual support; rendering does
not establish visual approval. Live Google Workspace human OAuth consent and
the agentic compute engine remain unqualified.
