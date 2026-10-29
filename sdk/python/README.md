# Branchyard for Python

Two Python modules for [Branchyard](https://github.com/vamsiramakrishnan/branchyard), which runs coding-agent
harnesses (Claude Code, Codex, Gemini CLI and others) on git branches. Standard library only, Python 3.9 or newer.

| Module | For code that runs | How it reaches Branchyard |
|---|---|---|
| `branchyard_client` | anywhere: a script, a bot, a service | `by serve`'s HTTP API, or a `by serve` it starts itself (`Client.local`) |
| `branchyard` | inside a harness turn that Branchyard runs, delegating work to child branches | `by ... --json`, acting as that turn's branch |

Not on PyPI yet, so there is no `pip install branchyard` to run today. Both modules need a `by` to talk to.

## Install

Install the released CLI without a Rust toolchain or source checkout:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --no-setup
by --version
```

Download `branchyard-sdk-<version>.zip` from the [latest release](https://github.com/vamsiramakrishnan/branchyard/releases/latest)
and extract it, then install both Python modules:

```sh
python3 -m pip install ./branchyard-sdk
```

From an existing checkout, use `python3 -m pip install ./sdk/python`. To build the CLI from that checkout instead,
run `cargo install --locked --path crates/branchyard-cli` with the repository's pinned Rust toolchain.

`by` is found from `BRANCHYARD_BY`, else `PATH`. The installer uses `~/.local/bin` by default; open a new terminal
or add that directory to `PATH` if needed. See [distribution](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/distribution.md#install-by)
for platform requirements and the `--runtime` build.

## Run something

In a git repository with at least one commit. No server to start, no token to copy: `Client.local` runs a `by serve`
of its own on the repository and stops it when the block ends.

```python
from branchyard_client import Client

with Client.local(".") as client:
    app = client.repo("app")
    op = app.task("Make the flaky parser test deterministic", name="flaky-parser", budget_usd=2,
                  policy={"preset": "edit-worktree"})        # the harness may read and edit files, nothing else
    op = client.wait(op, check=True)                         # raises if the harness's turn failed, not only the request
    branch = op.result_branches[0]
    print(branch.name, branch.state)                         # flaky-parser ready
    print(branch.candidate)                                  # its commit and how much it changed, None if nothing
```

The branch is the repository's: `by diff flaky-parser` and `by merge flaky-parser` work on it afterwards. `name=` is
optional; without it the branch is named from the prompt (`make-the-flaky-parser-test-deterministic`), with `-2`, `-3`
added when the name is taken. The harness is the repository's default; `harness="codex"` names another.
`wait(check=True)` raises `BranchFailedError` (a subclass of `OperationFailedError`) when the branch ended `failed`,
`budget_exceeded` or `interrupted`, naming it, its state and the way to find out why.

Runnable versions, which need no model (`--command` runs the harness as a stand-in): `examples/python/first_run.py`,
`stream_events.py` and `delegate.py` in the checkout.

## Against a server

For a service that is not on the same machine as the work, or that outlives it:

```sh
by serve --repo app=.        # listening on http://127.0.0.1:8421; the token is in .branchyard/server/token
```

```python
from branchyard_client import Client

client = Client("http://127.0.0.1:8421", token_file=".branchyard/server/token")
app = client.repo("app")     # the NAME in --repo NAME=PATH; the default name is the directory's
```

Everything after `client.repo` is the same as above. `Client.repos()` lists what a server serves.

Read document previews and their images through the same authenticated client:

```python
preview = app.preview_artifact("draft", "artifact-id", view="pages", page=1)
if preview.get("image"):
    image_bytes = app.read_preview_asset("draft", "artifact-id", preview.image, view="pages", page=1)
```

Table previews accept `sheet`, `offset` and `limit`. Image requests must repeat the preview options because the
server binds each image to its original artifact and options. The preview record reports conversion readiness;
these methods use the server's renderer and preserve its permission and availability errors.

## Knowledge workspaces

An operator can enable managed knowledge projects on the server. These projects need no user repository or
client filesystem path. Workspace initialization and native sessions use the existing durable operation queue:

```python
project = client.create_project("Quarterly review", key="quarterly-project")
allocation = client.create_workspace(project.id, "Cost review", key="quarterly-workspace")
client.wait(allocation, check=True)
workspace_id = allocation.workspace_id

current = client.workspace(workspace_id)
source = client.upload_workspace_source(
    workspace_id, b"quarter,cost\nQ1,100\nQ2,120\n", "Inputs.csv",
    media_type="text/csv", revision=current.revision, key="quarterly-input",
)
run = client.start_workspace_session(
    workspace_id, "Analysis", "Analyze the supplied costs and return your conclusions as JSON.",
    harness="codex", sources=[source.id], policy={"preset": "edit-worktree"},
    result_contract={"schema": {"type": "object"}, "required": True}, key="quarterly-analysis",
)
client.wait(run, check=True)
print(client.workspace_sessions(workspace_id))
```

The returned operations retain `workspace_id` and, when applicable, `session_id`. Keep each idempotency key when
retrying its request. `workspace_messages(workspace_id, session_id, after=cursor)` returns native messages and the
next cursor. Sending, steering and explicitly resuming use `send_workspace_message`; the server retains control
of harness installation, provider placement, native authentication and permissions.

Source uploads send original bytes with `X-Source-Title`, `X-Workspace-Revision` and the declared media type.
`read_workspace_source` and `read_workspace_artifact` return authenticated bytes. Source and artifact preview
methods use the same bounded options as repository previews:

```python
table = client.preview_workspace_source(workspace_id, source.id, view="table", limit=25)
print(table)

artifacts = client.workspace_artifacts(workspace_id)
if artifacts:
    preview = client.preview_workspace_artifact(workspace_id, artifacts[0].id, view="pages", page=1)
    if preview.get("image"):
        image_bytes = client.read_workspace_artifact_preview_asset(
            workspace_id, artifacts[0].id, preview.image, view="pages", page=1,
        )
```

Accepting deliverables with `finalize_workspace` requires current-turn provenance and the current workspace
revision. Code integration remains independent. Membership changes, comments and workspace lifecycle intents
also carry a revision; read the workspace again after a stale-revision error. Presence is advisory and does not
hold a writer lease. Subjects and comment authors come from verified server identity.

`workspace_runtime` reports owner-host readiness and provider capabilities. An authorized editor can open a
workspace terminal when the branch is idle and the operator has enabled shell access. Terminal output exposes
exact decoded bytes as `TerminalOutput.data_bytes`. Input uses explicit sequences starting at one; retry only
the last identical frame. A lost handle or uncertain input raises the server's error. The SDK never opens a new
shell or replays input automatically after a restart. Closing the terminal releases its writer lease.

Repository projects can inspect, publish and refresh a session's pull request through the workspace PR methods.
Publication requires the server's merge grant and forge policy; forge credentials stay with the worker. Resource
lifecycle operations preserve source originals and accepted deliverables, and never submit a model turn.

## Where to go next

- [docs/sdk.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/sdk.md): the whole client, its errors,
  streaming, connector grants, one conversation per branch for a chat bot, and what it does not do.
- [docs/server.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/server.md): the HTTP API it speaks.
- [docs/delegation.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/delegation.md#python): the
  `branchyard` module inside a harness turn.
