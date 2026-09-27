# Comparison with Scion, OpenRig and Herdr

Survey date: 27 September 2026. This page compares Branchyard with three open-source projects that also run coding harnesses, and says what Branchyard should take from each. It supersedes the positioning in [strategy](strategy.md), which was built from the small fragments in `vendor/`.

Every statement about an upstream project cites a path in its repository at the commit below. `path:N` is a line. "Not found" means the repository was searched for it and it is not there; it does not mean the project could not add it. Statements about Branchyard cite its documents or code at `807088e`.

| Project | Repository | Commit surveyed | Date |
|---|---|---|---|
| Scion | GoogleCloudPlatform/scion | `d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338` | 2026-09-27 |
| OpenRig | mvschwarz/openrig | `c9be421b9c8522075006f9cee3c0b2ba00d52a22` | 2026-09-26 |
| Herdr | herdrdev/herdr | `fff6c820aa45f4eabb9b2e0456326dc74cca5a25` | 2026-09-27 |
| Warp | warpdotdev/warp | `5af88f49f84e70025f9c19e13f6b9ae64b624627` | 2026-09-25 |

Warp's application code, where all its agent code lives (`app/`), is AGPL-3.0; only its UI framework crates are MIT. Warp is surveyed for ideas only: nothing from it is ported, and Branchyard files contain none of its code ([vendoring](vendoring.md#warp-preserve-the-license-boundary)).

## Summary

| | Runs | Drives harnesses by | Approvals | Topology | Merging | Licence |
|---|---|---|---|---|---|---|
| Scion | Agents in containers, locally or through a Hub on Kubernetes or Cloud Run | An interactive TUI in tmux, prompts as keystrokes, state from harness hooks | Bypassed at launch | Grown at runtime by agents through the CLI | Manual, or a pull request | Apache-2.0 |
| OpenRig | A local daemon with tmux seats | An interactive TUI in tmux, prompts pasted, state from activity hooks; Pi through RPC | A launch posture per seat | Declared in YAML, then grown or shrunk by command | By convention between agents | Apache-2.0 |
| Herdr | A terminal multiplexer with a background server | The terminal itself; state from screen manifests or lifecycle hooks | Not answered; `blocked` is shown | Panes and tabs created by people, scripts or agents | Not found | Apache-2.0 |
| Warp | A terminal app driving harnesses locally, or tasks on Warp's hosted service | A pseudo-terminal per harness, reading structured transcripts alongside it | Bypassed at launch for every harness it drives | A parent/child conversation tree for its own agent | Not found; a pull-request link is recorded | AGPL-3.0 (app), MIT (UI crates) |
| Branchyard | An SDK engine, locally or behind an HTTP server, with sandbox providers | Structured protocols: stream-json, App Server, RPC, ACP | Answered per invocation by policy | Grown at runtime inside a delegation envelope | Validated merge on the exact target revision | Apache-2.0 |

Branchyard is the only one of the five that drives harnesses through their machine protocols, answers each tool permission, holds a cost budget, and merges only a checked candidate. The other four are further along as products: people use them daily, and each has a richer interface, more harness coverage, and operational features Branchyard lacks.

## Scion

**What it is.** "An open-source orchestration platform for teams of AI agents and the people working with them" (`README.md:7`). An agent is one harness in its own container, with its own home, credentials and workspace (`GLOSSARY.md`, "Agent").

**How it is built.** Go, about 2,600 source files: CLI in `cmd/`, packages in `pkg/`. The web UI is Lit with xterm.js (`web/package.json:46`, `:50`). Harness support is data plus a Python provisioner per harness: `harnesses/<name>/config.yaml`, `provision.py` and a shared `harnesses/scion_harness.py`. Nine bundles ship: antigravity, claude, codex, copilot, gemini-cli, grok-build, hermes, muse-code, opencode (`harnesses/`).

**Deployment.** Four modes: local CLI; Workstation (a local Hub, Runtime Broker and web UI); single-node hosted on SQLite; HA hosted on PostgreSQL with shared storage (`README.md`, "Project Status"; `docs-site/src/content/docs/hosted/ha/overview.md:57`). Runtimes are Docker, Podman, Apple Container, Kubernetes and Cloud Run (`pkg/runtime/factory.go:49`). A Hub dispatches to Runtime Brokers over a control channel (`pkg/runtimebroker/controlchannel.go`). "Managed agents" skip the broker and use a provider API (`GLOSSARY.md:147`).

**Licence.** Apache-2.0 (`LICENSE`).

### Where Scion is ahead of Branchyard

- Remote execution in production use: container runtimes, Kubernetes and Cloud Run, a Hub with PostgreSQL HA, brokers with heartbeats. Branchyard's sandbox providers are unqualified.
- Harness provisioning: auth files, MCP translation, settings, native telemetry and skills written into the harness home per harness (`harnesses/claude/provision.py`), with declared capability levels and reasons (`harnesses/claude/config.yaml:64`).
- Identity and access: user tokens, agent JWTs with roles capped by the parent (`GLOSSARY.md:235`), access boundaries, scoped secrets, a quota system with reservations (`GLOSSARY.md:251`). **Narrowed**: the server now maps each bearer credential to a principal — a tenant, subject, scopes (`read`/`run`/`merge`/`admin`) and repository allowlist, never a caller-supplied tenant ID — with per-tenant quotas (`max_running`, `max_branches`, `max_cost_usd`, `max_artifact_bytes`) reserved atomically at admission ([server](server.md#identity-and-scopes), [server](server.md#quotas)). Still ahead: Scion's roles are capped transitively by the delegating parent's own role, not a flat scope set, and its JWTs are agent-scoped rather than server credentials the operator provisions.
- People in the loop: web chat, Slack, Discord, Telegram, Teams and Google Chat bridges (`extras/`), notifications on activity (`GLOSSARY.md:300`), schedules, and attach to a live tmux session. Branchyard's own notification surface is narrower: signed webhooks for status changes, stalls and permission requests ([server](server.md#webhooks)), no chat bridges.
- Lifecycle: suspend, resume, auto-suspend of stalled agents, reincarnation with a handoff (`docs-site/src/content/docs/local/agent-lifecycle.md`). **Stall detection and reincarnation are now built** ([lifecycle](lifecycle.md)); suspend/resume of a stalled agent to free its resource is not: Branchyard has no agent suspend, only `interrupt` (end the turn) as a stall action.

### Where Branchyard is ahead of Scion

- Permissions: Scion launches Claude Code with `--dangerously-skip-permissions` (`harnesses/claude/config.yaml:60`) and Codex with `--dangerously-bypass-approvals-and-sandbox` (`harnesses/codex/config.yaml:45`). Branchyard answers every request by policy and never bypasses ([harness integration](harness-integration.md)).
- Control channel: Scion sends prompts and interrupts as tmux keystrokes (`pkg/agent/manager.go:331`) and infers state from hooks. Branchyard reads typed turns, tool calls, usage and results from the protocol.
- Integration: Scion leaves merging to people or pull requests (`docs-site/src/content/docs/workstation/git-projects.md:18`). Branchyard checks the exact candidate against the exact target and moves the ref by compare-and-swap ([design §12](design.md#12-validated-integration)).
- Cost budgets: Scion limits turns, model calls and duration (`pkg/sciontool/hooks/handlers/limits.go:21`); no cost limit was found. Branchyard reserves and enforces USD budgets across a subtree ([delegation](delegation.md#the-envelope)).
- Session fork: not found in Scion. Branchyard forks Claude Code, Codex and Pi sessions ([compatibility](compatibility.md)).

## OpenRig

**What it is.** "A harness wraps a model. A rig wraps your harnesses. Define your agent team in YAML, boot it with one command" (`README.md:5`). A rig is pods of seats; a seat is a stable address such as `dev-owner@first-project` whose occupying conversation can change (`README.md`, "Key Concepts").

**How it is built.** TypeScript on Node.js 20–24, in `packages/`: `daemon` (Hono HTTP, better-sqlite3; `packages/daemon/package.json:82`, `:83`), `cli` (with an MCP server; `packages/cli/package.json:50`), `tui`, and a React `ui` in maintenance mode (`README.md`, "How It Works"). Every seat is a tmux session. Three runtime adapters implement a five-method contract: Claude Code, Codex and a terminal; Pi runs through an RPC runner inside a pane (`docs/as-built/architecture/adapters-and-runtimes.md:20`; `packages/daemon/src/adapters/pi-runner.ts`).

**Deployment.** One local daemon per user, state under `~/.openrig`, SQLite with 40 migrations (`docs/as-built/architecture/daemon-core.md:106`). A multi-host registry reaches other daemons over HTTP (`packages/daemon/src/domain/hosts/remote-daemon-http.ts`; `packages/cli/src/commands/host.ts:456`). Docker only for services a rig declares (`README.md`, "Requirements").

**Licence.** Apache-2.0 (`LICENSE`).

### Where OpenRig is ahead of Branchyard

- A declarative team: RigSpec with pods, members, edges, startup files and actions, restore policy, culture file and services (`docs/reference/rig-spec.md`; `packages/daemon/src/domain/rigspec-schema.ts`). Branchyard's [`by rig`](rigs.md) now declares seats, pods, delegation edges and startup files, and refuses the rest.
- Startup delivery: a ten-step sequence from projection to a proved-ready seat (`packages/daemon/src/domain/startup-orchestrator.ts:86`), with delivery hints `guidance_merge`, `skill_install` and `send_text` (`packages/daemon/src/domain/types.ts:849`).
- Honest restore: each resume is classified `resumed`, `failed`, `inconclusive` or `attention_required` from the pane (`packages/daemon/src/domain/native-resume-probe.ts:6`), and snapshot restore reports per-seat outcomes (`README.md`, "Key Concepts").
- Coordination state: an owned-work queue with closure reasons (`packages/cli/src/commands/queue.ts:391`), chat rooms, a workflow runtime with watchdogs (`docs/as-built/architecture/workflow-runtime.md`), and token-burn reporting (`packages/cli/src/commands/usage.ts:41`).
- Adoption: `rig discover` fingerprints existing tmux sessions and `rig adopt` manages them (`README.md`, "Key Concepts").

### Where Branchyard is ahead of OpenRig

- Per-invocation permissions. OpenRig's policies choose launch flags or native configuration; "This records a selection, not a live permission change" (`docs/reference/rig-spec.md`, "Attaching a permission policy"). Its floor is Claude `--permission-mode acceptEdits` and Codex `-s workspace-write` (`packages/daemon/src/adapters/yolo-mode.ts:42`, `:59`).
- Protocol control for Claude Code and Codex: OpenRig pastes text through tmux buffers (`packages/daemon/src/adapters/tmux.ts:366`) and reads state from hooks and pane content.
- Budgets: no cost or turn limit was found in the daemon; usage is reported, not enforced.
- Delegation authority: "Specialist delegation is conventional, not automatic — addressed by session name or normal communication surfaces" (`docs/as-built/architecture/architecture-rules-and-event-system.md:196`). Branchyard issues a per-turn token that acts only on descendants, inside an envelope.
- Isolation and remote sandboxes: OpenRig seats run as the user in tmux. Branchyard has (unqualified) Microsandbox and Substrate providers.
- Merging: OpenRig's first-use flow asks one agent to review "the exact candidate" (`README.md:46`); the engine does not check or promote it.

## Herdr

**What it is.** "The runtime your coding agents live on": a terminal multiplexer whose background server keeps panes running, marks each agent working, blocked or idle, and lets agents drive it (`README.md`).

**How it is built.** One Rust binary (`Cargo.toml`, version 0.9.1), about 300,000 lines, with a vendored libghostty-vt terminal and portable-pty (`vendor/`). A local socket API with a published JSON Schema (`docs/next/api/herdr-api.schema.json`) serves the CLI, plugins and agents (`docs/next/website/src/content/docs/socket-api.mdx:93`).

**Deployment.** A per-user server and one or more attached clients; saved SSH machines appear in the same window (`docs/next/website/src/content/docs/connecting-machines.mdx:6`). No containers or sandboxes: "herdr doesn't wrap or replace them; it owns their terminals" (`README.md`).

**Licence.** Apache-2.0 (`LICENSE`).

### Where Herdr is ahead of Branchyard

- Harness breadth for observation: 22 detection manifests (`src/detect/manifests/`) and 18 hook integrations (`docs/next/website/src/content/docs/integrations.mdx`). Any interactive harness works, with or without a protocol.
- A human interface: panes, tabs, workspaces, attach and detach, remote machines, plugins, notifications.
- Resume after restart through integration-reported session references (`docs/next/website/src/content/docs/session-state.mdx:73`), which Branchyard's recipes are adapted from.
- An agent-facing control surface with race-free waits: `agent.prompt` with `wait` pins the pane occupant and refuses when the agent is `blocked` (`socket-api.mdx:114`).

### Where Branchyard is ahead of Herdr

- State for Claude Code and Codex still comes from the screen (`docs/next/website/src/content/docs/agents.mdx:10`); Codex may stay `unknown` after a response (`agents.mdx:59`). Branchyard reads turn boundaries from the protocol.
- Permissions are not answered, only shown as `blocked`; a person or script sends keys (`agent-automation.mdx`, "Choose the control surface").
- Isolation, budgets, durable task state and merging: not found. Worktrees exist as a layout operation (`socket-api.mdx:104`).

## Warp

Warp (`warpdotdev/warp`, AGPL-3.0 application, MIT UI crates) is a GPU-rendered terminal with an agent subsystem under `app/src/ai/`. It runs harnesses two ways. Locally, it drives Claude Code, Codex and Gemini CLI as child processes in a pseudo-terminal it owns (`app/src/ai/agent_sdk/driver/terminal.rs`), reading their structured transcripts alongside (`driver/harness/claude_transcript.rs`, `codex_transcript.rs`). Remotely, "ambient agents" run on Warp's hosted service and the app polls them (`app/src/ai/ambient_agents/task.rs:155`, `ExecutionLocation`). `agent_sdk` is an internal name for the driver layer, not a published SDK.

It launches every third-party harness with approvals bypassed: `claude … --dangerously-skip-permissions` (`driver/harness/claude_code.rs:222`), `codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust` (`driver/harness/codex.rs:204`, `:209`), and `gemini --yolo` (`driver/harness/gemini.rs:107`). Its typed allow/ask/deny engine with reasons (`app/src/ai/blocklist/permissions.rs:31`, `:66`) governs Warp's own built-in agent, not these harnesses.

| Dimension | Warp |
|---|---|
| Harnesses | Claude Code, Codex, Gemini CLI (`driver/harness/{claude_code,codex,gemini}.rs`), besides Warp's own agent |
| Driven by | A pseudo-terminal per harness, with transcript parsing where the harness writes one |
| Approvals | Bypassed at launch for all three (above) |
| Provisioning | Warp's own MCP server and skill directories written into the harness (`driver/mcp_startup.rs`, `driver/harness/skill_dirs_publish.rs`), and Claude Code's API-key approval (`driver/harness/claude_code.rs:804`) |
| Resume | A per-harness `ResumePayload` fetched from Warp's server-stored transcript (`driver/harness/mod.rs:108`); a periodic workspace checkpoint at safe boundaries (`driver/checkpoint_coordinator.rs`) |
| Fork | Not found |
| **Mid-task messages** | A durable lead-to-child mailbox for Claude Code, delivered as a hook's `additionalContext` at the next hook boundary, with staged, surfaced and acknowledged stages, a local and a server cursor, and a size budget with a "more messages queued" note (`driver/harness/claude_code/parent_bridge.rs:1-9`, `:437`, `:587`). Not found for Codex or Gemini |
| Delegation | A parent/child conversation tree for Warp's own agent (`app/src/ai/blocklist/orchestration_topology.rs`, `child_agent_launch.rs:29`); no scoped authority for a child |
| Isolation | Local harnesses run as the desktop user; Codex's launch checks for an isolation platform (`driver/harness/codex.rs:642`); remote isolation is on Warp's service and not in this repository |
| Durability | Cursor-based consumption of the server's agent event stream (`app/src/ai/agent_events/driver.rs`) |
| Merging | Not found; a finished run records a pull-request link (`app/src/ai/artifacts/mod.rs`) |
| Budgets | Credits and cost tracked per task (`ambient_agents/task.rs:459`, `:467`); no limit found |
| Artifacts | A closed set of run deliverables: plan, pull request, external reference, screenshot, file (`app/src/ai/artifacts/mod.rs:31`) |
| Triggers | Linear, Slack, GitHub and GitLab webhooks, schedules and others start remote tasks (`ambient_agents/task.rs:41`) |
| APIs | None public |

### Where Warp is ahead of Branchyard

- A working mid-task mailbox into a running Claude Code session, which Branchyard is building now.
- A bounded shutdown ladder (`/exit`, a follow-up Enter, then a kill) and a kill that proves the process group belongs to the harness and is not the driver's own (`driver/harness/exit_escalation.rs`, `process_control.rs:39`).
- A polished human interface, typed run deliverables, and many ways to start a run.

### Where Branchyard is ahead of Warp

- Every tool permission of every driven harness is answered by policy; Warp bypasses them.
- Delegation carries scoped, attenuated authority; Warp's tree is for display and continuity.
- Cost budgets are enforced across a subtree; Warp tracks cost only.
- Candidates are merged only after checks on the exact target; Warp records a link.
- It is an open, portable contract; Warp's remote execution, events and delegation depend on its own service.

## Feature matrix

Paths in a column are relative to that project's repository at its surveyed commit.

| Dimension | Scion | OpenRig | Herdr | Branchyard |
|---|---|---|---|---|
| Harnesses covered | 9 provisioned bundles (`harnesses/`) plus a generic harness | Claude Code, Codex, Pi and terminal nodes (`packages/daemon/src/adapters/`) | 22 manifests (`src/detect/manifests/`); 24 kinds for `agent start` (`docs/next/website/src/content/docs/agent-automation.mdx`) | 17 profiles for 15 of 16 targets; 2 live-qualified ([compatibility](compatibility.md)) |
| How harnesses are driven | Interactive TUI in tmux inside the container; prompts and interrupts by `tmux send-keys` (`pkg/agent/manager.go:331`, `:391`); state from hook dialects (`harnesses/codex/dialect.yaml`; `pkg/sciontool/hooks/dialects/`) | Interactive TUI in tmux; text by `load-buffer`/`paste-buffer` (`packages/daemon/src/adapters/tmux.ts:366`); state from activity hooks relayed to `/api/activity/hooks` (`README.md`, "What OpenRig changes"); Pi over RPC (`adapters/pi-runner-protocol.ts`) | The terminal: screen manifests, or lifecycle hooks for six agents (`integrations.mdx:64`); input as keys or paste (`socket-api.mdx:93`) | Machine protocols only: Claude stream-json, Codex App Server, Antigravity and Amp stream-json, Pi RPC, ACP ([harness integration](harness-integration.md); `crates/branchyard-harness/src/`) |
| Provisioning into the harness home | Auth files, MCP translation, settings, telemetry, skills per harness (`harnesses/claude/provision.py`; `harnesses/claude/config.yaml:23`) | Managed blocks in `CLAUDE.md` or `CLAUDE.local.md`, `.claude/settings.local.json`, `.mcp.json`, Codex `config.toml` (`README.md`, "What OpenRig changes") | Hook scripts installed into each harness's config (`src/integration/targets.rs`) | Delegation MCP server and skill per harness, outside the worktree ([delegation](delegation.md#projection-per-harness)); Scion provisioning port in progress |
| Session resume | `resume_flag`, for example `--continue` (`harnesses/claude/config.yaml:61`), `resume --last` for Codex (`harnesses/codex/config.yaml:48`); suspend/resume and forced resume after a crash (`docs-site/src/content/docs/local/agent-lifecycle.md`) | `claude --resume <token>`, Codex resume (`packages/daemon/src/domain/native-resume-probe.ts:27`); `restore_policy` `resume_if_possible`, `relaunch_fresh`, `checkpoint_only` (`rigspec-schema.ts:40`) | Argument recipes from hook-reported session references (`src/agent_resume.rs:136`; `session-state.mdx:95`) | Native resume by session ID in each driver; a session that cannot resume fails rather than starting fresh ([compatibility](compatibility.md)). Herdr's argument-recipe builder was evaluated and found unneeded (below); only its official-agent-source registry check remains, in `crates/branchyard-controls/src/resume.rs` |
| Session fork | Not found | Seat fork from a live seat's context (`packages/cli/src/commands/fork.ts:34`; `adapters/claude-code-adapter.ts:216`) | Not found; `fork` recognized only as a session-start source (`src/agent_resume.rs:93`) | `Branch::fork` with native fork for Claude Code, Codex and Pi ([surfaces](surfaces.md)) |
| Permissions and approvals | Bypassed at launch (`harnesses/claude/config.yaml:60`; `harnesses/codex/config.yaml:45`); a deny list of Claude tools (`harnesses/claude/home/.claude/settings.json`) | Launch posture per seat: `builtin:locked`, `standard`, `open`, `yolo` or a custom file (`packages/daemon/policies/builtin/`); intent classes with allow/ask/deny (`policies/builtin/standard.policy.md`); YOLO off by default (`README.md`) | Not answered; `blocked` recognized from approval UI (`agents.mdx:59`) | Every request answered by ordered rules with allow, deny or ask fallback (`crates/branchyard/src/policy.rs`); profiles that cannot route requests are refused without `--allow-unapproved-tools` (README) |
| Delegation | Agents run `scion start` to create sub-agents; ancestry gives transitive access (`docs-site/src/content/docs/concepts.md:111`); roles capped by the parent (`GLOSSARY.md:235`); message modes for agent-to-agent messaging (`GLOSSARY.md`, "Branch mode") | By convention, through `rig send` and chat (`docs/as-built/architecture/architecture-rules-and-event-system.md:196`); edges `delegates_to`, `spawned_by`, `can_observe`, `collaborates_with`, `escalates_to` (`rigspec-schema.ts:30`) | Agents create panes and prompt each other through the CLI (`agent-automation.mdx`) | `by spawn`, Python, Rust and MCP; a per-turn token acts on descendants only; envelope of depth, width, harnesses, budget and denials ([delegation](delegation.md)) |
| Topology | Runtime-grown | Declared (RigSpec), then changed at runtime with `rig grow`, `expand`, `shrink`, `launch`, `remove` (`packages/cli/src/commands/`) | Ad hoc layout | Runtime-grown ([design §2](design.md#2-scope-and-invariants)); a [rig](rigs.md) declares the seats a root may fill |
| Isolation and sandboxing | Container per agent; tmpfs shadow mounts; projected env; read-only credentials (`concepts.md:138`); egress firewall script (`harnesses/claude/init-firewall.sh`) | None beyond the user; Codex `workspace-write` sandbox flag (`yolo-mode.ts:59`) | None | Local: OS user only. Microsandbox microVMs and Substrate actors, both unqualified ([providers](providers.md)) |
| Remote execution | Hub and Runtime Brokers; Kubernetes and Cloud Run (`pkg/runtime/k8s_runtime.go`; `pkg/runtime/cloudrun_runtime.go`) | Other hosts' daemons over HTTP (`domain/hosts/remote-daemon-http.ts`) | Remote Herdr servers over SSH (`src/remote/`) | `by serve` over HTTPS; harnesses on the server's host or in providers ([server](server.md)) |
| Workspaces | Shared-plain, worktree-per-agent, clone-per-agent (`docs-site/src/content/docs/local/workspaces-and-sharing.md`) | The seat's `cwd`; typed workspace declarations (`docs/as-built/architecture/workspace-primitive.md`) | `worktree.create` and friends (`socket-api.mdx:104`) | A git worktree per branch ([strategy](strategy.md#the-developer-model-branches)) |
| Durability and recovery | Hub state in SQLite or PostgreSQL (`hosted/ha/overview.md:57`); crash → `error` phase from the recovered exit code; stall detection and auto-suspend (`agent-lifecycle.md`) | SQLite daemon state; snapshots with `rig down --snapshot`, restore with per-seat outcomes (`docs/as-built/architecture/lifecycle-snapshot-restore.md`) | Live processes survive client detach; layout snapshots and optional screen history; processes do not survive a server restart (`session-state.mdx:8`) | SQLite or PostgreSQL; leases with fencing, journaled steps, durable cancel; a prompt is never resubmitted ([durability](durability.md)) |
| Merging work | Manual `git merge` locally, or a pull request (`workstation/git-projects.md:18`) | Not in the engine; the `standard` policy asks before `merge_or_release` (`policies/builtin/standard.policy.md`) | Not found | Checks on the exact merge in a temporary worktree, compare-and-swap on the target, conflicts returned (`crates/branchyard-workspace/src/integrate.rs`) |
| Observability and events | Activity and phase per agent (`GLOSSARY.md:328`); session metrics with token counts (`GLOSSARY.md:411`); OpenTelemetry (`pkg/sciontool/telemetry/`); the SSE event bus is "a latent capability" (`GLOSSARY.md:174`) | `RigEvent` union of 73 kinds, SSE at `/api/events` (`architecture-rules-and-event-system.md:99`, `:155`); transcripts captured from panes | `events.subscribe`, `events.wait`; agent status changes (`socket-api.mdx:755`); `agent explain` for detection | Per-branch event log with cursors; SSE activity feed across branches; signed webhooks by cursor for status changes, stalls and permission requests ([server](server.md#event-stream)) |
| Budgets and cost | `max_turns`, `max_model_calls`, `max_duration` (`limits.go:21`; `pkg/agent/run.go:737`); token metrics; no cost limit found | Token burn per seat (`usage.ts:41`); no limit found | Not found | USD, turns and minutes per branch; subtree reservations ([delegation](delegation.md#the-envelope)) |
| UI surfaces | Web UI with terminal (Lit, xterm.js); chat bridges (`extras/`); tmux attach | TUI (topology table and graph), older React web UI; Herdr or cmux terminal views (`README.md`, "Terminal UI and Workspaces") | The multiplexer TUI | `by watch` tree ([README](../README.md#watching-branches)); a Herdr plugin with a tab per branch ([`plugins/herdr`](../plugins/herdr/README.md)) |
| APIs and SDKs | Hub REST and WebSocket (`docs-site/src/content/docs/reference/api.md`); Go client (`pkg/hubclient/`); A2A bridge (`extras/scion-a2a-bridge/`) | Daemon HTTP; CLI with `--json`; MCP tools such as `rig_up`, `rig_send` (`README.md`, "How It Works") | Socket API with JSON Schema; CLI; plugins (`docs/next/website/src/content/docs/plugins.mdx`) | Rust SDK, HTTP JSON and SSE, Rust client, Python module, MCP ([surfaces](surfaces.md)) |
| Configuration model | Layered `settings.yaml`, templates, harness-configs (`docs-site/src/content/docs/reference/settings-precedence.md`) | RigSpec and AgentSpec YAML, bundles with SHA-256 integrity (`docs/reference/rig-spec.md`; `docs/reference/rig-bundle.md`) | TOML config; manifest overrides (`agents.mdx:67`) | Per-call options and CLI flags (`TaskOptions`); rig specs in TOML ([rigs](rigs.md)) |

## Vendored pins

`vendor/` holds fragments from 16 September 2026 ([third-party notices](../THIRD_PARTY.md)). Each vendored file's Git blob was compared with the same path at the surveyed commit, and the commits between were counted in a blobless fetch.

| Project | Pinned | Pin date | Commits since | Vendored files unchanged | Changed |
|---|---|---|---:|---:|---|
| Scion | `54b9387` | 2026-09-16 | 318 | 34 of 44 | `claude/config.yaml`, `claude/provision.py`, `claude/provision_test.py`, `codex/config.yaml`, `codex/provision.py`, `codex/provision_test.py`, `gemini-cli/provision.py`, `grok-build/config.yaml`, `grok-build/provision.py`, `grok-build/provision_test.py` |
| Herdr | `5f3763d` | 2026-09-16 | 69 | 20 of 25 | `src/agent_resume.rs`, `src/detect/manifest.rs`, manifests `codex`, `grok`, `kiro` |
| OpenRig | `cc75efd` | 2026-09-14 | 49 (17 on the first-parent line) | 5 of 6 | `packages/daemon/src/domain/runtime-adapter.ts` |

What changed:

- **Scion.** Model aliases now name concrete models (`harnesses/claude/config.yaml:43`; `harnesses/codex/config.yaml:39`). Grok launches with `--trust --disallowed-tools x_search` added to its bypass flags (`harnesses/grok-build/config.yaml:44`). Claude, Codex and Gemini provisioners gained native telemetry setup. Claude's `provision.py` now resolves model aliases itself (`_resolve_model_alias`). **The known incompatibility in [validation](validation.md#known-upstream-incompatibility) is resolved upstream**: in a scratch copy of the surveyed `harnesses/`, Claude's suite passes 13 of 13, and the Codex, Grok, Copilot, Muse and Hermes suites and `scion_harness_test.py` pass. Bumping the pin should remove the exclusion in `tools/test_scion.py` and the baseline in `tools/check_scion_compatibility.py`.
- **Herdr.** `agent_resume.rs` changed only by adding `Serialize, Deserialize` to one derive; the recipes are identical, so `patches/herdr-resume.patch` still describes the adaptation. `manifest.rs` dropped `should_skip_state_update` and falls back to `unknown` rather than `idle` for Codex. The Kiro manifest now requires detection engine 2 (`min_engine_version = 2`). `tools/check_catalog.py` reads only IDs, versions and rule IDs, so it is unaffected; any future evaluator must honour engine versions.
- **OpenRig.** `LaunchHarnessOpts` gained `claudeManagedBlockFile` (`runtime-adapter.ts:17`). The four runtime fragments are unchanged.

`vendor/` was not updated in this change. Follow [updating an upstream](vendoring.md#updating-an-upstream).

### What the fragments missed

The vendored files are launch fragments. The full repositories contain material Branchyard's documents did not account for:

| Project | Missed | Where |
|---|---|---|
| Scion | Hook dialects that normalize each harness's hook events into one activity vocabulary | `pkg/sciontool/hooks/dialects/`, `harnesses/*/dialect.yaml` |
| Scion | Capability declarations with support level and reason per feature | `harnesses/claude/config.yaml:64` |
| Scion | Agent roles capped by the parent, and message modes limiting who may message whom | `GLOSSARY.md:235`, "Branch mode" |
| Scion | Stall detection, self-declared `blocked`, auto-suspend and wake on message | `docs-site/src/content/docs/local/agent-lifecycle.md` |
| Scion | Reincarnation: same identity, new generation, handoff as first task | `GLOSSARY.md:17` |
| Scion | Egress firewall for the Claude image | `harnesses/claude/init-firewall.sh` |
| OpenRig | Declarative rigs, startup orchestration and delivery hints | `docs/reference/rig-spec.md`, `domain/startup-orchestrator.ts` |
| OpenRig | Permission posture policies as intent classes | `packages/daemon/policies/builtin/` |
| OpenRig | Resume honesty statuses | `domain/native-resume-probe.ts:6` |
| OpenRig | Runtime topology changes, which the old strategy table said OpenRig left to others | `packages/cli/src/commands/grow.ts`, `expand.ts`, `shrink.ts` |
| OpenRig | Herdr and cmux as terminal providers for a rig | `README.md`, "Terminal UI and Workspaces" |
| Herdr | Lifecycle hooks as the status authority for six agents; screen scraping only for the rest | `docs/next/website/src/content/docs/agents.mdx:41` |
| Herdr | Hook integrations that report native session IDs; the recipes depend on them | `src/integration/assets/claude/herdr-agent-state.sh` |
| Herdr | Automatic manifest updates from herdr.dev | `agents.mdx:67` |
| Herdr | Socket API schema, event subscriptions and plugins | `docs/next/api/herdr-api.schema.json`, `plugins.mdx` |

## Absorption plan

Types: **port** copies or translates code, keeping attribution under Apache-2.0 §4 (header, modification notice, patch, as for `resume.rs`); **idea** reimplements a design without copying code; **client** means the other project runs on top of Branchyard's API; **don't** means out of scope.

### Scion

| Item | Type | Effort | Depends on | Notes |
|---|---|---|---|---|
| Harness provisioning: auth files, MCP configuration, settings and telemetry written into the private harness home | port | In progress (another engineer) | Pin bump to `d9b9e6a` first, since the Claude provisioner and tests now agree | Keep the rules in [vendoring](vendoring.md#scion-reuse-environment-projection): no permission-bypass flags, no project-to-global MCP scope demotion, secrets only in private per-run storage |
| Capability reasons in the compatibility matrix | idea (done) | 0.5 day | None | Scion states why a capability is missing (`max_model_calls: { support: "no", reason: … }`). Every unsupported or partial driver capability now has a reason (`Driver::capability_reasons`, sourced from each driver's own refusal messages and doc comments, "not verified" where none exists), rendered as a "Reasons" column in [compatibility](compatibility.md) and looked up for admission errors (`branchyard_harness::reasons_for`) |
| Stall detection and self-declared `blocked` | idea (done) | Done | Server mode; Substrate suspend for the resource win | [Lifecycle](lifecycle.md#stall-detection): a branch waiting on children or on a permission is not stalled, known from structured state, so it needed no self-report. `blocked` itself (a status a harness declares) is not built — Branchyard reads permission waits from the protocol instead |
| Reincarnation as a fork with a fresh session and handoff brief | idea (done) | Done | None | [Lifecycle](lifecycle.md#reincarnation): useful when a harness version or profile changes under a long-lived branch |
| Roles capped by the parent; message modes | done | — | — | The envelope already attenuates roles. Parent/descendant messaging (ask, report, escalate, answer, an inbox) is built, authority following the delegation tree; sibling-to-sibling messaging is not, so `collaborates_with` is still refused |
| Egress allowlist | idea | With provider qualification | Microsandbox or Substrate network policy | Use the runtime's network controls; do not ship an iptables script |
| Hub, brokers, chat bridges, Skill Bank, A2A bridge | don't | — | — | Product surface beyond Branchyard's contract |
| Tmux keystroke control and hook-inferred state | don't | — | — | Branchyard drives protocols |

### OpenRig

OpenRig's most useful contribution is the declarative layer Branchyard lacks. Its engine is TypeScript bound to tmux, so nothing is ported: the absorption is a schema and a lowering onto operations Branchyard already has.

| Item | Type | Effort | Depends on | Notes |
|---|---|---|---|---|
| `by rig`: a declarative spec lowered to a task, its envelope and declared children | idea (done) | Done | — | [Rigs](rigs.md); as built and deviations below |
| Startup delivery: guidance, skills and a first message | idea | Guidance done; 2–3 days for the rest | Scion provisioning port | `guidance_merge` is done: a rig's startup files become the seat's instructions through provisioning ([rigs](rigs.md)). `skill_install` would project into the plugin directory or private home, never the worktree, and `send_text` would be a prefix of the first prompt, as ACP instructions are delivered today ([delegation](delegation.md#projection-per-harness)) |
| Permission posture presets and intent classes | idea | 3–5 days | A rule kind that matches command input | Branchyard rules match tool names (`crates/branchyard/src/policy.rs`). Classes such as `force_push` need matching on a shell command, like the existing delegation-command rule. `yolo` has no mapping: Branchyard never bypasses |
| Resume honesty vocabulary | idea | 0.5 day | None | Branchyard's drivers already fail a resume they cannot confirm. Name the outcomes `resumed`, `failed` and `attention_required` in branch status so clients can show them |
| Activity hooks | don't, for now | — | A terminal-attached profile, if one is ever added | Protocol drivers already emit tool and turn events. OpenRig's hooks exist because it drives TUIs |
| OpenRig as a client | client | Upstream work | Branchyard's HTTP API | OpenRig's five-method `RuntimeAdapter` could gain a Branchyard runtime whose seats are branches. That is OpenRig's decision; Branchyard needs only a stable API |
| Queue, chat rooms, workflows, services, culture files | don't | — | — | Coordination conventions above Branchyard's contract |

#### `by rig` (done)

Built as designed in outline, with the deviations listed below; [rigs](rigs.md) is the reference. A rig spec declares what may exist, not a fixed graph. It lowers to one root branch whose envelope permits exactly the declared seats; the root's harness fills them at runtime with `by spawn --seat`, so topology is still grown by the meta-harness, inside a declared shape.

| RigSpec | Branchyard, as built |
|---|---|
| `name` | Root branch name; children default to `<parent>-<seat>` |
| Pod | `[pods.NAME]` with a description and shared startup files; no runtime record |
| Member | A seat, `[seats.NAME]`, spawned by name when its parent chooses (`start = "on_demand"`) |
| `runtime` | `harness`: a harness or profile ID, checked against the registry at plan time |
| `model` | `model`, with `effort`, `auth`, `secrets` (names), `mcp` and `telemetry`, lowered to the seat's [provisioning](provisioning.md) |
| `delegates_to` from A to B | `seats.A.delegates_to = ["B"]`. A's envelope allows B's harness, B's `instances` more children and B's budget |
| `spawned_by` | Refused: declare the edge on the parent |
| `can_observe` | Refused: authority is still descendants only |
| `escalates_to` from A to an ancestor C | `seats.A.escalates_to = ["C"]`, `C` a seat above A's beyond its own parent (always allowed); `by escalate` reaches it, authority checked against the tree |
| `collaborates_with` | Refused: messaging is parent/descendant only, no sibling-to-sibling |
| `permission_policy` | Refused: presets are not implemented. The root seat has explicit `policy` rules (`default` allow, deny or ask; `deny`; `allow`; `delegation_commands`); a child seat may only add `deny` |
| `startup.files` | Standing instructions through provisioning, after a generated section naming the seat and the seats it may spawn; never written to a worktree. Paths are relative, without `..`; `required = false` skips a missing file |
| `startup.actions`, `delivery_hint` | Refused: the prompt is the first message, and drivers refuse slash commands |
| `restore_policy: resume_if_possible` | Accepted: the default behaviour |
| `restore_policy: relaunch_fresh`, `checkpoint_only` | Refused: a send resumes or fails |
| `continuity_policy`, `culture_file`, `services` | Refused, per [design invariant 8](design.md#2-scope-and-invariants) |
| (none) | Added: `budget`, `check`, `isolated` and `instances` per seat; the root seat's budget bounds the whole tree |

Deviations from the design above, and why:

- **TOML, not YAML.** A TOML parser (`toml_edit`) was already in `Cargo.lock`; YAML would have added a dependency. The format is Branchyard's own, not RigSpec-compatible, and recognises OpenRig's field names only to refuse them with a reason.
- **`by rig check` and `by rig run`**, not `plan` and `up`, with `--json` for both.
- **No eager seats.** The root starts alone; `start = "eager"` is refused. Starting children beside the root needs `by rig run` to spawn them once the root's record exists but while its turn runs, which `TaskBuilder::run` does not expose; the root can spawn them first thing instead.
- **Spawning is only by seat inside a rig.** The design let the lead also spawn freely within the envelope; a rig's branches spawn only the seats their own seat delegates to, so the file is the whole shape. `instances` lets a seat be filled more than once.
- **Seats are stored with the root branch** (`TaskOptions::seats`, kept in its grant) and each child gets the subtree below its seat, rather than the planner living only in the CLI: the server and the SDK enforce them the same way. The planner itself is in `branchyard-cli`, as designed.
- **Harnesses are checked against the registry, not for installation**, so a rig can be planned on a machine that runs it remotely. Installation is checked when each branch starts.
- **No read grant.** `observes` was to be implemented; it is refused until authority can extend beyond descendants.

### Herdr

| Item | Type | Effort | Depends on | Notes |
|---|---|---|---|---|
| Resume recipes as a fallback | port (evaluated and removed) | Done (0 days remaining) | None | No profile qualified: every native driver (Claude Code, Codex, Antigravity, Pi, Amp) already builds the same resume argv from its own protocol handshake, and every ACP profile's resume is negotiated inside the ACP connection itself, so a Herdr CLI recipe would launch a process the `Acp` driver cannot speak to, not resume the session. The unused argument builder was deleted; `is_official_agent_source` (the registry check) stays, along with the patch and its test. Full reasoning in [vendoring](vendoring.md#herdr-reuse-the-official-agent-source-registry-only) |
| Session-identity hooks | idea | Only with a terminal-attached profile | A decision to support adopting sessions started outside Branchyard | Herdr's hooks show where each harness reports its session ID (for Claude Code, the `SessionStart` hook). Branchyard's drivers get the ID from the protocol handshake |
| Terminal detection manifests | idea, observation only | None now | A human-facing terminal view | Never authorize, certify or merge from them ([vendoring](vendoring.md#herdr-reuse-the-official-agent-source-registry-only)). If one is needed, run Herdr rather than port its detection engine; never fetch manifests at runtime as Herdr does (`agents.mdx:67`) |
| Herdr as a client of Branchyard's feed | client (option B done) | 0 days for option A; option B built | Server SSE feed (exists) | Below; [`plugins/herdr`](../plugins/herdr/README.md), not yet run against a real Herdr |
| Multiplexer, plugins, remote SSH machines | don't | — | — | Branchyard excludes a terminal emulator ([design §3](design.md#explicit-exclusions)) |

#### Herdr as a client

Branchyard runs harnesses on protocol pipes, not terminals, so a Branchyard branch cannot also be a Herdr pane with a live harness. Herdr can instead present Branchyard's state:

- **A, today.** Run `by watch` (or `by --remote URL watch`) in a Herdr pane. It follows the SSE feed and needs no code.
- **B, a Herdr plugin.** Herdr plugins are ordinary commands that call the Herdr CLI (`plugins.mdx`). A plugin subscribes to `GET /v1/repos/{repo}/events/stream` ([server](server.md#event-stream)), opens one pane per branch running `by log <branch>`, and reports each branch's state on its pane with `herdr pane report-agent --source custom:branchyard` (`integrations.mdx`, "Integrate your own agent"). Mapping: `running` → `working`; a pending permission under `--ask` → `blocked`; `ready`, `no_changes` and `merged` → `idle`; `failed`, `interrupted` and `budget_exceeded` → `idle` with the reason as the message. Herdr's sidebar then shows which branch needs attention, and its plugin actions can run `by merge` or `by cancel`. The plugin lives outside this repository or in `plugins/`, uses only Branchyard's public API, and copies no Herdr code.

OpenRig already treats Herdr as a terminal provider (`README.md`, "Terminal UI and Workspaces"), which suggests option B would find users.

**Option B is built** as [`plugins/herdr`](../plugins/herdr/README.md): a Herdr plugin v1 manifest (`herdr-plugin.toml`, checked against Herdr's loader, `src/app/api/plugins/manifest.rs`) whose commands run `branchyard-herdr`, a workspace binary on `branchyard-client`. It is tested against `by serve` and a fake `herdr` that records its calls, not against Herdr itself; [live testing](testing-live.md#8-herdr-plugin-no-model-calls) has the check. Where it differs from the plan above:

- **Branch panes are plugin panes in tabs.** Each is a `[[panes]]` entrypoint opened with `herdr plugin pane open --placement tab --no-focus`, the branch passed with `--env`, rather than a shell pane given a command, so no shell quoting is involved. The pane runs `by log --follow`, a new option; `by log` alone printed once and exited.
- **The bridge is a pane, not a startup hook.** Herdr's `[[startup]]` hooks are one-shot and unsupervised (`plugins.mdx`, "Startup hooks"), and the bridge runs for as long as it follows the feed, so it is a `bridge` pane entrypoint opened by the `start` action.
- **`--ask` cannot happen remotely.** The server refuses `--ask`, so `blocked` shows only when a local `by run --ask` works on a served repository and its unanswered request reaches the feed. A request the policy answers at once is debounced away.
- **Settled statuses all carry a message** (`ready to merge`, `no changes`, `merged into main`), not only the failed ones.
- **Send is a popup.** Plugin actions take no input, so `send` opens a `popup` entrypoint that reads the prompt and runs `by send`.
- **Server only.** A local repository is followed through `by serve` on loopback rather than by reading its store directly.

### Warp

Ideas only; nothing is ported, because Warp's application is AGPL-3.0.

| Item | Type | Effort | Depends on | Notes |
|---|---|---|---|---|
| A durable mailbox delivered at the next boundary, with acknowledgment, a cursor and a size budget | idea | In the inbox work | Branchyard's inbox and steer | For a harness with no live input, deliver at its next hook or turn boundary; say so in the delivery semantics |
| Prove a process group before killing it | idea | 0.5 day | None | Branchyard already matches pid and start time; add a check that the group is not the engine's own |
| Named reasons for policy decisions | idea | 0.5 day | None | Branchyard records the rule that decided; a reason vocabulary would make audits easier |
| A closed set of run deliverables for display | idea | 1–2 days | Artifacts | Keep it separate from scratch areas |
| Remote tasks started by external triggers | idea | Later | Webhooks | Low priority |
| Warp's hosted service, terminal UI, notifications and billing | don't | — | — | Not a reusable contract; a terminal emulator is excluded ([design §3](design.md#explicit-exclusions)) |

`vendor/warp-agpl` stays as it is: five files, at `2f0db5c5edd8134f0e858aebbc0ecd0db2f91d38`, substantively unchanged at the surveyed commit. The mailbox and the permission engine are left out of it: they are designs to reimplement, not source to keep.

## Surprises

- Scion fixed the Claude model-alias mismatch that Branchyard documents as a known upstream incompatibility; the upstream suite passes at `d9b9e6a`.
- OpenRig grows and shrinks running rigs and has permission policies, so the old table's "leaves runtime-grown topology" no longer holds. Its policies still act at launch, not per invocation.
- Herdr is not purely a screen scraper: hooks are the status authority for six agents and report session IDs for twelve. Claude Code and Codex state still comes from the screen.
- Herdr fetches detection manifests from herdr.dev at runtime by default. Anything Branchyard takes from those manifests must stay pinned.
- Scion, like OpenRig, drives every harness as a TUI in tmux, and bypasses approvals for Claude Code, Codex and Grok. None of the three answers individual tool permissions.
- Warp, like Scion, launches every third-party harness with approvals bypassed; its permission engine governs only its own agent.
- Warp's most relevant mechanism for Branchyard, a mailbox into a running Claude Code session, is not in the vendored files; it exists because Claude Code's protocol has no live input, so Warp delivers at a hook boundary.
