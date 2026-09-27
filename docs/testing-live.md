# Live testing

Everything below is untested against real harnesses, real credentials or real KVM until someone runs it. The hermetic suite (`cargo test --workspace`) runs every path against a fake ACP agent. This is the checklist for a machine where you can drive the real ones. Work top to bottom: each section assumes the ones before it passed.

Record what you ran in the place each section names. A result that is not recorded does not count as qualification.

## 0. Machine and budget

- Linux or macOS with git, Python 3 and a C compiler; Linux x86_64 or aarch64 with `/dev/kvm` for section 5; Docker, `kind` and `kubectl` for section 6; an OpenTelemetry collector for the telemetry row of section 7.
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
| One server | `by serve --data-dir D` in two terminals | The second fails at once, naming the first's pid |
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
- Delegation: restart with `by serve --allow-delegation`, then `by run "…delegate with by spawn…" --delegate --allow-delegation --yes --budget-usd 1` remotely. The harness's `by spawn` works in its shell on the server, `by children <root> --json` and `by inspect <child> --json` print what local mode prints, and `by run` shows the `delegated` table. Without the flag, the same run is refused with `delegation_not_allowed`.
- Providers: after section 6, restart with `--allow-provider substrate` and repeat its step 7 with `by --remote`, giving the key's absolute path on the server and exporting the `--pass-env` variables in the server's environment, not yours.
- PostgreSQL: build with `--features postgres`, run `by serve --database postgres://…` against a real server, run a task, restart `by serve`, and check that `by ls` and the operation survive. Record the PostgreSQL version.

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
- Build an image with a harness installed, then run `by run "…" --provider microsandbox --image <it> --pass-env ANTHROPIC_API_KEY --yes`. The harness sees only `HOME` and the passed variables, and the microVM is gone after the turn. Run another, `kill -9` the `by` process mid-turn, and check that `by ls` recovers the branch, its `recovered` event says the sandbox was destroyed, and `msb` lists no sandbox for it.

**Record:** in [sandbox providers](providers.md); mark the provider qualified only when all of this passed.

## 6. Agent Substrate (cluster; model calls only in the last step)

The Substrate provider has run only against the in-process fake. On a machine with Docker, `kind`, `kubectl`, `openssl` and Envoy:

1. **Cluster.** Check out Substrate at the pinned revision (`1d7ca8ced056192a1801d6565251adcaab3eb0c9`) and run `hack/create-kind-cluster.sh` and `hack/install-ate-kind.sh`. Port-forward the `Control` API and the router to this host and note both URLs; plain HTTP is accepted to loopback, so the first runs need no TLS. Find how the router addresses an actor and write it as a URL template with `{atespace}` and `{actor}`; the provider cannot know it from the vendored API. Note whether the installation serves TLS on either endpoint itself.
2. **Key.** `cargo build --release -p branchyard-bridge`, then `target/release/branchyard-bridge keygen --out bridge.key`, which prints the public key.
3. **Image.** Build a static bridge (`rustup target add x86_64-unknown-linux-musl` and `musl-tools` for `ring`, then `cargo build --release -p branchyard-bridge --target x86_64-unknown-linux-musl`) and an image holding it at `/usr/local/bin/branchyard-bridge`, with `git`, `sh`, `sleep`, `cat`, `printf`, a user for the harness (say `by`, UID and GID 1000, with no `sudo` and no setuid helper) that owns a writable `/workspace`, and later the harness. The bridge is the entry point and runs as root; it needs no separate init. Push it where the cluster can pull it and note its digest.
4. **Template.** Create an atespace, then the template from the key:

   ```sh
   export BY_SUBSTRATE_ENDPOINT=http://127.0.0.1:8080 \
     BY_SUBSTRATE_ROUTER='http://127.0.0.1:8081/{atespace}/{actor}/' \
     BY_SUBSTRATE_ATESPACE=branchyard BY_SUBSTRATE_TEMPLATE=by-bridge \
     BY_SUBSTRATE_KEY=$PWD/bridge.key BY_SUBSTRATE_WORKDIR=/workspace \
     BY_SUBSTRATE_IMAGE=registry.example/by-bridge@sha256:... \
     BY_SUBSTRATE_BRIDGE=/usr/local/bin/branchyard-bridge BY_SUBSTRATE_RUN_AS=1000:1000 \
     BY_SUBSTRATE_SANDBOX_CONFIG=gvisor BY_SUBSTRATE_STORAGE=gs://bucket/branchyard
   cargo test -p branchyard-substrate --test cluster -- --ignored create_bridge_template
   ```

   Use the router template you found in step 1, and your cluster's `SandboxConfig` name and storage location.
5. **Cluster tests.** `cargo test -p branchyard-substrate --test cluster -- --ignored --test-threads 1 cluster_` runs the conformance checks without mounts, attempt rotation, and a worktree round trip. All must pass.
6. **TLS.** Make a throwaway authority, a server certificate for the names the client uses (here the loopback address of the port-forwards) and a client certificate:

   ```sh
   ec='-newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes'
   openssl req -x509 $ec -days 7 -subj /CN=by-qual-ca -keyout ca.key -out ca.pem
   for who in server client; do
     openssl req $ec -subj /CN=by-qual-$who -keyout $who.key -out $who.csr
   done
   openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 7 -out server.crt \
     -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth')
   openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 7 -out client.crt \
     -extfile <(printf 'extendedKeyUsage=clientAuth')
   openssl req -x509 $ec -days 7 -subj /CN=by-qual-other -keyout other.key -out other-ca.pem
   ```

   If the installation serves TLS itself, give it `server.crt` and `server.key` (or use its authority as `ca.pem`). Otherwise terminate TLS in front of the port-forwards with Envoy: one listener on 8443 that requires a client certificate from `ca.pem`, offers ALPN `h2` and passes the decrypted stream to 127.0.0.1:8080 with `envoy.filters.network.tcp_proxy`; one on 8444 without client certificates, offering `http/1.1`, to 127.0.0.1:8081. Both use `DownstreamTlsContext` with `server.crt` and `server.key`. Record which you used. Then:

   ```sh
   export BY_SUBSTRATE_ENDPOINT=https://127.0.0.1:8443 \
     BY_SUBSTRATE_ROUTER='https://127.0.0.1:8444/{atespace}/{actor}/' \
     BY_SUBSTRATE_CA=$PWD/ca.pem BY_SUBSTRATE_CLIENT_CERT=$PWD/client.crt \
     BY_SUBSTRATE_CLIENT_KEY=$PWD/client.key
   cargo test -p branchyard-substrate --test cluster -- --ignored --test-threads 1 cluster_
   ```

   All must pass. With `BY_SUBSTRATE_CA=$PWD/other-ca.pem`, or without the client certificate, the first call must fail; with `http://` URLs to a host other than loopback, the provider must refuse before connecting. If the router can route by SNI and pass TLS through to the actor, put `server.crt` and `server.key` in the image (key readable by root only), set `BY_SUBSTRATE_BRIDGE_TLS_CERT` and `BY_SUBSTRATE_BRIDGE_TLS_KEY` to their paths there, recreate the template, and run the tests again against the router's TLS address; the wakeup probe must still succeed.
7. **Observe what the fake assumes.** The identity files under `/run/branchyard/identity` after a resume and in a branched actor (new UID); that a superseded or ended credential is refused through the router; that the router forwards WebSocket upgrades and keeps a connection open for a long turn; whether it activates a suspended actor; `stop` ending processes before the suspend; that `ResumeActor`, `SuspendActor` and `RevertActor` return the actor they acted on. In an actor, through an exec: `id -u` prints 1000; the state file under `/var/lib/branchyard-bridge` cannot be read; `sh -c 'sleep 1 & exit'` leaves no zombie once `sleep` ends (`ps -o stat` has no `Z`); `kill -TERM 1` changes nothing; deleting or suspending the actor ends the bridge within ten seconds of the runtime's `SIGTERM`. Time create, resume, suspend, tag and branch, cold and warm.
8. **A harness.** Rebuild the image with a harness installed, recreate the template, then in a throwaway repository: `by run "…" --provider substrate --substrate-endpoint $BY_SUBSTRATE_ENDPOINT --substrate-router "$BY_SUBSTRATE_ROUTER" --substrate-ca ca.pem --substrate-client-cert client.crt --substrate-client-key client.key --substrate-atespace $BY_SUBSTRATE_ATESPACE --substrate-template <it> --substrate-key bridge.key --pass-env ANTHROPIC_API_KEY --yes --budget-usd 1`. The candidate holds the harness's changes, `by merge` works, the actor is gone afterwards (`kubectl ate` or `ListActors`), and a `by send` resumes the session from the carried home. Ask the harness to commit its work in two commits and leave one more change uncommitted: `by log` shows both commits with their messages under the turn's snapshot. Kill `by` mid-turn after the harness has written a file, and check that `by ls` recovers the branch, says in its `recovered` event that the work was brought back, shows the file in the candidate, and deletes the actor.

**Record:** in [Agent Substrate](substrate.md) and [validation](validation.md), with the Substrate revision, the router template, how TLS was served on each hop, the image digest and which tests passed; mark the provider qualified only when all of this passed.

## 7. Provisioning (model calls)

[Provisioning](provisioning.md) writes each harness's native files from Scion's knowledge; only Claude Code's key, MCP file and MCP headers have been checked against a real binary (offline, below). In the throwaway repository, with credentials in your environment and not logged in to the harnesses (`--isolated` gives each branch a fresh home):

| Check | Command | Expect |
|---|---|---|
| Claude Code, API key | `by run "Run env in Bash and say whether ANTHROPIC_API_KEY is set" --harness claude-code --isolated --secret ANTHROPIC_API_KEY --model large --yes --budget-usd 0.2` | The turn runs without a login; `by log` shows `provisioned: auth api-key; wrote .branchyard/credentials/anthropic-api-key, .claude/settings.json, …` and no `… in the environment of its tool commands`; the key file is 0600, `settings.json` has `apiKeyHelper`; the model says the variable is not set; the model reported is the one `large` maps to |
| Claude Code over ACP | the same with `--harness claude-code-acp` | The same; `ANTHROPIC_MODEL` applies to claude-agent-acp, and so does the helper (it loads user settings) |
| Claude Code, OAuth token | `--secret CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`), same prompt | `by log` says `CLAUDE_CODE_OAUTH_TOKEN in the environment of its tool commands`, and the model finds it set: record it if not |
| Claude Code, no secret on a command line | `--delegate --mcp docs=/abs/path/to/an/mcp-server`, and while the turn runs, `ps -o args= -C claude` | `--mcp-config <home>/.branchyard/claude-mcp.json` (0600) and no JSON; without `--isolated`, a path under `.branchyard/turns/` that is gone after the turn |
| Remote MCP server | the SDK or API with `remote_mcp_servers` (an HTTP server with an `Authorization` header from a secret), `--harness claude-code` and `claude-code-acp` | The server receives the header; the header is in the 0600 MCP file (stream-json) and not in `ps` output, the event log or `state.db` |
| Credentials on removal | a Claude Code branch with a key, `by fork <b> "x"` (the fork shares the home), then `by rm <b>` | `.branchyard/homes/<b>/.branchyard/credentials/` is empty and `settings.json` has no `apiKeyHelper`; the fork's next send provisions them again; with `by rm --keep-credentials` they stay |
| Tools' environment, other harnesses | for each harness whose key is a variable (Gemini CLI, OpenCode, Copilot, Hermes, Antigravity), ask it to run `env` and say whether its key's variable is set | Record the answer in the [table](provisioning.md#secrets-and-the-harnesss-tools), which assumes yes |
| Codex, API key and effort | `by run "Say hi" --harness codex --isolated --secret OPENAI_API_KEY --effort high --yes --budget-usd 0.2` | `~/.codex/auth.json` (0600) is accepted by codex-cli 0.157.1; `config.toml` has `model_reasoning_effort = "high"` at the top level and Codex honors it |
| Codex auth file | `--secret CODEX_AUTH=@$HOME/.codex/auth.json` after `codex login` | Codex runs with the copied login |
| Codex over ACP, model | `--harness codex-acp --model <name>` | codex-acp uses the `model` from `config.toml` |
| Gemini CLI | `--harness gemini-cli --isolated --secret GEMINI_API_KEY --model <name>` | `security.auth.selectedType` and `model.name` in `settings.json` are honored over ACP |
| Copilot, Hermes, OpenCode | each with its secret (see the table in [provisioning](provisioning.md#harnesses)) | Each authenticates from what was written; record any file the harness does not read |
| Antigravity MCP | `--harness antigravity --isolated --secret GEMINI_API_KEY --mcp docs=/abs/path/to/an/mcp-server --allow-unapproved-tools` | `agy` loads the server from `~/.gemini/config/mcp_config.json` and the instructions from `~/.gemini/GEMINI.md` |
| Telemetry | any of the above with `--telemetry http://127.0.0.1:4317` and an OpenTelemetry collector listening there | Metrics or logs arrive; no prompt text in them; `--telemetry off` sends nothing |
| In a sandbox | the Codex row with `--provider microsandbox` (section 5) and `--provider substrate` (section 6) | The same files, with the same modes, inside the sandbox; after the turn the private home holds them |
| Secrets stay out | after the rows above, `grep -rF <each key> .branchyard/state.db .branchyard/server by/ 2>/dev/null` and in the worktrees | Found only under `.branchyard/homes/` |
| Server | `by serve --secret OPENAI_API_KEY --allow-client-commands` with the key in the server's environment, then `by --remote … run "Say hi" --harness codex --isolated --secret OPENAI_API_KEY --yes` | The server's key is used; `--secret OPENAI_API_KEY=X` is refused by `by`; an undefined secret is refused with `secret_not_allowed` |

**Record:** in [validation](validation.md), with the harness versions; correct [provisioning](provisioning.md) for every path or variable a harness does not read, and mark the harness's provisioning unverified until it does.

**Without model calls.** Claude Code can be checked against a local stand-in for the Messages API: a small HTTP server on `127.0.0.1` that answers `POST /v1/messages` with a streamed `tool_use` of `Bash` (a command that writes `env` to a file), then with text. Put `{"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:PORT"}}` in `.branchyard/homes/<name>/.claude/settings.json` before `by run --name <name> --isolated …` (provisioning merges into it, and isolation strips a base URL from the environment). The stand-in's log shows the key each request carried; the file shows the Bash tool's environment. This is how the Claude Code rows of the [table](provisioning.md#secrets-and-the-harnesss-tools) were checked against 2.1.283. Log only the keys you gave: on a host with its own Claude Code login, a harness with no credential of its own may send that one.

## Cleaning up

`by rm <branch>` for each branch, `by ls` empty, no `by/…` worktrees left (`git worktree list`), no stray harness processes, and the spending recorded.
