# Documentation

| Read | What it answers |
|---|---|
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Implementation plan](implementation-plan.md) | What do we build next, in what order, and what proves each milestone works? |
| [Delegation](delegation.md) | How does a harness create and coordinate child branches with `by`, Python, Rust or MCP, within what envelope and authority? |
| [Sandbox providers](providers.md) | What is the provider contract, what does the local provider guarantee, and how do I run harnesses in Microsandbox microVMs and its KVM tests? |
| [Agent Substrate](substrate.md) | How does Branchyard use Agent Substrate as a sandbox provider, and what does it not provide? |
| [Server and remote mode](server.md) | How do I run `by serve`, call its HTTP API, and drive it with `by --remote`? What is durable, and what is not isolated? |
| [Driver qualification](qualification/README.md) | Which driver profiles passed live protocol qualification, and what did it find? |
| [Compatibility](compatibility.md) | Which profile drives each harness, with which capabilities, and has it passed live qualification? |
| [Writing a driver](writing-a-driver.md) | How do I add a harness profile or driver, test it against a recorded transcript, and qualify it? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Live testing](testing-live.md) | What do I run on a machine with real harnesses, credentials and KVM, what should happen, and where do I record it? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |

The architecture and driver documents are specifications. The current executable components are [branchyard-harness](../crates/branchyard-harness/src/lib.rs), protocol drivers for Claude Code, Codex and ACP agents; [branchyard-controls](../crates/branchyard-controls/src/lib.rs), resume recipes and the harness identity registry; [branchyard-sandbox](../crates/branchyard-sandbox/src/lib.rs), the provider contract and capability admission; [branchyard-microsandbox](../crates/branchyard-microsandbox/src/lib.rs), an unqualified Microsandbox provider behind a cargo feature; and [branchyard-substrate](../crates/branchyard-substrate/src/lib.rs), an unqualified Agent Substrate adapter. None is yet the public task SDK or a server.
