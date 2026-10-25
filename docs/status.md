# Status

Written 25 October 2026. This is the one page that says what is built, what has run and what has not. Other pages link here instead of restating it. The harness tables below are generated (`python3 tools/docs_facts.py --write`); the rest is written by hand and dated, so a line older than the code it describes is a bug to fix here.

## The words

| Word | Means |
|---|---|
| built | The code exists and compiles in the build named (the default build, or behind the cargo feature named). It says nothing about whether the code has run. |
| hermetic | Tested against fakes only: a fake ACP agent, a fake `gh`, a fake cluster, a mock gateway. No real harness, model, provider or upstream took part. |
| live-qualified | Every scenario of the driver's qualification suite passed against the real harness binary, started by `branchyard-qualify` as a local process ([qualification](qualification/README.md)). |
| partial | A live qualification run with at least one failing scenario. |
| sandbox-qualified | The same, or a real task, inside a Branchyard sandbox provider. Nothing is sandbox-qualified yet. |
| unqualified | Built and hermetically tested; never run against the real thing. |
| design | Specified in a document; no code. |

## How it runs today

- **Local.** `by` runs the engine in its own process. Each harness is a local process in its own git worktree under `.branchyard/`, as your operating-system user. State is SQLite in `.branchyard/state.db`.
- **Server.** `by serve` and `by worker` run the same engine behind an authenticated HTTP API, with SQLite per data directory or, in a build with the `postgres` feature, PostgreSQL shared by several servers and workers. By default the server uses the local process provider, so harnesses run as the server's user with no isolation.
- **Sandbox.** The Microsandbox and Agent Substrate providers are built and unqualified. A server runs them only when its operator allows them.
- **Not built.** The execution-node service of [the architecture](design.md), where all managed execution happens on separate nodes, is design only.

See [modes](modes.md) for how to choose between these.

## Subsystems

| Subsystem | State | Last evidence | Read |
|---|---|---|---|
| Local engine and `by` (run, fan, send, fork, merge, checkpoints, delegation, graphs, rigs) | built; hermetic; a few real runs | 27 September 2026: `by run`, `by diff`, `by merge` with Claude Code 2.1.283. 8 and 9 October 2026: a twelve-scenario delegation battery with real Claude Code children passed 12 of 12. 25 October 2026: `by setup`, `by run`, `show`, `diff`, `log`, `merge`, `rm` and `send` in ten harnesses on GLM (Z.ai) and Gemini models, and the `testing-live.md` checks on Gemini CLI ([qualification](qualification/README.md#results-25-october-2026), [roadmap](roadmap.md)) | [validation](validation.md), [dogfood](dogfood/round5/README.md), [delegation](delegation.md) |
| `by serve` and `by worker` | built; hermetic over HTTP on loopback | None recorded through a real harness; not load-tested; not deployed (the status note in [server](server.md)) | [server](server.md), [surfaces](surfaces.md) |
| PostgreSQL store | built behind the `postgres` feature (`cargo install --features postgres`) | 29 September 2026: the suites against a throwaway PostgreSQL 16. 11 October 2026: a real PostgreSQL 16.13 run for the feed-lock fix. Not run: servers on more than one host | [durability](durability.md), [deploy](deploy.md) |
| Sandbox providers: Microsandbox (`microsandbox` feature), Agent Substrate | built; unqualified | None against a real runtime: Microsandbox's KVM tests and a Substrate cluster have not been run. Pause, snapshot and live branching are tested against fakes | [providers](providers.md), [substrate](substrate.md), [live testing](testing-live.md) |
| Model gateway | built; hermetic; used live | 16 October 2026: DeepSeek Harness (Gemini and Z.ai backends), Claude Code 2.1.301 and codex-cli 0.157.1 (Z.ai) ran turns through it | [model gateway](model-gateway.md), [qualification](qualification/README.md) |
| Fleet routing and judge | built; hermetic; run on real harnesses | 16 October 2026: routed fans on Codex, Oh My Pi and DeepSeek Harness, and a model's verdict (Oh My Pi on GLM-5.3) was used. A failover has not fired in a live run | [fleet](fleet.md) |
| Connectors, triggers, effects and the web companion | built; hermetic | Connectors and effects: tested against a mock gateway and Anvil's own test gateway, not yet used by a real harness (3 October 2026). Triggers: no delivery from a real GitHub, Slack, Linear or mail provider. Companion: no real push service | [connectors](connectors.md), [triggers](triggers.md), [effects](effects.md), [companion](companion.md) |
| Sync and task repositories | built; hermetic | 3 October 2026: directory and git backends, in-process stand-ins for S3, Cloud Storage and Azure | [sync](sync.md), [task repositories](task-repos.md) |
| Hooks and status lines | built for every harness with an install manifest | 24 and 25 October 2026: see the table below; on 25 October the reworded lines were repeated by real models in ten harnesses ([harness hooks](harness-hooks.md)) | [harness hooks](harness-hooks.md) |
| SDKs | Python `branchyard.py` and `branchyard_client.py`, Rust `branchyard` and `branchyard-client`: built; hermetic. TypeScript client: not built | 24 October 2026: one script run both inside and outside a harness turn. 25 October 2026: the two Python examples ran as written against Gemini CLI. Installed from a checkout or the SDK archive; the Rust crates are not published | [SDKs](sdk.md) |
| Distribution | `cargo install` from a clone works. Prebuilt binaries, `install.sh` and a Homebrew formula: written; the release workflow has never run | No GitHub release or tag exists | [distribution](distribution.md) |

## Harness drivers

<!-- fact:profiles.summary -->17 driver profiles cover 15 of 16 harness targets: 9 live-qualified, 2 partial and 6 unqualified.<!-- /fact -->

A live run is protocol qualification against the real binary as a local process. It is not a sandbox run: isolation, credentials and recovery are unqualified for every profile. "Permissions answered" and "Steer" are the two gates Branchyard can intercept mid-turn; [compatibility](compatibility.md) defines their values and lists every capability and the reason for each `no`.

<!-- fact:profiles.table -->

| Harness | Profile | Status | Live run (local process) | Permissions answered | Steer |
|---|---|---|---|---|---|
| Claude Code | `claude-code-stream-json` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 2.1.310 (Claude Code) ([report](qualification/claude-code-stream-json.json)) | live-tested | live-tested |
| Claude Code | `claude-code-acp` (alternate) | live-qualified | 9 of 9 pass, 2026-09-26, 0.81.2 ([report](qualification/claude-code-acp.json)) | live-tested | live-tested |
| Codex | `codex-app-server` (default) | live-qualified | 9 of 9 pass, 2026-10-25, codex-cli 0.164.0 ([report](qualification/codex-app-server.json)) | live-tested | live-tested |
| Codex | `codex-acp` (alternate) | unqualified | not run | contract-only | not verified |
| Antigravity | `antigravity-stream-json` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 1.4.7 ([report](qualification/antigravity-stream-json.json)) | — | — |
| Oh My Pi | `oh-my-pi-acp` (default) | live-qualified | 9 of 9 pass, 2026-10-25, omp/18.12.6 ([report](qualification/oh-my-pi-acp.json)) | live-tested | not verified |
| DeepSeek Harness | `deepseek-harness-acp` (default) | partial | 6 of 9 pass, 2026-10-25, 0.2.1-rc.2 ([report](qualification/deepseek-harness-acp.json)) | live-tested | not verified |
| Gemini CLI | `gemini-cli-acp` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 0.65.0 ([report](qualification/gemini-cli-acp.json)) | live-tested | not verified |
| OpenCode | `opencode-acp` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 1.19.4 ([report](qualification/opencode-acp.json)) | live-tested | not verified |
| Pi | `pi-rpc` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 1.4.0 ([report](qualification/pi-rpc.json)) | — | live-tested |
| Goose | `goose-acp` (default) | unqualified | not run | contract-only | not verified |
| Aider | none | not built | — | — | — |
| Cursor CLI | `cursor-acp` (default) | unqualified | not run | contract-only | not verified |
| GitHub Copilot CLI | `github-copilot-acp` (default) | unqualified | not run | contract-only | not verified |
| Amp | `amp-stream-json` (default) | unqualified | not run | — | — |
| Qwen Code | `qwen-code-acp` (default) | live-qualified | 9 of 9 pass, 2026-10-25, 0.26.0 ([report](qualification/qwen-code-acp.json)) | live-tested | not verified |
| Kimi CLI | `kimi-cli-acp` (default) | unqualified | not run | contract-only | not verified |
| Hermes | `hermes-acp` (default) | partial | 6 of 9 pass, 2026-10-25, Hermes Agent v0.19.0 (2026.7.20) ([report](qualification/hermes-acp.json)) | live-tested | not verified |

<!-- /fact -->

## Hooks per harness

`by hook` and `by statusline` tell a session a person started in a harness what Branchyard's branches did ([harness hooks](harness-hooks.md)). On 24 October 2026 the hooks were run with a model, and the model repeated the note, in Claude Code 2.1.310, Codex 0.164.0, Gemini CLI 0.65.0, Qwen Code 0.26.0, DeepSeek Harness 0.2.1-rc.2, Pi 0.73.1, Oh My Pi 18.12.6, OpenCode 1.19.4, Hermes 0.19.0 and Antigravity 1.4.7; on 25 October 2026 the same ten (Pi 1.4.0 as well) repeated the reworded lines after `by setup --yes` alone had installed the hooks, each in a private home. For Copilot CLI, Droid, Cursor and Kiro the payloads are written from documentation and the CLIs have not been run; the Amp plugin was run against a fake host API. Goose documents no way to add context, so it has no dialect.

## Crates

<!-- fact:workspace.crate_count -->23<!-- /fact --> crates. Test counts are in [validation](validation.md#passed-locally), generated from the source.

| Crate | What it is | State |
|---|---|---|
| `branchyard` | The engine: tasks, branches as worktrees, forks, checkpoints, budgets, permission policies, validated merges, delegation and graphs, durable execution on SQLite or PostgreSQL | built; hermetic |
| `branchyard-cli` | The `by` command, its watch dashboard, shell completions and man page | built; hermetic |
| `branchyard-harness` | Sans-IO protocol drivers: Claude Code stream-json, Codex App Server, Antigravity stream-json, Pi RPC, Amp stream-json and ACP v1 | built; see the driver table |
| `branchyard-runtime` | Runs a driver against a harness process through any sandbox provider, and the local provider | built; hermetic |
| `branchyard-workspace` | Git worktrees, candidates and validated merges: compare-and-swap on the target, checks in a temporary worktree | built; hermetic |
| `branchyard-server`, `branchyard-client` | The HTTP and SSE server with tenants, quotas, a durable queue and metrics; the typed blocking client | built; hermetic |
| `branchyard-config`, `branchyard-setup` | `branchyard.toml` and the setup interview | built; hermetic |
| `branchyard-provision` | Harness home provisioning, translated from Scion's provisioners | built; Claude Code's key and MCP delivery checked against its binary offline, the rest unqualified |
| `branchyard-sandbox`, `branchyard-microsandbox`, `branchyard-substrate`, `branchyard-bridge`, `branchyard-recipe` | The provider contract and admission; the Microsandbox and Agent Substrate providers; the in-sandbox exec bridge; environment recipes over ssh | built; unqualified |
| `branchyard-sync` | Replication of task repositories to object storage | built; hermetic |
| `branchyard-mcp`, `branchyard-herdr` | Delegation tools over MCP; the Herdr plugin's binary | built; hermetic. Not run against a real Herdr |
| `branchyard-qualify` | Runs driver qualification against real harness binaries | built; used for the live runs above |
| `branchyard-controls` | One harness identity registry and the harness and connector catalogs, from Herdr, Scion, emdash and Orca | built; hermetic |
| `branchyard-support`, `branchyard-wire`, `branchyard-testkit` | Shared helpers, the one HTTP/1.1 codec, test infrastructure | built |

## Vendored sources

All **<!-- fact:vendor.files -->259<!-- /fact --> vendored files** are pinned to upstream revisions, licenses, Git blob IDs and SHA-256 hashes. `vendor/` is a pinned reference snapshot: a file may carry a local patch only when `vendor.patches.json` records it with its reason and upstream commit (none does today), and `tools/verify_vendor.py` checks every other file against its pin. Adaptations built into Branchyard are recorded outside `vendor/` (`patches/`). See [vendoring](vendoring.md) and [third-party notices](../THIRD_PARTY.md). Warp's AGPL sources are references only and are excluded from the Rust build.

## What has not been run

[Validation](validation.md#not-validated) lists it in full. In short: no profile has run inside a Branchyard sandbox; no Substrate cluster or KVM host has been used; no real GitHub, Slack, Linear or mail delivery has reached a trigger; no real upstream has sat behind the connector gateway; `by serve` has not been deployed or load-tested. [Live testing](testing-live.md) is the checklist for a machine that can run them.

## Keeping this page honest

A claim about status belongs here, once. A page that needs it links to this one. Change a line when a run changes it, with the date of the run; `python3 tools/docs_facts.py --check` fails when the generated tables no longer match [compatibility](compatibility.md), which is generated from the qualification reports.
