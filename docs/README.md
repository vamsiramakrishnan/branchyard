# Documentation

If you are new to Branchyard, start with [Getting started](getting-started.md) and [Concepts](concepts.md). To see what is tested and working today, check [Status](status.md). Every documentation page in this directory is indexed below by topic.

## Get started

Read these pages in order when setting up Branchyard for the first time.

| Guide | What it covers |
|---|---|
| [Getting started](getting-started.md) | Install `by`, run `by setup`, delegate your first task, inspect its diff, merge the result, and compare multiple harnesses. |
| [Setup](setup.md) | Configure a repository with `by setup` or the interactive `by init` wizard, write `branchyard.toml`, and install harness hooks and skills. |
| [Doctor](doctor.md) | Check whether your machine, repository, configuration, harnesses, sandbox provider, and server connection are ready, without modifying any state. |
| [Up](up.md) | Bring a repository's configured backends up in one command (`by up`), install allowed harnesses, start the connector gateway, and run readiness checks. |
| [Concepts](concepts.md) | Core terminology (task, branch, turn, checkpoint, candidate, harness, profile, provider) and what each branch status means. |
| [Modes](modes.md) | Execution and storage modes (local, isolated, sandbox, shared server, and PostgreSQL), what each mode guarantees, and how to verify it. |
| [Distribution](distribution.md) | Install prebuilt `by` binaries, install Branchyard skills and hooks into each harness, and build release and plugin archives. |

## Guides

Task-focused guides for daily workflows.

| Guide | What it covers |
|---|---|
| [Cooperative teams](cooperative-teams.md) | Coordinate native coding and knowledge workers, validate structured JSON deliverables, and pass frozen handoffs between branches. |
| [Knowledge and design skills](knowledge-skills.md) | Use built-in skills to gather evidence, analyze tables, and generate and validate documents, spreadsheets, and slide decks. |
| [Tools: one model](tools-model.md) | How connectors, skills and helpers are chosen, mounted as one searched tool tree, and kept out of the context a turn does not need. |
| [Skill-discovery eval](skill-eval.md) | Measure with a real model whether an agent finds and opens the right skill, connector operation or helper through the mounted tool tree: 48 scenarios, a log scorer, a runner, and the static sizes a turn pays for. |
| [Document previews](artifact-previews.md) | Preview CSV, Excel, Word, PowerPoint, PDF, and HTML artifacts safely while preserving originals and enforcing read permissions. |
| [Workspace runtime controls](workspace-runtime.md) | Route interactive terminals and workspace lifecycle actions to the owning worker and sandbox allocation. |
| [Watching branches](watch.md) | Monitor all branches live in the terminal with `by watch`, run keyboard actions on any branch, and configure desktop or terminal notifications. |
| [Checkpoints, rewind, try and compare](checkpoints.md) | Rewind or fork a branch at any turn checkpoint, test a branch's changes in your main checkout with `by try`, and compare parallel attempts. |
| [Pull requests](pull-requests.md) | Start branches from GitHub, GitLab, Linear, or Jira issues, open pull requests with `by pr`, and feed CI failures or review comments back into a branch. |
| [Workspace lifecycle](workspace.md) | Copy `.env` files, run dependency setup and teardown scripts, assign per-branch ports, and manage workspace script trust. |
| [Prepared environments](environments.md) | Build dependencies once per lockfile and clone, link, or snapshot them into each new branch worktree (`by env` and `.worktreeinclude`). |
| [Warm pools](pools.md) | Keep pre-built worktrees ready in the background so new branches start immediately. |
| [Fleet: routing, judging and failover](fleet.md) | Automatically pick a harness, model, and effort per task, score parallel `by fan` attempts with `by judge`, and fail over when a harness errors. |
| [Plans and goals](plans-and-goals.md) | Run a branch in read-only planning mode for approval before editing files, and verify completion criteria with a goal judge. |
| [Repository knowledge](knowledge.md) | Distill reusable repository rules from completed branches, review and adopt them, and inject them into matching harnesses. |
| [Delegation](delegation.md) | Let an agent spawn and coordinate child branches via `by`, Python, Rust, or MCP within bounded budgets, paths, and permissions. |
| [Messaging](messaging.md) | The one layer every harness-to-harness message goes through: envelope, addresses, kind registry, pipeline and delivery classes; what is built and what is designed. |
| [Rigs](rigs.md) | Define a multi-agent team layout in TOML, validate it with `by rig check`, and run it with `by rig run`. |
| [Wide map](map.md) | Run a prompt across a list of inputs in parallel (`by map`), validate each branch's output against a JSON schema, and collect results into a table. |
| [Triggers and schedules](triggers.md) | Start tasks on cron schedules, intervals, or signed webhooks from GitHub, Slack, and Linear with deduplication and failure circuit-breakers. |
| [Connectors](connectors.md) | Give branches scoped access to GitHub, Slack, and internal APIs through a local connector gateway without exposing raw credentials. |
| [Tools: one model](tools-model.md) | Design, not built: how connectors, skills and helpers are made available, allowed, readied, defaulted and chosen per query; the mounted tool tree that saves context; identity to the gateway; and one turn's lifecycle on GCE, GKE and Substrate. |
| [Usage and adopting sessions](usage.md) | Check 5-hour and weekly usage limits for Claude Code and Codex (`by usage`), avoid rate-limited logins, and adopt existing local sessions into branches (`by adopt`). |
| [Web companion](companion.md) | Monitor, steer, approve, and merge branches or manage knowledge workspaces from a desktop or mobile browser with `by serve --app`. |
| [Harness hooks](harness-hooks.md) | Notify interactive agent sessions about branch completions, questions, and failures through harness hooks and status lines. |
| [Workspace management](workspace-management.md) | Manage code and knowledge workspaces through an authenticated MCP client and publish checked pull requests from the server. |
| [SDKs](sdk.md) | Drive Branchyard from Python or Rust, map chat threads to branches, stream events, and manage connector grants. |
| [Deploying `by serve`](deploy.md) | Run `by serve` in Docker or Docker Compose backed by PostgreSQL, and verify host readiness before deployment. |
| [Remote over ssh](remote-ssh.md) | Run `by` commands against a repository on a remote SSH host (`by --remote ssh://…`) with automatic Unix socket forwarding and no manual server setup. |
| [Task repositories](task-repos.md) | Run tasks in folders that are not Git repositories by storing both files and conversation history in an automatic task repository. |
| [Troubleshooting](troubleshooting.md) | Diagnose stuck, failed, interrupted, or waiting branches and look up common error messages and their fixes. |
| [Upgrade](upgrade.md) | Upgrade `by`, migrate `.branchyard/state.db` safely, refresh harness plugins and hooks, and update agent CLIs. |
| [Adding a harness](adding-a-harness.md) | Add support for a new agent CLI, from its catalog entry and hook definitions to live protocol qualification. |
| [Writing a driver](writing-a-driver.md) | Implement a harness driver or profile, test it against recorded transcripts, and run the qualification suite. |

## Reference

Detailed specifications, command references, and validation records.

| Page | What it covers |
|---|---|
| [Status](status.md) | Current implementation status, live-qualified harness versions, and remaining gaps. |
| [Surfaces](surfaces.md) | Feature matrix across the SDKs, local CLI, `--remote`, HTTP API, Rust client, and child delegation. |
| [Server and remote mode](server.md) | Configure and run `by serve`, use the HTTP API and `--remote` client, and understand durability and security boundaries. |
| [Command line](cli.md) | Global flags, environment variables, configuration precedence, shell completions, exit codes, and terminal permission handling. |
| [Command reference](cli-reference.md) | Complete CLI flag and subcommand reference generated from `by`'s clap definitions. |
| [Sandbox providers](providers.md) | The sandbox provider contract, local provider guarantees, and running harnesses in Microsandbox microVMs or Agent Substrate actors. |
| [Compatibility](compatibility.md) | Supported harnesses, driver profiles, protocol capabilities, and qualification status. |
| [Driver qualification](qualification/README.md) | Live protocol qualification reports and scenario results for each harness driver. |
| [Validation](validation.md) | Summary of passing end-to-end checks, upstream harness issues, and untested areas. |
| [Substrate on GKE](substrate-gke.md) | Plan, provision, or adopt a dedicated GKE cluster for Agent Substrate using pinned upstream tools. |
| [Compute on GCE](compute-gce.md) | Provision a dedicated GCE worker VM for Antigravity and connect over native SSH or IAP. |
| [Agent Sandbox on GKE](agent-sandbox-gke.md) | Prepare private gVisor and Kata MicroVM pools with a pinned controller and runtime policies |
| [Cloud execution](cloud-execution.md) | Select native GCE and GKE runtimes through CLI options and console presets |
| [Admin compute console](admin-console.md) | See and plan the GCE and GKE compute a server places turns on; developers connect with `by --remote` |
| [Container runtime](container-runtime.md) | Install and qualify the pinned gVisor Docker runtime |
| [Container provider](container-sandbox.md) | Run an owned container with copied files and enforced resource limits |
| [Agent Sandbox provider](agent-sandbox.md) | Run owned GKE gVisor and managed Kata allocations |
| [Microsandbox runtime](microsandbox-runtime.md) | Build, verify and install the patched MicroVM runtime |
| [GCE validation, 2 November 2026](remote-validation-2026-11-02-gce.md) | Read the patched MicroVM runtime and adapter evidence |
| [GCE container evidence, 2 November 2026](evidence/gce-container-2026-11-02/README.md) | Read the actual gVisor isolation and resource-limit observations |
| [GKE native evidence, 2 November 2026](evidence/gke-native-2026-11-02/README.md) | Read the real gVisor and Kata provider checks and qualification limits |
| [AIPlex run, 14 November 2026](evidence/aiplex-control-plane-2026-11-14.json) | Read the record of the real `by` run against a real `aiplex dev` (mock providers, a stand-in agent): each check and what was observed |
| [AIPlex signed-token run, 14 November 2026](evidence/aiplex-signed-token-2026-11-14.json) | Read the record of the real `by serve` run against a real `aiplex dev` with a token the server signs (mock providers, a stand-in agent): each check and what was observed |
| [Substrate setup](substrate-setup.md) | Save an Agent Substrate connection, prepare an actor template, and verify bridge execution with a disposable actor. |
| [Remote validation, 28 October 2026](remote-validation-2026-10-28.md) | Validation record for remote compute backends and developer setup workflows. |
| [Console validation, 29 October 2026](console-validation-2026-10-29.md) | Validation record for web console and remote execution checks. |
| [Live testing](testing-live.md) | Checklist and recording procedure for running live tests with real models, credentials, and KVM. |
| [CI gates](ci-gates.md) | Automated repository checks for dependencies, licenses, documentation, schemas, lint ratchets, and test coverage. |
| [Vendoring](vendoring.md) | Vendored upstream components, how they are adapted, and how patches are verified. |
| [Contributing](../CONTRIBUTING.md) | How to build, test, and verify changes before pushing, along with codebase conventions and ratchets. |
| [Third-party notices](../THIRD_PARTY.md) | Upstream revisions and licenses for vendored and ported code. |
| [Adding a harness](adding-a-harness.md) | Step-by-step checklist of files and tables to update when registering a new harness. |

## Design and internals

Architecture notes and internal specifications. Some pages describe target designs alongside what is built today; see [Status](status.md) for current coverage.

| Page | What it covers |
|---|---|
| [Architecture](design.md) | Core engine architecture: how the SDK, worktrees, topologies, budgets, storage, recovery, and merges fit together. |
| [Durable execution](durability.md) | Crash recovery, operation leases, journaled steps, cancellation, and how the state store maps onto SQLite and PostgreSQL. |
| [Branch lifecycle](lifecycle.md) | Stall detection, operator webhook notifications, and switching a branch's harness or model via reincarnation. |
| [Task graphs](graph.md) | Dependencies between delegated child branches and atomic task-graph updates. |
| [Effects, approvals and undo](effects.md) | How external connector writes are recorded in a ledger, gated by approval policies, reconciled after crashes, and undone when supported. |
| [Shared storage](storage.md) | How branches publish immutable artifacts and share single-writer scratch directories. |
| [Ambient registry](registry.md) | How local background services announce capabilities with leases, are discovered by repository, and get reclaimed on exit. |
| [Harness integration](harness-integration.md) | How Branchyard drives sixteen harnesses through ACP and native streaming protocols. |
| [Provisioning](provisioning.md) | How a branch's private home directory is prepared before each turn with secrets, MCP servers, instructions, model settings, and telemetry. |
| [Artifact toolkit](artifact-toolkit.md) | How document skills create, edit, render, and validate DOCX, XLSX, PDF, and PPTX files, and assemble multi-part slide decks. |
| [Artifact capture](artifact-capture.md) | How declared output files are collected after a turn finishes and published before deliverables are accepted. |
| [Artifact preview component](artifact-preview-component.md) | How the web console renders authenticated document previews and cleans up pending requests and object URLs. |
| [Console fixtures](examples/console-fixtures.md) | How to generate deterministic sample documents and tables for console testing. |
| [Native identity](identity.md) | How Google ADC, verified server callers, and connector accounts preserve native IAM and upstream permissions. |
| [Atomic knowledge validation, 29 October 2026](atomic-knowledge-validation-2026-10-29.md) | Validation record for document skills, source-free harnesses, Workload Identity Federation, IAM, and protected MCP checks. |
| [Model gateway](model-gateway.md) | How harness model requests route through Branchyard using per-turn tokens, weighted backends, fallbacks, budgets, and cost tracking. |
| [Egress policy](egress.md) | How network allowlists and tool permission presets restrict which hosts and tools a branch can access. |
| [Agent Substrate](substrate.md) | How Branchyard runs harnesses inside Agent Substrate actors using the bridge protocol, per-attempt credentials, and Git transfers. |
| [Desktop console design](superpowers/specs/2026-10-29-desktop-console-design.md) | Design spec for the three-pane desktop console, branch lanes, primary actions, and message composer behavior. |
| [Dispatch console design](superpowers/specs/2026-10-31-dispatch-console-design.md) | Design spec for the Decide, Watch, and Work views in the console, including `BranchInfo::awaiting` and `/decisions`. |
| [Console execution presets and source titles](superpowers/specs/2026-10-29-console-execution-presets-contract.md) | Design spec for operator-defined execution presets, side-effect-free readiness discovery, and Unicode source filenames. |
| [Skill delivery design](superpowers/specs/2026-10-29-skill-delivery-design.md) | Design spec for repository, workspace, user, and team skills, per-turn skill provenance, and packaging the document runtime. |
| [Sandbox snapshots](sandbox-snapshots.md) | How sandbox providers pause between turns, take checkpoint snapshots, and branch or rewind from snapshots with Git fallback. |
| [Observability](observability.md) | Metrics and traces on `tracing` and OpenTelemetry: `/metrics`, OTLP export from the standard `OTEL_*` variables, the span tree, and how turn trace context is passed to harnesses. |
| [Sync](sync.md) | Backing up and syncing tasks across Cloud Storage, S3, Azure Blob, Git remotes, or local directories with leases and client-side encryption. |
| [Environment recipes](recipes.md) | Defining custom VM or container lifecycle scripts in `branchyard.toml` and driving harnesses inside them over SSH. |
| [Toolchain images](images.md) | Sandbox images with harness CLIs pre-installed at pinned versions, labelled and digest-pinned, and how routing reads them. |
| [Harness lifecycle](harness-lifecycle.md) | Detecting installed harnesses, versions, logins, and quotas, and installing or updating agent CLIs under user policy. |
| [Code mode](code-mode.md) | Benchmarks from the multi-agent delegation suite and design notes for programmatic SDK delegation. |
| [Obligations](obligations.md) | What do my branches still owe (unknown or staged calls, approvals, lost turns, children, closing undo windows), and which command deals with each? |
| [Durability contract](durability-contract.md) | Which rules about turns, effects, ownership, signals and obligations does a durable agent runtime keep, which does Branchyard keep today, and which test shows each? |
| [Durability primitives design](superpowers/specs/2026-11-02-durability-primitives-design.md) | Why ideas from Tape and Durable Actors are built inside Branchyard rather than taken as a dependency, what was built (lost-turn effects, liveness, obligations) and what comes next, in order |
| [Skill registry](skill-registry.md) | Design, not built: one index over every skill source (built-in, repository, user, organisation, connector, memory), references as search documents, every ready skill mounted where the provider links the store, a server registry with a per-turn skills credential and a per-tenant cache, and a build plan the skill-discovery eval gates. |
| [Memory](memory.md) | Design, not built: memory as a reviewed skill first (team memory through git, personal memory in a local user source), then observations derived from event logs and deterministic consolidation with provenance, review by a person before anything is shared, and publication through the registry so agents find it by search. |
| [Anvil contract v2](anvil-contract-v2.md) | What Anvil added for remote tools (built in its PR #91, merged 5 November 2026; hermetic, not run live): a Firestore vault with KMS, disconnect, a search index, host install, a Cloud Run gateway, renewing tokens, `by_manifest` and connector icons, and the mock Branchyard tests against. |

## Roadmap and background

Project direction and comparisons with related tools.

| Page | What it covers |
|---|---|
| [Roadmap](roadmap.md) | Planned milestones, priorities, and the systems that informed each part of the design. |
| [Product strategy](strategy.md) | How Branchyard fits alongside Scion, OpenRig, Herdr, Warp, and Agent Substrate. |
| [Comparison](comparison.md) | Feature-by-feature comparison with Scion, OpenRig, Herdr, Warp, Straitjacket, and Coder, including vendored components and licenses. |
| [Cowork parity](cowork-parity.md) | Comparison with Claude Cowork capabilities and which milestones close remaining gaps. |
| [Competitive notes: emdash, Superset and Conductor](competitive-notes.md) | Notes on desktop agent workbenches (emdash, Superset, Conductor) and the ideas Branchyard adopted from them. |

## History

Archived plans and dogfooding records.

| Page | What it covers |
|---|---|
| [Implementation plan (history)](implementation-plan.md) | The original initial build plan (archived; see [Roadmap](roadmap.md) for current plans). |
| [Dogfood round 6](dogfood/round6/README.md) | Record of dogfooding round 6 (1 November 2026): building sandboxed HTML output previews across Claude Code, Codex, Antigravity, and GLM via `by fan` and `by run`, plus [feature 2](dogfood/round6/feature2-office-viewer.md). |
| [Dogfood round 6, feature 2](dogfood/round6/feature2-office-viewer.md) | Record of five real harnesses building the Office document viewer via `by fan` and `by run`, including judge scores and test results. |
| [Dogfood round 5](dogfood/round5/README.md) | Delegation battery results and notes from dogfooding round 5 ([checks](dogfood/round5/checks.md), [progress](dogfood/round5/progress.md)). |
| [Dogfood round 4](dogfood/round4/README.md) | Delegation battery results and notes from dogfooding round 4 ([checks](dogfood/round4/checks.md), [progress](dogfood/round4/progress.md)). |

`tests/test_docs_index.py` verifies that every Markdown file in `docs/` is linked from this page.
