# SDKs

Written 10 October 2026.

Branchyard has three programmatic surfaces besides `by` itself. Which one you use depends on where your code runs.

| Your code is | Use | How it reaches Branchyard | Acts as |
|---|---|---|---|
| a harness turn inside a branch (Claude Code, Codex, Gemini CLI, ... running a task) | [`sdk/python/branchyard.py`](../sdk/python/branchyard.py), the MCP tools, or `by` in the shell ([delegation](delegation.md#python)) | `by ... --json`, found from `BRANCHYARD_BY` or `PATH` | that branch, on its own descendants only |
| a service outside: a Slack, Discord or Teams bot, a web app, a scheduler, a connector's own back end | [`sdk/python/branchyard_client.py`](../sdk/python/branchyard_client.py), **this page** | HTTP to `by serve` ([server](server.md)) with a bearer token | the token's principal, within its scopes, tenant and repositories |
| a Rust program | the [`branchyard-client`](../crates/branchyard-client/src/lib.rs) crate (HTTP), or the `branchyard` crate in-process ([architecture](design.md)) | HTTP, or the engine itself | the same as above, or the yard's own authority |
| anything else | the HTTP API directly, against [`schema/contract.json`](../schema/contract.json) | HTTP | the token's principal |

Both Python modules ship in `branchyard-sdk-<version>.zip` ([distribution](distribution.md)) and need nothing but the standard library (Python 3.9 or newer). Everything a module returns is the Rust type's serde form, the same bytes on every surface ([surfaces](surfaces.md)), so a result read through `by --json`, the HTTP API or either module is one shape.

The rest of this page is about the HTTP client: what a third-party surface needs to run conversations on Branchyard, see what they say, and give and inspect connector grants without ever holding a credential.

## The HTTP client

```python
from branchyard_client import Client, Grant

client = Client("http://127.0.0.1:8421", token_file=".branchyard/server/token")
app = client.repo("app")

op = app.task("Make the flaky parser test deterministic", harness="claude-code", budget_usd=2)
op = client.wait(op, check=True)              # polls GET /v1/operations/{id}
branch = op.result_branches[0]
print(branch.name, branch.state)              # "flaky-parser", "ready"
```

`Client(url, token=..., token_file=..., ca_file=..., timeout=30)` is one server; `client.repo(name)` is one served repository, and every `Repo` method is one route of the [API reference](server.md#api-reference). Methods that start work (`task`, `send`, `fork`, `reincarnate`, `merge`, `spawn`, `integrate`, `approve_plan`, `reject_plan`) return an `Operation` and `client.wait` polls it; everything else returns at once. Results are `Record`s: dicts whose keys also read as attributes (`op.state`, `branch.status.state`), a missing key reading as `None`, so a newer server's fields never break an older client. `op.raw` is the plain dict.

Every `POST` that creates an operation carries an `Idempotency-Key`, generated unless you pass `key=`: keep the key and a retry after a lost response returns the original operation (`op.replayed` is true) instead of starting another; the same key with a different body is `IdempotencyKeyReusedError` ([idempotency](server.md#idempotency)). `client.operation_by_key(key)` finds the operation a key created.

### Errors

Every refusal is a `BranchyardError` subclass carrying the server's stable `code`, its `message`, its `detail` and the HTTP `status` ([errors](server.md#errors)). Catch the class, read the code when it matters.

| Exception | Codes |
|---|---|
| `UnauthorizedError` | `unauthorized` |
| `NotFoundError` | `unknown_repo`, `unknown_branch`, `unknown_operation`, `unknown_trigger`, `not_found` |
| `DeniedError` | `denied` (the envelope or a branch's authority refused), `scope_required`, `repo_not_allowed` |
| `NotAllowedError` | what the server's operator did not allow: `connectors_not_configured`, `delegation_not_allowed`, `command_not_allowed`, `provider_not_allowed`, `secret_not_allowed`, `unapproved_tools_not_allowed`, ... |
| `InvalidRequestError` | `invalid_request`, `invalid_name`, `unknown_harness`, `cursor_out_of_range`, `body_too_large`, ... |
| `BranchBusyError` | `branch_busy`; `.holder` is the operation that holds the branch |
| `RunningError` | `running` (a turn runs in another engine) |
| `ConflictError` | `branch_exists`, `no_candidate`, `conflict`, `target_moved`, `already_merged`, `fenced`, `trigger_exists`, `no_plan`, `stale_revision`, ... |
| `CheckFailedError` | `check_failed`, `check_timed_out`, `check_not_started` |
| `QuotaExceededError` | `quota_exceeded`, `rate_limited`; `.retry_after` when the server said |
| `IdempotencyKeyReusedError` | `idempotency_key_reused`; `detail.operation` |
| `ServerError` | 5xx, `shutting_down` |
| `OperationFailedError` | an operation that ended `failed` or `interrupted`, from `client.wait(..., check=True)` or `Reply.wait()`; `.operation` is it |
| `TransportError` | the server could not be reached, or answered with something that is not its API |

The client retries nothing but its event stream. A `TransportError` on a `POST` means the request may or may not have been admitted: retry it with the same `key`.

## A surface: one branch per conversation

A chat assistant keeps a session per conversation. On Branchyard that session is a branch that lives on: the first message creates it, every later message continues it with another turn, and its worktree, its harness session and its grant persist between messages. `Surface` and `Conversation` are that pattern, so a bot needs one call per message:

```python
chat = app.surface(
    "slack",                                   # branches are named slack-<channel>
    harness="claude-code",
    budget_usd=5,                              # each turn's budget
    connectors=[Grant.parse("github:write:issues.*"), "linear:read"],
    delegation={"max_depth": 1, "max_children": 4, "harnesses": []},
    allow_delegation=True,
)

def on_message(channel: str, text: str) -> str:
    reply = chat.conversation(channel).say(text, busy="queue")
    return reply.wait(timeout=600).text        # what the harness said, this turn only
```

`conversation(key)` names the branch `branch_name(prefix, key)`: lowercased, letters, digits, `_`, `.` and `-` kept, the rest folded to `-`, so a channel id, a thread timestamp or a user id becomes a valid name (`slack-c0123`). `say(text)` is a `task` with the surface's defaults when the branch does not exist and a `send` when it does (a branch created between the look and the task is continued, not duplicated). It returns a `Reply`:

- `reply.wait(timeout)` blocks until the turn's operation finishes; a failed one raises `OperationFailedError`.
- `reply.text` is the harness's message text for that operation, this branch's `message_delta`s between the operation's feed `cursor` and `end_cursor`, so it is this turn's reply and nothing of earlier turns or of children.
- `reply.deltas()` yields the same text as it arrives, for a surface that streams or edits a message in place; it ends when the operation does.
- `reply.branch_info.state` says how the branch ended the turn: `ready`, `failed`, `budget_exceeded`, `awaiting_plan_approval`, ...

**Busy.** A message that arrives while the branch runs a turn meets `409 branch_busy`: the server admits one operation per branch. `say(text, busy=...)` decides what happens, with the same three policies as a [trigger that delivers to a branch](triggers.md#a-branch-that-lives-on):

| `busy` | Then |
|---|---|
| `queue` (default) | `say` waits for the running operation and sends after it, up to `timeout` seconds (`TimeoutError` past it); nothing is lost, nothing interrupts. Several queued messages are sent in the order their waits end. |
| `steer` | the text joins the running turn as steered input ([steering](delegation.md#steering-a-child)), and the `Reply` is that turn's (`reply.steered` is true, `reply.steer` the server's `Steer`). A harness that cannot take mid-turn input, or no turn by the time it arrives, falls back to `queue`. |
| `skip` | `say` raises `BranchBusyError`; for a heartbeat or anything the surface would rather drop than delay. |

**Costs.** Every message is a turn with the surface's budget; a branch that lives on accumulates cost and context like any session. `conversation.inspect()` shows `turns`, `cost_usd` and `remaining_usd`; `app.reincarnate(branch)` starts a fresh session from the branch's state with a generated handoff brief when a conversation has grown long ([checkpoints](checkpoints.md)); `app.cancel(branch)` stops a turn; `conversation.history(limit)` is the recent events.

**Triggers or the client?** A [trigger](triggers.md) with `deliver` does the same thing server-side for a webhook the server can verify (GitHub, Slack, Linear or a signed generic sender): no code of yours runs, but replies go to the branch's log and the trigger's conditions are the filter. The client is for a surface that needs the reply back (a bot answering in the channel), its own filtering, or a conversation keyed by something the webhook does not carry. The two compose: a trigger can create and continue the branch, and the client can read it (`app.said(branch, cursor)`, `app.stream(...)`).

## Streaming

`app.stream(cursor, branches=[...])` is the repository's activity as `FeedEntry`s from the [event stream](server.md#event-stream), reconnecting from the last position after a failure (`Last-Event-ID`), so a surface that mirrors every branch's progress needs one loop:

```python
stream = app.stream(cursor=None)               # from now; 0 for everything recorded
for entry in stream:                           # FeedEntry: seq, branch, at_ms, activity
    if entry.text:                             # a message_delta's text
        show(entry.branch, entry.text)
    elif entry.status is not None:             # the branch's status changed
        show(entry.branch, f"is now {entry.status}")
    remember(stream.cursor)                    # resume here after a restart
```

`entry.activity` is the recorded `Activity` (`{"harness": {...}}`, `{"prompt": ...}`, `{"status": {...}}`, `{"connector_call": {...}}`, ...; [surfaces](surfaces.md#output-types)). A connection quiet for `read_timeout` seconds (60 by default; the server comments every 15) is reconnected; `max_failures` connection failures in a row raise the last `TransportError`. For one branch's log by its own numbering, `app.events(branch, cursor)` pages `RecordedEvent`s and `app.said(branch, cursor)` is its text after a cursor, for a bot that polls.

## Connectors

A connector grant is what a branch's turns may call through the gateway ([connectors](connectors.md)): the harness holds a short-lived token, never a credential, and the gateway enforces the grant. From the client's side there are four facts to know.

**Giving a grant.** `connectors=[...]` on `task`, `send`, `fork` and `spawn`, and on a `surface`, takes `Grant`s, the flag string or the object; all three are one wire form:

```python
Grant.parse("github:write:issues.*")           # CONNECTOR[@ACCOUNT][:MODE[:OP,...]]
Grant.read("github", "issues.list", "pulls.list", account="work")
Grant.write("linear", confirm=True)            # write+confirm: may confirm effects that ask
str(grant), grant.to_json(), Grant.from_json(...)   # the flag, the object, and back
```

`Grant.parse` checks what the server checks (connector id, account, mode, operation globs) and raises `ValueError` with the server's words, so a bad grant fails in your process rather than as a `400`. A grant needs a home private to the branch, so `task` sets `isolated=True` when you give one unless you say otherwise.

**Constraints the server keeps.** The server refuses a grant it cannot honour: `NotAllowedError(connectors_not_configured)` at the request on a server without a gateway; without a private home the operation is admitted and fails as it runs, before a branch exists (`OperationFailedError` from `wait(check=True)`); and a grant above the principal's ceiling (`ceilings` in the server's configuration, [model gateway and ceilings](server.md#model-gateway-and-ceilings)) is cut to the ceiling. Writes a bundle marks as needing confirmation wait for an approval unless the grant says `write+confirm`: `app.approvals()` lists them (`Approval.about` names the connector and operation), `app.allow(id)` and `app.deny(id)` answer as the caller ([approvals](effects.md#approvals)), and `app.effects(branch=...)` is the ledger of what was performed, staged or undone ([effects](effects.md)).

**Propagation to children.** A conversation's harness may delegate (with `delegation` and `allow_delegation` on the surface). Every child's grant is computed from what the spawn asks for, else its seat's, else its parent's, and always intersected with its parent's: a child can be given less, never more, down the whole tree ([delegation](delegation.md#connectors)). So the grant a surface gives a conversation is the ceiling of everything that conversation does, however deep it delegates. A child's ask for a connector its parent lacks is refused `denied`, naming it: at once for a harness's `by spawn`, and as the spawn operation's failure (`OperationFailedError` with code `denied`) for a spawn over HTTP, which is admitted first. A `send` that names no `connectors` keeps the branch's grant; one that does replaces it, narrowed for a delegated child.

**Seeing what a branch holds.** `app.inspect(branch).grants` is the branch's grant as stored, a list of `Grant`s: what its turns' tokens carry and the most a child of its can be given. It is the same `grants` field `by inspect --json` prints and the harness-side `branchyard.inspect()` returns, so a surface, an operator and a parent harness read one truth rather than inferring the intersection themselves.

```python
parent = app.inspect("slack-c0123")
[str(g) for g in parent.grants]                # ["github:write:issues.*", "linear:read"]
[str(g) for g in app.inspect("kid").grants]    # ["github:read:issues.list", "linear:read"]
for child in app.children("slack-c0123"):      # every branch it delegated to, directly or below
    print(child.name, child.state)
```

## The rest of the API

`Repo` also covers what the routes cover: `branches`, `branch`, `exists`, `remove`, `diff`, `children`, `graph`, `apply_graph`, `operations`, `event_page`, `wait_for` (asks again past the server's 30 s cap), the inbox as a branch (`inbox`, `ask`, `report`, `escalate`, `answer`), plans (`plan`, `approve_plan`, `reject_plan`), artifacts and scratch areas, and `Client` covers `repos`, `harnesses`, `me`, `healthy` and triggers (`triggers`, `trigger`, `create_trigger`, `delete_trigger`, `enable_trigger`, `test_trigger`, `trigger_runs`). `client.request(method, path, body, key=...)` is the escape hatch for a route the client has no method for; it returns the parsed JSON and raises the same typed errors. The module's docstrings are the reference; `docs/server.md` is the wire's.

## What the client does not do

It adds no authority: what a token may do, the server decides, and a `DeniedError` or `NotAllowedError` is the server's answer, not the client's. It does not route replies back to a channel on its own (your bot does, with `Reply`), does not compact or reincarnate a long conversation unasked, and retries nothing but the event stream. There is no TypeScript or Go client yet; the HTTP API and `schema/contract.json` are what one would be generated from.

## Tests

- `tests/test_sdk_client.py`: the client against a fake server scripted per route (stdlib only): the token, the idempotency key and a replay, the grant with its private home, every error code's class and detail, `wait` and `wait_for`, the stream's parsing, filtering, resumption after a cut and idle hook, a reply's text between the operation's cursors and while running, the first message creating and the next continuing a branch, the three busy policies and a refused steer's fallback, approvals and effects, grant round trips and refusals, branch names.
- `crates/branchyard-cli/tests/sdk_client.rs` runs `tests/sdk_client_e2e.py` against a real `by serve` with the fake agent, a fake Anvil packager and delegation allowed: a surface's conversation created with a grant and an envelope, its harness spawning a child that asks for more than the parent holds and getting the intersection (visible in `inspect().grants`), a second message continuing the branch and keeping the grant, both turns' text on the stream, `skip` and `queue` against a slow turn, a replayed and a reused idempotency key, and the server's refusals (`unknown_branch`, a grant without a home, a child granted a connector its parent lacks) as typed errors.
- `tests/test_schema_validation.py` and `crates/branchyard-client/tests/contract.rs`: the wire contract, with a grant entry in either form.
