# Documentation

| Read | What it answers |
|---|---|
| [Setup](setup.md) | How do I set Branchyard up by interview, in a terminal wizard or through my coding harness, and what do `branchyard.toml` and `by init`'s protocol look like? |
| [Workspace lifecycle](workspace.md) | How does each new branch's worktree get its `.env`, its install and its own port before the first turn, what cleans up after it, and why does a repository's script never run until I trust it? |
| [Prepared environments](environments.md) | How does setup run once per lockfile and every new branch start from its result, cloned, linked or branched from a sandbox snapshot, with the last good build when one fails, and how do `by env` and `.worktreeinclude` work? |
| [Roadmap](roadmap.md) | What comes next, in which order, and what each piece is learned from? |
| [Connectors](connectors.md) | How does a branch use GitHub, Slack or an internal API through a skill, an SDK and one gateway, without holding a credential? |
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Implementation plan](implementation-plan.md) | What do we build next, in what order, and what proves each milestone works? |
| [Delegation](delegation.md) | How does a harness create and coordinate child branches with `by`, Python, Rust or MCP, within what envelope and authority? |
| [Rigs](rigs.md) | How do I declare a team of harnesses in a file, check it, and run it with `by rig`, and what is refused? |
| [Provisioning](provisioning.md) | How is a harness's home prepared before each turn: secrets, MCP servers, instructions, model, effort and telemetry, translated from Scion's provisioners, and what is not ported? |
| [Sandbox providers](providers.md) | What is the provider contract, what does the local provider guarantee, and how do I run harnesses in Microsandbox microVMs or Agent Substrate actors? |
| [Sandbox snapshots](sandbox-snapshots.md) | How does a branch keep its sandbox paused between turns, take a provider snapshot with each checkpoint, and fork, rewind, delegate and fan from it, with git and setup as the fallback? |
| [Agent Substrate](substrate.md) | How does Branchyard run harnesses in Agent Substrate actors: the bridge and its protocol, per-attempt credentials, git transfer, and what is still unqualified? |
| [Fleet: routing, judging and failover](fleet.md) | How does `by run` pick a harness, model and effort for each kind of task and learn from outcomes, how does `by judge` score a fan's attempts and propose one, and when does a failed harness fail over to the next? |
| [Checkpoints, rewind, try and compare](checkpoints.md) | How do I fork or rewind a branch to any turn, try a branch in my own checkout and take it back out exactly, and compare attempts and pick one? |
| [Durable execution](durability.md) | What survives a crash, how leases, journaled steps, cancellation and recovery work, what is never replayed, and how the store maps onto PostgreSQL? |
| [Pull requests](pull-requests.md) | How do I start a branch from a GitHub, Linear, Jira or GitLab issue or a pull request's head, push it as a pull request, feed CI failures and reviews back into it, and see whether it can merge? |
| [Usage and adopting sessions](usage.md) | How much of each Claude Code and Codex login's 5-hour and weekly limits is used, how do `by run` and the router avoid a login near its limit, and how do I turn a session already on this machine into a branch? |
| [Surfaces](surfaces.md) | Which operations and options work in the SDK, `by`, `by --remote`, the HTTP API, the Rust client and delegation, and which are refused where? |
| [Triggers and schedules](triggers.md) | How does a task start on a cron schedule, at an interval, or from a signed GitHub, Slack, Linear or generic webhook, with conditions, a precheck, a test run and a pause after repeated failures, and why does nothing fire twice? |
| [Server and remote mode](server.md) | How do I run `by serve`, call its HTTP API, and drive it with `by --remote`? What is durable, and what is not isolated? |
| [Distribution](distribution.md) | How do I install the Branchyard skill for Claude Code or Codex, or build reproducible plugin, skill and SDK archives? |
| [Deploying `by serve`](deploy.md) | How do I run the server in a container or behind PostgreSQL with compose, and check a host is ready to? |
| [Driver qualification](qualification/README.md) | Which driver profiles passed live protocol qualification, and what did it find? |
| [Compatibility](compatibility.md) | Which profile drives each harness, with which capabilities, and has it passed live qualification? |
| [Writing a driver](writing-a-driver.md) | How do I add a harness profile or driver, test it against a recorded transcript, and qualify it? |
| [Comparison](comparison.md) | How do Scion, OpenRig and Herdr compare with Branchyard, feature by feature with cited sources, and what should Branchyard absorb from each? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Live testing](testing-live.md) | What do I run on a machine with real harnesses, credentials and KVM, what should happen, and where do I record it? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |

The architecture and driver documents are specifications. The current executable components are [branchyard-harness](../crates/branchyard-harness/src/lib.rs), protocol drivers for Claude Code, Codex and ACP agents; [branchyard-controls](../crates/branchyard-controls/src/lib.rs), the harness identity registry and Herdr's official-agent-source check; [branchyard-sandbox](../crates/branchyard-sandbox/src/lib.rs), the provider contract and capability admission; [branchyard-microsandbox](../crates/branchyard-microsandbox/src/lib.rs), an unqualified Microsandbox provider behind a cargo feature; [branchyard-substrate](../crates/branchyard-substrate/src/lib.rs), an unqualified Agent Substrate provider; and [branchyard-bridge](../crates/branchyard-bridge/src/lib.rs), the in-sandbox exec bridge it uses. None is yet the public task SDK or a server.
