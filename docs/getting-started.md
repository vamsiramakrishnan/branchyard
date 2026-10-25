# Getting started

By the end of this page you have run a task on a branch, read what it did, and merged it. The first build is the slow part. The commands in steps 5 to 8 run against a fake agent, with no model and no cost, and are run again on every change (`tests/test_getting_started.py`), so the output shown there is what they print. The last line of the page says what was not run.

## What you need

- **git**, with `user.name` and `user.email` set, and a repository that has at least one commit.
- **A harness CLI, installed and logged in**: Claude Code, Codex or Gemini CLI are the usual first picks. `by harnesses` (step 2) says what it finds. With none, [step 5](#5-try-it-without-a-model) runs the whole loop against a fake agent.
- **Rust**, to build `by` on Linux or macOS: install [rustup](https://rustup.rs); the repository pins toolchain 1.94.0 and rustup fetches it on the first build. After that `cargo` matters only if your check command uses it. A check is any command that exits 0 when the work is good (`npm test`, `pytest`, `make test`).

## 1. Install

No release is published yet, so build from source:

```sh
git clone https://github.com/vamsiramakrishnan/branchyard
cd branchyard
cargo install --locked --path crates/branchyard-cli
by --version
```

`by --version` prints `by 0.0.1`. If the shell cannot find `by`, add `~/.cargo/bin` to your `PATH`.

## 2. See what you have

```sh
by harnesses
```

```text
HARNESS      VERSION  LOGIN                QUOTA  PATH
claude-code  2.1.310  logged out (likely)  -      /opt/node22/bin/claude
```

One row per installed harness. `logged out (likely)` means `by` found no credential file or key variable; it did not ask the harness (see [troubleshooting](troubleshooting.md#likely-logged-out)). `by harnesses login ID` signs in through the harness's own flow. When something else looks wrong, `by doctor` checks the machine and the repository and ends with what to fix.

## 3. Set up the repository

```sh
cd path/to/your/repo
by setup
```

`by setup` is the one command for a first setup. It detects the harnesses installed here, whether each is logged in, which API-key variables are set (names only, never values) and the check command the repository's files suggest. It writes `branchyard.toml` when there is none, with those defaults; on a terminal it asks only what it cannot detect. It installs hooks, commands and skills into each harness it found, so a session in that harness hears when your branches finish, and checks that the hook answers. It never starts a login: for each harness that is logged out, or whose login it cannot tell, it prints the command to run.

It ends with a short card: what was detected, written and installed, the `Only you can do:` lines (trusting the hooks in Codex with `/hooks`, then `t`, for example), and `Next:` with a command to try. A second run writes nothing and says so. Without a terminal and without `--yes` it only shows the plan, as `--dry-run` does on a terminal. It exits 0 when it is done, even with things left for you. Other flags: `--json`, `--harness ID,ID`, `--no-hooks`, `--no-config`. Afterwards `by` on its own prints where the repository stands and the next step.

The same work, one piece at a time: `by init project`, `by init plugin --all-detected`, `by harnesses login ID`. The settings are in [setup](setup.md).

## 4. Run a task

With your harness, in your repository:

```sh
by run "Make the flaky parser test deterministic" --check "cargo test" --yes
```

`by run` starts the harness on a new branch `by/NAME` in its own git worktree under `.branchyard/` (git ignores that directory) and streams what it does. `--check` is the command that must pass on the merged result before `by merge` moves anything. `--yes` allows every tool permission request the harness makes; `--ask` asks you at the terminal instead, and `--permissions read-only|edit-worktree|full` answers by a preset. `--harness ID` picks another harness, `--name` names the branch. In a second terminal, `by watch` shows every branch live.

Two things to know first. The harness runs as you, in local mode, with no isolation beyond your user (`by` prints that on its first line); `docs/modes.md` shows the steps up. And a real run spends your login's allowance or your API key: cap it with `--budget-usd`, `--max-turns` or `--max-minutes`.

Every run ends with a card: the branch, its status, the candidate (the files it changed), the cost, and `next`, the commands that fit. Step 5 shows one.

## 5. Try it without a model

Branchyard ships a fake agent for its own tests. It speaks the same protocol as Gemini CLI, does what the prompt says (`WRITE path=content` writes a file) and costs nothing. From the clone:

```sh
cargo build -p branchyard-runtime --bin fake-acp-agent
FAKE="$PWD/target/debug/fake-acp-agent"
```

Then a throwaway repository, and a run in it:

```sh test
cd "$(mktemp -d)" && git init -q -b main try-by && cd try-by
echo "# try-by" > README.md
git add README.md && git commit -q -m "first commit"
by run "WRITE hello.txt=hello" --name hello --harness gemini-cli --command "$FAKE" --check "test -f hello.txt" --yes
```

```text
by: local mode: harnesses run as your user, with no isolation beyond it
wrote hello.txt
turn 1 completed
candidate … (1 file +1 -0)

branch     hello (by/hello)
status     ready
candidate  1 file +1 -0 at …
cost       not reported, 1 turn
next       by diff hello
           by merge hello
```

`--command` starts the fake agent in place of the program the profile would launch, and `--harness gemini-cli` selects the profile that speaks its protocol. Nothing else differs from a real run.

## 6. Read it

Use your branch's name from the card (`hello` here).

```sh test
by ls
by show hello
by diff hello
```

```text
BRANCH  HARNESS     STATUS  CANDIDATE     TURNS  COST  AGE
hello   gemini-cli  ready   1 file +1 -0      1     -  …
branch      hello
git branch  by/hello
harness     gemini-cli (gemini-cli-acp)
status      ready
candidate   1 file +1 -0 at …
checkpoints
+++ b/hello.txt
+hello
```

`by ls` is the table of every branch. `by show` is one branch in full: prompt, base, candidate, cost, session, worktree and its checkpoints, one per turn. `by diff` is the candidate against the base. `by log hello` has the events.

## 7. Merge it

```sh test
by merge hello
by ls
cat hello.txt
```

```text
merged hello into main (…)
hello   gemini-cli  merged into main  1 file +1 -0      1     -  …
hello
```

`by merge` runs the check on the exact merge result, then moves your current branch. If the check fails it moves nothing and prints the check's output ([troubleshooting](troubleshooting.md#check-failed)). The branch and its worktree stay until you remove them:

```sh test
by log hello
by rm hello
by ls
```

```text
…Z  status: running
…Z  prompt: WRITE hello.txt=hello
…Z  turn 1 completed
…Z  status: ready
…Z  status: merged into main
removed hello
no branches; start one with: by run "<prompt>"
```

## 8. Try several harnesses

`by fan` runs one prompt on several harnesses, one branch each, and compares them. The fake agent plays both here:

```sh test
by fan "WRITE fan.txt=hi" --name pair --harness gemini-cli,qwen-code --command "$FAKE" --yes
by compare --fan pair
```

```text
BRANCH           STATUS  CANDIDATE     TURNS  COST
pair-gemini-cli  ready   1 file +1 -0      1     -
pair-qwen-code   ready   1 file +1 -0      1     -
BRANCH           STATUS  TURNS  COST  TOKENS   TIME  CHECK  FILES    +/-  UNIQUE FILES
pair-gemini-cli  ready       1     -       -  …  none       1  +1 -0  -
pair-qwen-code   ready       1     -       -  …  none       1  +1 -0  -
```

With real harnesses, `--harness claude-code,codex` runs each in its own worktree. `by compare --fan pair --pick pair-qwen-code` merges the one you choose. Delete the throwaway repository when you are done: `cd .. && rm -rf try-by`.

## What the `next` lines mean

After a run, send or fork, `next` lists the commands that fit the branch's state. The common ones:

| Status | `next` says | Meaning |
|---|---|---|
| `ready` | `by diff NAME`, `by merge NAME` | A candidate exists. Read it, then merge it. |
| `no changes` | `by send NAME "<prompt>"`, `by rm NAME` | The turn changed no file. Ask again, or throw the branch away. |
| `running` | `by log NAME` | A turn is in progress. `by watch` shows every branch. |
| `interrupted`, `over budget`, `failed` | `by log NAME`, `by diff NAME`, `by rm NAME` | Something stopped the turn. Look at the log, keep or drop the work. |
| `merged` | `by rm NAME` | Done. Remove the worktree. |

[Concepts](concepts.md) has the table for every state, and [troubleshooting](troubleshooting.md) what to run when a branch is not `ready`.

## Where to go next

- [Concepts](concepts.md): the words (task, branch, turn, candidate, run, fan, map, spawn) and the states.
- `docs/modes.md`: local, isolated, sandbox, server, PostgreSQL, each with its one setting and its `by doctor` row.
- [Setup](setup.md): `branchyard.toml`, `by config show`, the interview behind `by init`.
- [Harness hooks](harness-hooks.md): how a Claude Code or Codex session hears about your branches.
- [The Python SDK](sdk.md): the same operations from a script.
- `docs/adding-a-harness.md`: a driver for a CLI Branchyard does not know yet.
- [Troubleshooting](troubleshooting.md) and [upgrading](upgrade.md).

Not done: `by setup` and the status card of a bare `by` are described from the specification and the command's help text as they stood while the command was being built; they were not run, and step 3 shows no output for them. The real-harness command in step 4 and the `by harnesses` rows were not run against a model here. The install command was run in debug mode, offline, into a scratch directory. Everything was run on Linux; macOS and Windows were not.
