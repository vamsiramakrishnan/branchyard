# Branchyard for Python

Two Python modules for [Branchyard](https://github.com/vamsiramakrishnan/branchyard), which runs coding-agent
harnesses (Claude Code, Codex, Gemini CLI and others) on git branches. Standard library only, Python 3.9 or newer.

| Module | For code that runs | How it reaches Branchyard |
|---|---|---|
| `branchyard_client` | anywhere: a script, a bot, a service | `by serve`'s HTTP API, or a `by serve` it starts itself (`Client.local`) |
| `branchyard` | inside a harness turn that Branchyard runs, delegating work to child branches | `by ... --json`, acting as that turn's branch |

Not on PyPI yet, so there is no `pip install branchyard` to run today. Both modules need a `by` to talk to.

## Install

From a checkout of the repository:

```sh
cargo install --locked --path crates/branchyard-cli     # puts `by` on PATH; no release is published yet
pip install ./sdk/python                                # branchyard and branchyard_client
by --version
```

`by` is found from `BRANCHYARD_BY`, else `PATH`. The release archive `branchyard-sdk-<version>.zip` installs the same
way (`unzip` it, then `pip install ./branchyard-sdk`).

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

## Where to go next

- [docs/sdk.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/sdk.md): the whole client, its errors,
  streaming, connector grants, one conversation per branch for a chat bot, and what it does not do.
- [docs/server.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/server.md): the HTTP API it speaks.
- [docs/delegation.md](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/delegation.md#python): the
  `branchyard` module inside a harness turn.
