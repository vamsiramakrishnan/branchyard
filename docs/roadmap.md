# Roadmap

Written 30 September 2026 from four studies: an audit of Branchyard itself; Manus, Genspark, Devin, Codex cloud, Claude Code, Cursor, Jules, Factory, OpenHands, Amp and Kiro; emdash, Superset and Orca; and the MCP authorization specification with the gateways and credential brokers around it.

## Where the value is

Two products whose main offer was running coding CLIs in worktrees (Terragon, and the company behind Vibe Kanban) shut down in 2026. The leaders compete on what a worktree UI cannot copy: environments that start warm, judging results automatically, routing each task to the right agent and model, connectors, and triggers. Branchyard's strengths (policy, durable execution, validated merge, delegation) belong there. Worktree conveniences are taken from emdash (Apache-2.0) and Orca (MIT) by porting; Superset (Elastic License 2.0 since February 2026) is replicated from behaviour only.

## Wave 1

| Track | What it gives you | Learned from |
|---|---|---|
| Connectors ([design](connectors.md)) | `by connect github` once; every branch gets a skill and SDK per granted connector and calls one gateway; no harness sees an upstream credential or needs MCP support | Codex and Claude Code's credential proxies, Manus's on-demand skills, the MCP authorization specification; Anvil compiles the connectors |
| Judge and router | A judge scores `by fan`'s candidates and proposes one; a fleet table maps each task kind to harness, model, effort, attempts and budget; outcomes are recorded and routing learns from them; a failed harness fails over to the next | Jules and OpenHands critics, Claude's ultrareview, Factory's router, Devin Adaptive, Genspark |
| Prepared environments | `[workspace]` setup runs once per setup hash; new worktrees start from the prepared copy (reflinks, shared ignored directories, `.worktreeinclude`), sandboxes from a prepared snapshot, falling back to the last good build; workers carry labels | Cursor builds, Devin snapshots, Codex environments, Orca's worktree sharing |
| Ports | `by review` (comment on diff lines, send once); `by pr --watch` resolves the threads it addressed; about 40 harness CLIs in the registry; config imported from emdash, Orca, Superset and Conductor; a starting catalog of connectors | Orca, emdash |

**Ports: done** (1 October 2026, hermetic tests only; nothing ran against GitHub or a real harness). `by review` sends comments written on a branch's diff in your editor as one prompt, in Orca's format, and `v` in `by watch` runs it ([pull requests](pull-requests.md#by-review)); `by pr --watch` answers "Addressed in <commit>" to, and resolves, the review threads a pushed fix addressed (`--no-resolve` to leave them); the harness registry maps 47 CLIs, every agent in emdash's and Orca's registries, and `by harnesses --all` shows their install and login commands, API-key variables and models where upstream records them ([compatibility](compatibility.md#known-not-driven)); `by init project` imports `.emdash.json`, `orca.yaml`, `.superset/config.json` and `.conductor/settings.toml` ([setup](setup.md#importing-another-tools-workspace-configuration)); `catalog/connectors.toml` lists 55 MCP servers from emdash's catalog for Anvil to adopt ([connectors](connectors.md#catalog)), shown by `by connectors catalog`. emdash and Orca files are vendored and pinned; Superset is replicated from its documentation only ([vendoring](vendoring.md#emdash-and-orca-ports-as-data-and-as-translations)).

## Wave 2

Triggers and schedules (cron, GitHub, Slack, Linear, email; a precheck, a test run, pause after repeated failures); priority and fair share in the queue; quota meters per login; Linear, Jira and GitLab issues; listening ports per branch; `by adopt` for existing Claude and Codex sessions; metrics and traces for Branchyard itself.

## Wave 3

A web and phone companion on the server's event stream; `by --remote ssh://`; environment recipes (repo scripts that create a VM); repository knowledge that learns from sessions, adopted after review; plan approval and goals a judge verifies; prebuilt binaries and an image with harnesses.
