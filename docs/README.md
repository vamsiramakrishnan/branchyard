# Documentation

New here: [getting started](getting-started.md), then [concepts](concepts.md). Looking for what works today: [status](status.md). Every page in this directory is listed below, under the shelf that fits what you want to do.

## Get started

Read these in order the first time.

| Read | What it answers |
|---|---|
| [Getting started](getting-started.md) | What does the first hour look like: install `by`, run `by setup`, delegate a first task, read its diff, merge it, and find the next page? |
| [Setup](setup.md) | How do I set Branchyard up with `by setup` or by interview, in a terminal wizard or through my coding harness, and what do `branchyard.toml` and `by init`'s protocol look like? |
| [Doctor](doctor.md) | Is this machine and repository ready, which of the repository, configuration, harnesses, provider, host, server and model gateway needs attention, what do `ok`, `warn` and `fail` and the exit codes mean, and what does `by doctor` read and never change? |
| [Up](up.md) | How does one command bring a repository's configured backends up, installing the harnesses the configuration names where my user file allows, starting the connector gateway and telling me what only I can do, and how do the Rust and Python SDKs run the same operation? |
| [Concepts](concepts.md) | What do branch, task, run, fan, map, spawn, attempt, candidate, checkpoint, turn, provider and profile mean, and what does each branch status tell me to do next? |
| [Modes](modes.md) | Where can a harness run (local, isolated, sandbox, server, PostgreSQL), what does each give and cost, and which one setting and `by doctor` line confirms it? |
| [Distribution](distribution.md) | How do I install `by`, and Branchyard's skills and hooks into each harness, and how are plugin, skill and SDK archives built? |

## Guides

How to do one thing.

| Read | What it answers |
|---|---|
| [Cooperative teams](cooperative-teams.md) | How do I coordinate native code and knowledge workers, validate results, and relay frozen handoffs? |
| [Knowledge and design skills](knowledge-skills.md) | How do I discover skills progressively, capture evidence, analyze tables and check designed artifacts using established libraries? |
| [Watching branches](watch.md) | How do I watch every branch live with `by watch`, act on one with a key, and get told when a branch needs me or ends? |
| [Checkpoints, rewind, try and compare](checkpoints.md) | How do I fork or rewind a branch to any turn, try a branch in my own checkout and take it back out exactly, and compare attempts and pick one? |
| [Pull requests](pull-requests.md) | How do I start a branch from a GitHub, Linear, Jira or GitLab issue or a pull request's head, push it as a pull request, feed CI failures and reviews back into it, and see whether it can merge? |
| [Workspace lifecycle](workspace.md) | How does each new branch's worktree get its `.env`, its install and its own port before the first turn, what cleans up after it, and why does a repository's script never run until I trust it? |
| [Prepared environments](environments.md) | How does setup run once per lockfile and every new branch start from its result, cloned, linked or branched from a sandbox snapshot, with the last good build when one fails, and how do `by env` and `.worktreeinclude` work? |
| [Warm pools](pools.md) | How do ready worktrees with the prepared environment in place make a new branch start sooner, and what refills them? |
| [Fleet: routing, judging and failover](fleet.md) | How does `by run` pick a harness, model and effort for each kind of task and learn from outcomes, how does `by judge` score a fan's attempts and propose one, and when does a failed harness fail over to the next? |
| [Plans and goals](plans-and-goals.md) | How does a branch plan read-only and wait for my approval, edits or rejection (or its parent's), and how does a goal get verified by a judge, with follow-up turns until it is met? |
| [Repository knowledge](knowledge.md) | How does Branchyard propose rules from how branches went, how do I review and adopt them, and how are adopted ones given to each matching harness within a budget and traced? |
| [Delegation](delegation.md) | How does a harness create and coordinate child branches with `by`, Python, Rust or MCP, within what envelope and authority? |
| [Rigs](rigs.md) | How do I declare a team of harnesses in a file, check it, and run it with `by rig`, and what is refused? |
| [Wide map](map.md) | How do I run one prompt over every item of a list, each on its own branch, with every answer checked against a schema and collected into a table? |
| [Triggers and schedules](triggers.md) | How does a task start on a cron schedule, at an interval, or from a signed GitHub, Slack, Linear or generic webhook, with conditions, a precheck, a test run and a pause after repeated failures, and why does nothing fire twice? |
| [Connectors](connectors.md) | How does a branch use GitHub, Slack or an internal API through a skill, an SDK and one gateway, without holding a credential? |
| [Usage and adopting sessions](usage.md) | How much of each Claude Code and Codex login's 5-hour and weekly limits is used, how do `by run` and the router avoid a login near its limit, and how do I turn a session already on this machine into a branch? |
| [Web companion](companion.md) | How do I watch and steer branches from a phone with `by serve --app`, pair it with a link and a QR code, get push notifications, and why is it safe? |
| [Harness hooks](harness-hooks.md) | How does a session a person started in any harness hear what Branchyard's branches did, through its hooks and status line? |
| [Workspace management](workspace-management.md) | How does an external agent manage code and knowledge work through a separately authenticated MCP client, and how does the server reuse checked PR publication? |
| [SDKs](sdk.md) | Which SDK does a harness, a Slack or web service, or a Rust program use, how does a third-party surface keep one branch per conversation and stream its replies, and how does it give, inspect and stay within connector grants? |
| [Deploying `by serve`](deploy.md) | How do I run the server in a container or behind PostgreSQL with compose, and check a host is ready to? |
| [Remote over ssh](remote-ssh.md) | How do I run commands against a repository on another machine with `by --remote ssh://…` and no server setup? |
| [Task repositories](task-repos.md) | How is every task a git repository, including one on a folder that is not a repository, with the conversation committed beside the files and rewound with them? |
| [Troubleshooting](troubleshooting.md) | My branch is stuck, failed, interrupted or waiting: what does its state mean, and which command do I run next? |
| [Upgrade](upgrade.md) | How do I upgrade `by`, the store, the plugins and the harness CLIs, and what cannot be undone? |
| [Adding a harness](adding-a-harness.md) | How do I add a harness Branchyard does not drive yet, from its install entry to a qualified profile? |
| [Writing a driver](writing-a-driver.md) | How do I add a harness profile or driver, test it against a recorded transcript, and qualify it? |

## Reference

What exactly is true, and what has run.

| Read | What it answers |
|---|---|
| [Status](status.md) | What is built, what has run against real harnesses and what has not, with dates, one vocabulary and a generated table of every harness driver? |
| [Surfaces](surfaces.md) | Which operations and options work in the SDK, `by`, `by --remote`, the HTTP API, the Rust client and delegation, and which are refused where? |
| [Server and remote mode](server.md) | How do I run `by serve`, call its HTTP API, and drive it with `by --remote`? What is durable, and what is not isolated? |
| [Command line](cli.md) | What do the global options and their variables, short forms, durations, shell completions and the man page do, and where is each command's own flag list? |
| [Command reference](cli-reference.md) | Every command and subcommand with its usage and options, generated from `by`'s own definitions? |
| [Sandbox providers](providers.md) | What is the provider contract, what does the local provider guarantee, and how do I run harnesses in Microsandbox microVMs or Agent Substrate actors? |
| [Compatibility](compatibility.md) | Which profile drives each harness, with which capabilities, and has it passed live qualification? |
| [Driver qualification](qualification/README.md) | Which driver profiles passed live protocol qualification, and what did it find? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Substrate on GKE](substrate-gke.md) | Plan, provision or adopt a dedicated GKE cluster using pinned upstream tools |
| [Compute on GCE](compute-gce.md) | Create a dedicated agy worker and connect through native SSH |
| [Substrate setup](substrate-setup.md) | Save a connection, prepare a template, and verify a disposable actor |
| [Remote validation, 28 October 2026](remote-validation-2026-10-28.md) | What ran on remote compute, which execution backends passed, and what setup friction did a new developer encounter? |
| [Live testing](testing-live.md) | What do I run on a machine with real harnesses, credentials and KVM, what should happen, and where do I record it? |
| [CI gates](ci-gates.md) | Which checks keep dependencies, licenses, docs, features, unused code, Python and test coverage from decaying, what fails them, and how do I run each locally? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Contributing](../CONTRIBUTING.md) | How do I build, test and check a change before I push it, and which boundaries and ratchets apply? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |
| [Adding a harness](adding-a-harness.md) | Which eight files and tables do I touch to add a harness, from its catalog entry to its hooks and qualification report, and which test tells me what I missed? |

## Design and internals

Why it is built this way. Some pages describe a target rather than what exists; [status](status.md) says which.

| Read | What it answers |
|---|---|
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Durable execution](durability.md) | What survives a crash, how leases, journaled steps, cancellation and recovery work, what is never replayed, and how the store maps onto PostgreSQL? |
| [Branch lifecycle](lifecycle.md) | How are stalled turns detected, how does an operator get webhook notifications, and how does a branch change harness or model by reincarnation? |
| [Task graphs](graph.md) | How do child branches depend on one another, and how does a branch change its graph of children in one atomic step? |
| [Effects, approvals and undo](effects.md) | How is every connector call that changes the world written to a ledger before it is made, decided by policy, staged, reconciled after a crash and undone where the upstream allows? |
| [Shared storage](storage.md) | How do branches share immutable artifacts and single-writer scratch areas? |
| [Ambient registry](registry.md) | How do the services Branchyard starts or uses announce themselves with capabilities and a lease, get found by what they can do and get reclaimed when their owner stops? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Provisioning](provisioning.md) | How is a harness's home prepared before each turn: secrets, MCP servers, instructions, model, effort and telemetry, translated from Scion's provisioners, and what is not ported? |
| [Artifact toolkit](artifact-toolkit.md) | How do atomic document skills create, revise, render and validate DOCX, XLSX, PDF and PPTX files, and assemble independent slide sections? |
| [Artifact capture](artifact-capture.md) | How are declared output files collected after a producing turn and published before accepting its deliverable? |
| [Native identity](identity.md) | How do Google ADC, verified server callers and connector accounts preserve native IAM and upstream permissions? |
| [Atomic knowledge validation, 29 October 2026](atomic-knowledge-validation-2026-10-29.md) | Which document, source-free harness, WIF, IAM and protected MCP checks actually ran, and what setup friction did they expose? |
| [Model gateway](model-gateway.md) | How does a harness's model call go through Branchyard on the turn's token, with the key held back, weighted backends, fallbacks, budgets and exact cost? |
| [Egress policy](egress.md) | Which hosts may a branch reach, how is that enforced, and which permission presets name common tool policies? |
| [Agent Substrate](substrate.md) | How does Branchyard run harnesses in Agent Substrate actors: the bridge and its protocol, per-attempt credentials, git transfer, and what is still unqualified? |
| [Sandbox snapshots](sandbox-snapshots.md) | How does a branch keep its sandbox paused between turns, take a provider snapshot with each checkpoint, and fork, rewind, delegate and fan from it, with git and setup as the fallback? |
| [Observability](observability.md) | What metrics, traces and summaries does a server give about itself, and how does a turn's trace reach its harness? |
| [Sync](sync.md) | How are tasks copied to Cloud Storage, S3, Azure Blob, a git remote or a directory, with one manifest per task, conflict branches, leases and client-side encryption? |
| [Environment recipes](recipes.md) | How does a repository say how to make a machine to work on, a VM or a container, and how does a branch run its harness there over ssh? |
| [Harness lifecycle](harness-lifecycle.md) | Which harnesses does each machine have, at what version, logged in and with what quota, and how are they installed, updated and logged in to under a policy, advertised by workers and used by the router? |
| [Code mode](code-mode.md) | What did the real-harness delegation battery measure, and what would an SDK that a harness programs change? (design) |

## Roadmap and background

Where it is going, and what it learned from.

| Read | What it answers |
|---|---|
| [Roadmap](roadmap.md) | What comes next, in which order, and what each piece is learned from? |
| [Product strategy](strategy.md) | What does Branchyard do that Scion, OpenRig, Herdr, Warp and Agent Substrate do not, and where does it sit among them? |
| [Comparison](comparison.md) | How do Scion, OpenRig, Herdr, Warp, Straitjacket and Coder compare with Branchyard, feature by feature with cited sources, which of their sources are vendored under which licences, and what should Branchyard absorb from each? |
| [Cowork parity](cowork-parity.md) | Which Claude Cowork capabilities does Branchyard have today, which are gaps, and in which wave does each close? |
| [Competitive notes: emdash, Superset and Conductor](competitive-notes.md) | What do the desktop workbenches do well in a first five minutes of use, what do they struggle with, and what has Branchyard taken from them? |

## History

Kept for the record; not current guidance.

| Read | What it answers |
|---|---|
| [Implementation plan (history)](implementation-plan.md) | What did the first plan say to build, in what order? It is history; the live plan is the [roadmap](roadmap.md). |
| [Dogfood round 5](dogfood/round5/README.md) | How did Branchyard score as a delegation substrate on the real-harness battery after round 5 ([checks](dogfood/round5/checks.md), [progress](dogfood/round5/progress.md))? |
| [Dogfood round 4](dogfood/round4/README.md) | How did Branchyard score on the same battery after round 4 ([checks](dogfood/round4/checks.md), [progress](dogfood/round4/progress.md))? |

`tests/test_docs_index.py` fails when a page in this directory is not linked from here.
