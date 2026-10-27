# SDKs

Written 10 October 2026, revised 25 October 2026.

Branchyard has three programmatic surfaces besides `by` itself. Which one you use depends on where your code runs.

| Your code is | Use | How it reaches Branchyard | Acts as |
|---|---|---|---|
| a harness turn inside a branch (Claude Code, Codex, Gemini CLI, ... running a task) | [`sdk/python/branchyard.py`](../sdk/python/branchyard.py), the MCP tools, or `by` in the shell ([delegation](delegation.md#python)) | `by ... --json`, found from `BRANCHYARD_BY` or `PATH` | that branch, on its own descendants only |
| a service outside: a Slack, Discord or Teams bot, a web app, a scheduler, a connector's own back end | [`sdk/python/branchyard_client.py`](../sdk/python/branchyard_client.py), **this page** | HTTP to `by serve` ([server](server.md)) with a bearer token | the token's principal, within its scopes, tenant and repositories |
| a Rust program | the [`branchyard-client`](../crates/branchyard-client/src/lib.rs) crate (HTTP), or the `branchyard` crate in-process ([architecture](design.md)): `Yard::open_configured` reads `branchyard.toml` as `by` does ([configuration](setup.md#configuration)), `Yard::up` brings the configured backends up ([up](up.md)) | HTTP, or the engine itself | the same as above, or the yard's own authority |
| anything else | the HTTP API directly, against [`schema/contract.json`](../schema/contract.json) | HTTP | the token's principal |

Both Python modules ship in `branchyard-sdk-<version>.zip` ([distribution](distribution.md)) and install as one distribution, `branchyard` ([install](#install)); they need nothing but the standard library (Python 3.9 or newer). Everything a module returns is the Rust type's serde form, the same bytes on every surface ([surfaces](surfaces.md)), so a result read through `by --json`, the HTTP API or either module is one shape.

The rest of this page is about the HTTP client: what a third-party surface needs to run conversations on Branchyard, see what they say, and give and inspect connector grants without ever holding a credential. It starts with the shortest way to a first run, which needs no server at all.

## Install

The SDK drives a `by`, so there are two things to install. Neither is published yet: there is no release of `by`, and no `pip install branchyard` from an index. From a checkout:

```sh
cargo install --locked --path crates/branchyard-cli     # `by`, onto PATH
pip install ./sdk/python                                # branchyard and branchyard_client
```

`pip install ./branchyard-sdk` does the same from the extracted release archive (`unzip branchyard-sdk-0.0.1.zip` first). `by` is found from `BRANCHYARD_BY`, else `PATH`; a missing one is a `LocalError` that says so and gives the `cargo install` line. [`sdk/python/README.md`](../sdk/python/README.md) is this install and the first run in one page.

`sdk/python/pyproject.toml` builds with setuptools (pip fetches it, or `--no-build-isolation` uses the one installed) and installs the two top-level modules, `branchyard` and `branchyard_client`, with no dependencies. Its version is the workspace's: `tools/package.py` refuses to build the archives when the two differ, and `tests/test_distribution.py` checks it, builds the project from the extracted archive and imports both modules from what was built. Nothing needs installing inside a harness: a delegated turn gets both modules written to `.branchyard/sdk/python/`, first on its `PYTHONPATH` ([delegation](delegation.md#python)), so `import branchyard` works there as it did, and `import branchyard_client` too.

## First run

In a git repository with a commit, with `by` and the SDK [installed](#install). There is no server to start, no token to copy and no port to pick: `Client.local` runs a `by serve` of this process's own on the repository and stops it when the block ends ([local mode](#local-mode)).

```python
from branchyard_client import Client

with Client.local(".") as client:
    app = client.repo("app")                       # Client.local serves the repository as "app"
    op = app.task("Make the flaky parser test deterministic", name="flaky-parser",
                  budget_usd=2, policy={"preset": "edit-worktree"})
    op = client.wait(op, check=True)               # raises if the request failed, or the branch it made did
    branch = op.result_branches[0]
    print(branch.name, branch.state)               # flaky-parser ready
    print(branch.candidate)                        # its commit and how much it changed; None if nothing
```

The harness is the repository's default; `harness="codex"` names another. Its tool requests are answered by `policy`, which denies them unless told otherwise (a server has no terminal to ask on), so a harness that must edit files needs the `edit-worktree` preset ([permission presets](egress.md#permission-presets)) or `{"mode": "allow"}`. The branch belongs to the repository, not to the script: `by diff flaky-parser` and `by merge flaky-parser` work on it afterwards.

`name=` is optional. Without it the branch is named from the prompt, as `by run` names one: lower-case words joined by hyphens, cut at a word boundary to 40 characters (`make-the-flaky-parser-test-deterministic`), with `-2`, `-3`, ... added when the name is taken, so the same prompt twice makes `...-2`. The 40 characters include that suffix, and a `by fan`'s `-<harness>` (`create-a-file-named-fan-txt-gemini-cli`): the words give way, so the end that tells one name from another is never cut off in a table. A `name=` that is taken is `ConflictError` (`branch_exists`).

`wait(op, check=True)` raises for two failures. An operation that failed, such as one refused as it ran, raises `OperationFailedError`. An operation that succeeded can still have made a branch whose harness turn failed (it crashed, never answered, or ran out of its budget): that raises `BranchFailedError`, a subclass of `OperationFailedError`, naming the branch and its state (`failed`, `budget_exceeded` or `interrupted`), with `reason` or `limit`, and the finished `operation`. Without `check=True` neither raises: read `op.state` and `op.failed_branches`.

With the fake ACP agent standing in for a model (a checkout builds it: `cargo build -p branchyard-runtime --bin fake-acp-agent`), the same calls, with `args=["--harness-command", "gemini-cli=/path/to/branchyard/target/debug/fake-acp-agent"]` on `Client.local` (an absolute path: the server runs in the repository) and `harness="gemini-cli"` on the task, print:

```text
flaky-parser ready
{'commit': 'e988d1ebf3f244d0ac4c261e9ecbd9610f95a62b', 'files_changed': 1, 'insertions': 1, 'deletions': 0}
```

and a turn that fails, as the fake's `EXIT` prompt does, raises instead:

```text
BranchFailedError: failed: branch flaky-parser ended failed: the turn's outcome is unknown: the harness connection closed before the turn ended. to fix: read why with `by log flaky-parser` or app.events("flaky-parser"), then app.send("flaky-parser", "try again")
```

[`examples/python/`](../examples/python/) has this as scripts that run as they are: `first_run.py` (the diff summary, and exit 1 for a failed branch), `stream_events.py` (the [event stream](#streaming) until the branch settles) and `delegate.py` (spawn, wait and integrate through [`connect()`](#one-api-inside-and-outside-a-harness); it runs inside a harness turn, or as a named branch of a server that allows delegation). Each takes `--command CMD` to run the harness as a stand-in, so they need no model; `tests/test_sdk_examples.py` runs them that way against the built `by`.

### Before real harnesses

A real harness has to be installed and logged in. The first run does not call `Local(".").up()`, because `up` can install software; ask it what is missing first, with `plan=True`:

```python
from branchyard_client import Local

plan = Local(".").up(plan=True)               # changes nothing
for step in plan.todo:                        # the steps that are not ready, in report order
    print(step.name, step.state, step.detail)
# harness.claude-code needs_person not installed: [harnesses] install is "never" here (set install = "ask" or "auto" in your user configuration to allow it)
```

`Local(".").up(yes=True)` then installs what the `[harnesses]` policy allows and lists what only a person can do (log in, set a key) ([up](up.md)); `Local(".").doctor()` is `by doctor`.

## The HTTP client

For a service that is not on the machine that does the work, or that outlives a run: serve the repository, then connect.

```sh
by serve --repo app=.        # listening on 127.0.0.1:8421; the token is created in .branchyard/server/token
```

```python
from branchyard_client import Client

client = Client("http://127.0.0.1:8421", token_file=".branchyard/server/token")
app = client.repo("app")                       # the NAME of --repo NAME=PATH; unnamed, the repository's directory name

op = app.task("Make the flaky parser test deterministic", name="flaky-parser", harness="claude-code",
              budget_usd=2, policy={"preset": "edit-worktree"})
op = client.wait(op, check=True)               # long-polls GET /v1/operations/{id}?wait=
branch = op.result_branches[0]
print(branch.name, branch.state)               # flaky-parser ready
```

The repository's name is the one the server was started with; a name it does not serve is `NotFoundError` (`unknown_repo`) with the fix in its message, and `client.repos()` lists what it serves. A server that is not running is a `TransportError` that says what to start. The token is on the first line of `DATA-DIR/token` (`--data-dir`, default `.branchyard/server` in the first repository), created with mode 600 when missing ([server](server.md#running-it)).

`Client(url, token=..., token_file=..., ca_file=..., timeout=30)` is one server; `client.repo(name)` is one served repository, and every `Repo` method is one route of the [API reference](server.md#api-reference). Methods that start work (`task`, `send`, `fork`, `reincarnate`, `merge`, `check`, `spawn`, `integrate`, `approve_plan`, `reject_plan`, `map`, `resume_map`) return an `Operation` and `client.wait(op, timeout=None, check=False)` waits for it, with long polls the server answers as the operation finishes (each at most 30 s, asked again until `timeout`, then `TimeoutError`); everything else returns at once. Results are `Record`s: dicts whose keys also read as attributes (`op.state`, `branch.status.state`). A key the answer does not carry reads as `None`, so a newer server's fields never break an older client and an older server's absent fields are only `None`; the price is that a typo (`op.stat`) is `None` too. `dir(op)` lists the keys the record holds beside its properties, and the class docstrings list the fields. `op.raw` is the plain dict. `Operation`, `BranchInfo`, `Inspection`, `FeedEntry`, `Message` and `CheckReport` declare their fields as annotations, which editors that read the source (Pyright, Pylance) complete and check; see [what the client does not do](#what-the-client-does-not-do) for what mypy sees.

Every `POST` that creates an operation carries an `Idempotency-Key`, generated unless you pass `key=`: keep the key and a retry after a lost response returns the original operation (`op.replayed` is true) instead of starting another; the same key with a different body is `IdempotencyKeyReusedError` ([idempotency](server.md#idempotency)). `client.operation_by_key(key)` finds the operation a key created.

### Errors

Every refusal is a `BranchyardError` subclass carrying the server's stable `code`, its `message`, its `detail` and the HTTP `status` ([errors](server.md#errors)). Catch the class, read the code when it matters. `kind` is the same value as `code`, by the name `by --json` gives it: `branchyard.py` raises these same classes (its names for them are these objects), mapping `by --json`'s kinds through the same table (`error_for_code`), so one `except` clause serves code inside a harness and outside it.

| Exception | Codes |
|---|---|
| `UnauthorizedError` | `unauthorized` |
| `NotFoundError` | `unknown_repo`, `unknown_branch`, `unknown_operation`, `unknown_trigger`, `unknown_checkpoint`, `unknown_knowledge`, `unknown_message`, `not_found` |
| `DeniedError` | `denied` (the envelope or a branch's authority refused), `scope_required`, `repo_not_allowed` |
| `NotAllowedError` | what the server's operator did not allow: `connectors_not_configured`, `delegation_not_allowed`, `command_not_allowed`, `provider_not_allowed`, `secret_not_allowed`, `unapproved_tools_not_allowed`, ... |
| `InvalidRequestError` | `invalid_request`, `invalid_name`, `unknown_harness`, `cursor_out_of_range`, `body_too_large`, ... |
| `BranchBusyError` | `branch_busy`; `.holder` is the operation that holds the branch |
| `RunningError` | `running` (a turn runs in another engine; a wait that timed out) |
| `NotRunningError`, `SteerRefusedError` | `not_running`, `steer_refused`: a steer with no turn to join, or one the turn did not take |
| `UnsupportedError` | `unsupported`, `harness_unavailable` |
| `ConflictError` | `branch_exists`, `no_candidate`, `conflict`, `target_moved`, `already_merged`, `fenced`, `trigger_exists`, `no_plan`, `checkpoint_unavailable`, `service_held`, `invalid_candidate`, ...; `StaleRevisionError` (`stale_revision`) is one |
| `CheckFailedError` | `check_failed`; `CheckTimedOutError` (`check_timed_out`) and `CheckNotStartedError` (`check_not_started`) are ones. `checks`, `integrate_together`, `unsettled` and `own_work` read its `detail` |
| `QuotaExceededError` | `quota_exceeded`, `rate_limited`; `.retry_after` when the server said |
| `IdempotencyKeyReusedError` | `idempotency_key_reused`; `detail.operation` |
| `ServerError` | 5xx, `shutting_down`; from `by`, the kinds `git`, `io`, `state` and `harness` |
| `OperationFailedError` | an operation that ended `failed` or `interrupted`, from `client.wait(..., check=True)` or `Reply.wait()`; `.operation` is it |
| `BranchFailedError` | a subclass of the above: the operation succeeded but a branch it made ended `failed`, `budget_exceeded` or `interrupted`. `.code` and `.state` are that state, `.branch` the name, `.branch_info` its `BranchInfo`, `.reason` or `.limit` why, `.branches` every such branch (a map can have several) |
| `TransportError` | the server could not be reached, or answered with something that is not its API. A connection refused says to start `by serve`, or use `Client.local` |
| `LocalError` | `Client.local` or `Local` could not run `by`, or it printed no JSON. A `by` that is not installed says where to get one |

The client retries nothing but its event stream. A `TransportError` on a `POST` means the request may or may not have been admitted: retry it with the same `key`.

**Errors end with the fix.** An error the client knows a remedy for ends with `to fix: ...`, after what went wrong: a missing `by` (`cargo install --locked --path crates/branchyard-cli`), a connection refused (`by serve --repo app=.`, or `Client.local(".")`), an `unknown_repo` (`client.repos()` lists the names), a `BranchFailedError` (`by log NAME`, then `app.send(...)`). A message the server wrote keeps its own words; the client adds `to fix:` only where the server's text has none. A new error that has a known remedy follows the same rule.

## One API inside and outside a harness

A meta-harness, code that splits work across child branches and lands what they make, can run as a harness turn (a script its harness runs) or as a service outside. `connect()` gives it the same calls in both places: inside a delegating turn (`BRANCHYARD_DELEGATION` set) it runs `branchyard.py`, acting as the turn's branch; given a `Client` or a `Repo`, or with `BRANCHYARD_REMOTE` and `BRANCHYARD_TOKEN_FILE` set (and `BRANCHYARD_REPO` when the server serves several), it speaks HTTP, acting as `branch` (default `BRANCHYARD_BRANCH`); with neither, it runs `branchyard.py` with your authority on the repository here.

```python
from branchyard_client import CheckFailedError, connect

by = connect()                                  # or connect(client.repo("app"), branch="lead")
kid = by.spawn("Make the parser test deterministic", harness="codex", name="parser", budget_usd=0.5)
done = by.wait(kid.name, timeout=1800)          # an Inspection; RunningError past the timeout
try:
    by.integrate(kid.name)                      # a Merged; integrate(a, b) a MergedAll
except CheckFailedError as error:               # the same class, kind and detail on both sides
    by.send(kid.name, f"The check failed: {error.message}")
by.report("parser fixed", result={"files": done.result["files"]})
```

The `Delegator` it returns has `spawn`, `send`, `steer`, `integrate`, `check`, `wait`, `wait_any`, `wait_all`, `inspect`, `events`, `report` and `report_status`, with `branchyard.py`'s argument names and order, which `OneApi` in `tests/test_sdk_client.py` checks for every call by comparing the two signatures. `spawn` is the portable subset in full, all 23 of `branchyard.spawn`'s arguments: `harness`, `name`, `base`, `budget_usd`, `max_turns`, `max_minutes`, `check` (a command line or an argv), `max_depth`, `max_children`, `harnesses`, `deny`, `seat`, `depends_on`, `after`, `bindings` (a dict from scratch area to `read_only` or `exclusive_write`), `connectors` (grants in any form `Grant` takes), `plan`, `model`, `result`, `why`, `owns`, `land` and `review` (a harness ID or `{harness, model, rubric, budget_usd}`). Each reaches the backend in its own terms: `max_minutes` is `--max-minutes` inside and `budget.max_seconds` over HTTP, `bindings` are `--bind` flags and `{scratch, access}` entries. Any other keyword goes to the backend's own spawn as it is (`Repo.spawn`'s over HTTP: `policy`, `priority`, `require_labels`, ...), and is not portable. Both backends return `Record`s holding the whole answer: `BranchInfo` from `spawn` and `send`, `Inspection`, `Steer`, `Message`, `Reported`, `CheckReport`, `Waited`, and a `Record` for a `Merged` or `MergedAll`. A dict loses no field, where `branchyard.py`'s dataclasses keep only those they name. Both raise the same exceptions: over HTTP a call that is an operation is waited for, and one that fails raises the class of its error's code, with `operation` set, as the call inside would. One difference stays, and it is the server's: its spawn and send operations last until the turn they start settles, so outside, `spawn` and `send` return a settled branch, where inside they return as the turn starts.

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

- `reply.wait(timeout)` blocks until the turn's operation finishes; a failed one raises `OperationFailedError`, and a turn that ended with the branch `failed`, `budget_exceeded` or `interrupted` raises `BranchFailedError`. `reply.text` still reads what the harness said before it stopped.
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

The connection opens when the loop starts, and `cursor=None` means from then, so a stream opened after `app.task(...)` can miss what the branch did first. To follow one request from its start, pass the position it was admitted at and name the branch: `app.stream(op.cursor, branches=["flaky-parser"])`. [`examples/python/stream_events.py`](../examples/python/stream_events.py) does that and stops at the first status that means the branch will not move by itself (`ready`, `no_changes`, `failed`, `budget_exceeded`, `interrupted`):

```text
  78  s2-ex  status: running
  80  s2-ex  prompt: WRITE s2.txt=hi
  81  s2-ex  harness: session_started
  82  s2-ex  says: wrote s2.txt
  83  s2-ex  harness: turn_ended
  85  s2-ex  candidate: 1 file(s), +1 -0
  87  s2-ex  status: ready
s2-ex: ready
```

`with app.stream() as stream:` closes the connection however the block ends, a `break` or an exception included; without it, call `stream.close()`. `entry.activity` is the recorded `Activity` (`{"harness": {...}}`, `{"prompt": ...}`, `{"status": {...}}`, `{"connector_call": {...}}`, ...; [surfaces](surfaces.md#output-types)). A connection quiet for `read_timeout` seconds (60 by default; the server comments every 15) is reconnected; `max_failures` connection failures in a row raise the last `TransportError`. For one branch's log by its own numbering, `app.events(branch, cursor)` pages `RecordedEvent`s and `app.said(branch, cursor)` is its text after a cursor, for a bot that polls.

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

**Seeing what a branch says of itself.** `app.inspect(branch).reported` is the branch's own latest account of what it is doing, a `StatusReport` (`state`, `summary`, `link`, `at_ms`, and `ttl_seconds`, `expires_at_ms`, `stale` and `stale_because` when they apply), or `None` until its harness states one with `report_status` ([self-reported status](delegation.md#self-reported-status)). It sits beside `status`, which comes from the protocol and may disagree: `inspect("kid").state` can be `running` while `reported.state` is `blocked`. It is a claim, not a result, and it is not cleared by a new turn, so `at_ms` says how old it is and `stale` whether it still holds: the branch ended, a later turn finished without a new one, or its time-to-live (`report_status(..., ttl_seconds=600)`) passed.

**Asking for a result in a shape.** `app.spawn(parent, prompt, result={"schema": {...}, "required": True})` gives the child a [result contract](delegation.md#result-contract): a JSON Schema in a small subset, which the server checks when it is asked for. The child's `report` carries a `result` that is checked against it, and `app.inspect(child).result` (also `.result_contract`) reads the latest back as JSON, as does each settled child of `wait_for`. `app.report(branch, text, result=...)` reports as `branch`; a result that does not match raises `DeniedError` naming the path and the rule.

## Local mode

A program that is not `by` and has no server to talk to can run one of its own. `Client.local(root, name="app")` starts `by serve` on the repository at `root`, on a free loopback port with a fresh token in a private data directory, and returns a client on it; `close()`, or leaving the `with`, stops the server and removes the directory. The server reads the repository's configuration as every `by` command does and keeps its branches in the repository's `.branchyard/`; only its operation registry is in the private directory. `by` is `BRANCHYARD_BY`, else `by` on `PATH`, else the `by=` argument; `args` are more `by serve` flags (`--allow-delegation` lets the harness's children be spawned, `--harness-command H=CMD` runs harness `H` as `CMD`). It is the [first run](#first-run) above; the name is `app` unless you pass `name=`. A `by` that cannot be run is a `LocalError` that says how to get one.

`Local(root)` is this machine's `by` for what a server does not do, and the step before a real harness ([above](#before-real-harnesses)): `up(plan=False, yes=False)` returns a `Plan` (`steps`, `todo`, `ready`, `step(name)`; [up](up.md)), `doctor()` a `Report` (`checks`, `failed`, `status`), `harnesses(refresh=False)` the inventory, and `run(*args)` any command with `--json`. A `by` that cannot run or prints no JSON is a `LocalError` carrying what it said; `by`'s own `{"error": {"kind", "message"}}` is a `BranchyardError` with that kind, such as `config` for a configuration file that does not load.

## The rest of the API

`Repo` also covers what the routes cover: `branches`, `branch`, `exists`, `remove`, `diff`, `children`, `graph`, `apply_graph`, `operations`, `event_page`, `wait_for` (asks again past the server's 30 s cap), [checkpoints](checkpoints.md) (`fork(branch, prompt, at=N)` forks from checkpoint N, 0 being the base, refused `unknown_checkpoint` before anything is queued; `rewind(branch, to)` resets a settled branch to one and returns a `Rewound`, `{branch, from, to, commit, session}`, at once: it is not an operation), the inbox as a branch (`inbox`, `ask`, `report`, `escalate`, `answer`), plans (`plan`, `approve_plan`, `reject_plan`), artifacts and scratch areas, and `Client` covers `repos`, `harnesses`, `me`, `healthy` and triggers (`triggers`, `trigger`, `create_trigger`, `delete_trigger`, `enable_trigger`, `test_trigger`, `trigger_runs`, `set_trigger_secret`). The families below are the Rust client's, under its names where Python has no clash; each `POST` carries an idempotency key, generated unless the method takes `key=`.

| Family | Methods | Returns |
|---|---|---|
| [Maps](map.md) | `map(prompt, items, name=, schema=, concurrency=, retries=, total_usd=, reduce=, remove_done=, retry_failed=, key=, **task)`, `resume_map(name, retry_failed=, key=)`, `maps()`, `map_report(name)`, `remove_map(name)` | an `Operation` of kind `map` for the first two; `MapSummary`s, the `MapReport` |
| [Task records](task-repos.md) | `task_records()`, `task_record(task)` (its ID, a prefix of four or more characters, or an attempt) | `TaskView`s, each with its attempts |
| [Knowledge](knowledge.md) | `knowledge(status=)`, `knowledge_entry(id)`, `add_knowledge(text, path=, kind=, propose=)`, `adopt_knowledge(id)`, `reject_knowledge(id, reason=)`, `edit_knowledge(id, text=, path=, kind=)` (`""` clears), `remove_knowledge(id)`, `export_knowledge()`, `distill(branch)` | `KnowledgeEntry`s; `{markdown, entries}`; `Distilled` |
| [Effects](effects.md) | `reconcile_effects()`, `undo_plan(branch, to=0)`, `undo(branch, to=0, only=())` | `Reconciled`, `UndoPlan`, `UndoReport` (`plan`, `outcomes`); files are not rewound |
| [Scratch locks](storage.md) | `lock_scratch(branch, name, key=)`, `unlock_scratch(branch, name, key=)`, `scratch_lock_state(name)` | the `ScratchLock`; nothing; the lock or `None` |
| [The fleet](registry.md) (on `Client`) | `well_known()` (sent without the token), `inventory()`, `services(kind=)`, `register_service(kind, id=, capabilities=, endpoints=, health=, weight=, ttl_seconds=)`, `deregister_service(id)`, `reclaim_services()` | `WellKnown`; `WorkerInventory`s; `Service`s; the registration needs the `admin` scope, and another principal's live record is `ConflictError` (`service_held`) |
| [Web Push](companion.md) (on `Client`) | `push_info()`, `subscribe_push(endpoint, p256dh, auth, kinds=())`, `unsubscribe_push(endpoint)`, `test_push()` | `PushInfo`; `PushResult`; `NotFoundError` on a server without the companion, `NotAllowedError` (`push_not_enabled`) when it sends none |

```python
op = app.map("Find the license of {{item.repo}}", [{"id": "a", "repo": "x/a"}, {"id": "b", "repo": "x/b"}],
             name="licenses", schema={"type": "object", "properties": {"license": {"type": "string"}}},
             harness="codex")
client.wait(op, check=True)
print(app.map_report("licenses").rows)        # each item's latest row

entry = app.add_knowledge("Run the parser tests before merging", path="src/parser/**")
app.lock_scratch("build", "cache"); app.unlock_scratch("build", "cache")
```

`map`'s `items` is a list of values, each with an `id` field (a string or a whole number), or a dict from id to value; a value without an id is a `ValueError` here, since the server's default id is a BLAKE3 digest the standard library cannot compute. The rest of `map`'s keywords are every item branch's options, as `task` takes them, less `name`, `harnesses`, `seats`, `plan` and `goal`. `client.request(method, path, body, key=...)` is the escape hatch for a route the client has no method for; it returns the parsed JSON and raises the same typed errors. The module's docstrings are the reference; `docs/server.md` is the wire's.

Which routes have no method is checked: `RouteParity` in `tests/test_sdk_client.py` takes every route the server registers (`docs/facts.json`), calls each method that covers it against a recorder and checks the path it requested, or finds the route in `EXCUSED` with its reason. Only routes that are not a client's are excused: the companion's pages and its pairing (a phone redeeming a code for a token of its own), `/metrics`, the gateway's JWKS and a trigger's webhook. A new server route fails the test until it is one or the other.

## What the client does not do

It adds no authority: what a token may do, the server decides, and a `DeniedError` or `NotAllowedError` is the server's answer, not the client's. It does not route replies back to a channel on its own (your bot does, with `Reply`), does not compact or reincarnate a long conversation unasked, and retries nothing but the event stream. There is no TypeScript or Go client yet; the HTTP API and `schema/contract.json` are what one would be generated from.

Not done:

- **No wheel, no PyPI package, no bundled `by`.** `pip install branchyard` finds nothing; the SDK installs from a checkout or the release archive and needs a `by` installed beside it (`cargo install`, as [above](#install)). A wheel with the platform's `by` inside, built by the release workflow, is what would change that. The source distribution is built in the tests, a wheel is not (some distributions' patched setuptools cannot build one on every Python they ship).
- **No async.** Every call blocks, `reply.wait()` and the event stream included; call them from a thread, or from `asyncio.to_thread`. There is no `AsyncClient`.
- **No TypeScript client.** Nothing generates one yet: `schema/contract.json` has the types but no paths or methods, and the 90 routes are listed in `docs/facts.json` and the server's router only.
- **No `py.typed`.** A marker needs a package directory, and both modules are single files that Branchyard also writes, by name, into a delegating turn (`crates/branchyard/src/projection.rs`) and into the archives. So mypy treats the installed modules as untyped (`Any`). What is done instead: every public name has a docstring (a test fails on one without), `Operation`, `BranchInfo`, `Inspection`, `FeedEntry`, `Message` and `CheckReport` declare their fields for editors that read the source, and `dir(record)` lists the keys a record holds. Records still read a missing key as `None`, so `op.stat` is `None` and not an error.
- **Real harnesses.** The quickstart, the examples and their tests run against the fake ACP agent and the built `by`; no model is called. The `edit-worktree` policy, and what a given harness does with it, is as `docs/egress.md` describes, not checked here.
- `connect()` covers the delegation calls above, not `delegate`, `brief`, graphs, storage or the inbox, which stay on each module's own API.
- Typed records for the newer families (maps, knowledge, the fleet and Web Push answer plain `Record`s), and a map item's default id when it has none.

## Tests

- `tests/test_sdk_client.py`: the client against a fake server scripted per route (stdlib only): the token, the idempotency key and a replay, the grant with its private home, every error code's class and detail, `wait` and `wait_for`, the stream's parsing, filtering, resumption after a cut, idle hook and closing as a context manager, `rewind` and `fork(at=)` with their refusals, a reply's text between the operation's cursors and while running, the first message creating and the next continuing a branch, the three busy policies and a refused steer's fallback, approvals and effects, maps and task records, knowledge, the fleet (discovery without the token, services, inventory), undo and reconciling, scratch locks, a trigger's secret and Web Push, each with its request's shape and a typed refusal, grant round trips and refusals, branch names, and the route parity above.
- `crates/branchyard-cli/tests/sdk_client.rs` runs `tests/sdk_client_e2e.py` against a real `by serve` with the fake agent, a fake Anvil packager and delegation allowed: a surface's conversation created with a grant and an envelope, its harness spawning a child that asks for more than the parent holds and getting the intersection (visible in `inspect().grants`), a second message continuing the branch and keeping the grant, both turns' text on the stream, `skip` and `queue` against a slow turn, a replayed and a reused idempotency key, and the server's refusals (`unknown_branch`, a grant without a home, a child granted a connector its parent lacks) as typed errors; then one family after another for real: `well_known`, a service registered, listed and deregistered, knowledge added, decided, edited, exported and distilled (`unknown_knowledge` typed), a scratch lock taken and released, an undo plan and undo, the task records, a two-item map waited for, shown, resumed and forgotten, a generic trigger's generated secret, and `push_info` without the companion.
- One API: `OneApi` in `tests/test_sdk_client.py` runs one script through `connect()` inside, over a fake `by` as `branchyard.py` runs it, and outside, over the fake server, and checks that the results and the refusals (`unknown_branch`, `denied`, and a failed check, an operation's failure over HTTP) are the same; `OneErrorHierarchy` checks that both modules' classes are one.
- One API, the signatures: `OneApi` compares each `Delegator` call's parameters, names, order and defaults, with `branchyard.py`'s, and runs `spawn` with all 23 arguments inside (the flags a fake `by` receives) and outside (the request body the fake server receives).
- A failed branch: `Waiting` in `tests/test_sdk_client.py` ends operations that succeeded with a branch `failed`, `budget_exceeded` or `interrupted` and checks `wait(check=True)` raises `BranchFailedError` naming it, with its reason or limit and a `to fix:`; that a ready, no-change or merged branch does not; a map's several failures; that a failed operation stays an `OperationFailedError`; and that a `Reply` still reads what the harness said.
- Errors that end with the fix: a missing, unreadable or not executable `by`, a connection refused (a mocked `urllib` error, so no port is freed for it), and an `unknown_repo` with and without the server's own hint.
- Documentation and typing: every public name the module exports has a docstring (a test fails on one without), and `dir(record)` lists its keys.
- `tests/test_sdk_examples.py`: `examples/python/` against a built `by` and the fake ACP agent in a temporary repository: `first_run.py` makes a change and prints its status and diff summary, says when nothing changed, exits 1 for a branch whose turn failed (the fake agent exits mid-turn) with the branch named and a fix, and reports a taken name and a missing `by` without a traceback; `stream_events.py` prints the activity in order until the branch is ready, and stops at a failed one; `delegate.py` spawns, waits for and integrates a child as a branch of a server of its own, and says it has nothing to act as when it runs outside both. It finds `by` in `BRANCHYARD_BY`, else `target/debug`, else `PATH`, and skips without one, so the Python job, which builds nothing, passes; `crates/branchyard-cli/tests/sdk_client.rs` runs it with the `by` and agent it was built with and fails if it skipped, so `cargo test` runs it for real.
- `tests/test_schema_validation.py` and `crates/branchyard-client/tests/contract.rs`: the wire contract, with a grant entry in either form.
- `tests/test_distribution.py`: the README, the project URLs and the classifiers reach the source distribution built from the archive.
- Local mode: `tests/test_sdk_client.py` over a fake `by` (the server started with the arguments `Client.local` documents, its token read, stopped and removed on close; a server that does not start reported with what it said; `Local.up`, `doctor`, `harnesses` and `run` with their flags, `by`'s own error as the typed error, a missing answer as `LocalError`), and `crates/branchyard-cli/tests/sdk_client.rs` starting the real `by serve` through `Client.local` and stopping it.
