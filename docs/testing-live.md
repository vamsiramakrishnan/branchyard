# Live testing

Everything below is untested against real harnesses, real credentials or real KVM until someone runs it. The hermetic suite (`cargo test --workspace`) runs every path against a fake ACP agent. This is the checklist for a machine where you can drive the real ones. Work top to bottom: each section assumes the ones before it passed.

Record what you ran in the place each section names. A result that is not recorded does not count as qualification.

## 0. Machine and budget

- Linux or macOS with git, Python 3 and a C compiler; Linux x86_64 or aarch64 with `/dev/kvm` for section 5.
- Rust 1.90 (`rust-toolchain.toml` pins it), and Rust 1.94 only for section 5.
- The harnesses you want to test, at the versions [compatibility](compatibility.md) lists as checked against (Claude Code 2.1.283 and codex-cli 0.157.1 for the native drivers). Note any other version you use.
- A throwaway repository with a fast test command, for example a small crate whose `cargo test` takes seconds. Never your real work: harnesses in local mode run as you.
- A spending cap. Every section that makes model calls says so; set `--max-cost-usd` or `--budget-usd` on each run. The full list below should stay under about $10 on the Claude and Codex profiles.

```sh
cargo install --locked --path crates/branchyard-cli
by --version
by harnesses          # which profiles find their executable here
```

## 1. Driver qualification (model calls)

For each profile you can run, qualify its driver against the real binary. This checks protocol behavior only, not isolation.

```sh
cargo run -p branchyard-qualify -- --profile claude-code-stream-json \
    --workdir /tmp/qual --max-cost-usd 3 --report /tmp/claude-code-stream-json.json
cargo run -p branchyard-qualify -- --profile codex-app-server \
    --keep-env OPENAI_API_KEY --workdir /tmp/qual --max-cost-usd 3 \
    --report /tmp/codex-app-server.json     # or rely on `codex login`
```

The ACP profiles (`gemini-cli-acp`, `opencode-acp`, `goose-acp` and others) take the same command. The Antigravity, Pi and Amp drivers are unqualified and need the most attention: Amp's driver was written from documentation only.

**Pass:** every scenario passes, or each failure is understood and filed.

**Record:** copy each report into `docs/qualification/<profile>.json`, add it to `REPORTS` in `crates/branchyard/src/harness.rs`, summarize it in `docs/qualification/README.md`, then regenerate [compatibility](compatibility.md):

```sh
cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md
```

## 2. `by` end to end (model calls)

In the throwaway repository:

| Check | Command | Expect |
|---|---|---|
| One branch | `by run "Add a test for X" --check "cargo test" --ask --budget-usd 1` | A `by/…` worktree; each permission request prompts; `by diff` and `by log` show the work and every decision |
| Refusals by default | the same with neither `--ask` nor `--yes`, piped (`\| cat`) | Requests denied and logged; the turn ends cleanly |
| Fan out | `by fan "…" --harness claude-code,codex --yes --budget-usd 1` | Two branches side by side and a comparison table |
| Send and fork | `by send <b> "now also …" --yes`, then `by fork <b> "try another way" --yes` | The session resumes; the fork starts from the candidate (Claude Code may need `--fresh-session`; see the README) |
| Merge | `by merge <b>` | The check runs in a temporary worktree; the target moves only if it passes |
| Budget | `by run "…" --max-turns 1` then `by send`; and `--max-minutes 1` on a long task | `budget_exceeded` with the limit named |
| Interrupt | Ctrl-C during a long turn | The harness and its process group are gone (`ps -eo pgid,comm`) |
| Cancel | `by cancel <b>` from a second terminal during a long turn | The turn stops; `by log <b>` names who cancelled |
| Crash recovery | `kill -9` the `by` process mid-turn (after the prompt is submitted), then `by ls` | The harness's process group is gone; the branch is `interrupted` with a `recovered` event saying the outcome is unknown; the prompt was not sent again; `by send <b> …` resumes the session |
| Two terminals | `by send <b> …` while another `by` runs a turn on `<b>` | Refused as running; no race |
| Unapproved tools | `by run "…" --harness pi` | Refused; with `--allow-unapproved-tools` it runs |
| Watch | `by watch` in a second terminal during the above | The tree updates live; `q` exits cleanly |

**Record:** anything that differs from the expectation, in an issue or in [validation](validation.md). Recovery is described in [durability](durability.md); on macOS it relies on `ps -o lstart=` for process start times, which is untested.

## 3. Delegation (model calls)

```sh
by run "Split this into two subtasks and delegate each with by spawn; integrate what passes" \
  --delegate --allow-delegation --check "cargo test" --yes --budget-usd 2
by ls                 # children indented under the root
by children <root>
```

Check each of these against [delegation](delegation.md):

- Claude Code: the harness uses `by spawn` from its shell, and also sees the `branchyard` MCP server and the `branchyard:delegate` skill.
- Codex: its sandboxed shell can reach the delegation socket, and `BRANCHYARD_DELEGATION` survives into it. This is the riskiest unverified assumption.
- Codex `send` and `fork` of a delegating branch keep the MCP config and instructions; they were checked only on `thread/start`.
- An ACP agent (`claude-code-acp` or `gemini-cli-acp`) honours `mcpServers`.
- A long integration check does not hit the harness's MCP or shell tool timeout.
- `by cancel <child>` from outside stops the child's subtree.

**Record:** results in [delegation](delegation.md), under "Not guaranteed" and in the projection table's evidence column.

## 4. Server and remote mode (model calls)

```sh
by serve                                              # in the repository; creates .branchyard/server/token
export BRANCHYARD_REMOTE=http://127.0.0.1:8421
export BRANCHYARD_TOKEN_FILE=$PWD/.branchyard/server/token
by run "…" --check "cargo test" --yes --budget-usd 1  # from another directory
by watch
```

- Kill `by` mid-turn: the turn keeps running on the server, and re-running `by watch` picks it up.
- Stop the server mid-turn (Ctrl-C), restart it: the operation reads as interrupted, as [the server reference](server.md) says.
- The same idempotency key submitted twice runs once.
- A wrong token gets 401; binding to a non-loopback address without TLS or `--insecure-bind` is refused.

**Record:** in [the server reference](server.md) if behavior differs.

## 5. Microsandbox (KVM; model calls only in the last step)

Follow [sandbox providers](providers.md) to install `msb` 0.7.3 and build with the `microsandbox` feature on Rust 1.94, then:

```sh
BY_MSB_IMAGE=alpine:3.20 cargo +1.94 test -p branchyard-microsandbox --features microsandbox \
  -- --ignored --test-threads 1
```

- All 14 ignored tests pass.
- Record who owns files the guest writes into the mounted worktree (uid and gid on the host).
- Confirm the runtime archive's name and layout match what `providers.md` assumes.
- Build an image with a harness installed, then run `by run "…" --provider microsandbox --image <it> --pass-env ANTHROPIC_API_KEY --yes`. The harness sees only `HOME` and the passed variables, and the microVM is gone after the turn.

**Record:** in [sandbox providers](providers.md); mark the provider qualified only when all of this passed.

## 6. Agent Substrate (cluster)

The Substrate adapter is tested only against an in-process fake. With a cluster (kind works), follow the qualification steps in [Agent Substrate](substrate.md): install Substrate at the pinned revision, run the adapter against its `ateapi` endpoint, time each lifecycle operation cold and warm, and check that tag readiness, UID preconditions and egress policy match the fake. Running harnesses there needs the in-sandbox bridge, which is not built yet.

## Cleaning up

`by rm <branch>` for each branch, `by ls` empty, no `by/…` worktrees left (`git worktree list`), no stray harness processes, and the spending recorded.
