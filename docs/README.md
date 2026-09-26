# Documentation

| Read | What it answers |
|---|---|
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Implementation plan](implementation-plan.md) | What do we build next, in what order, and what proves each milestone works? |
| [Agent Substrate](substrate.md) | How does Branchyard use Agent Substrate as a sandbox provider, and what does it not provide? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |

The architecture and driver documents are specifications. The current executable components are [branchyard-controls](../crates/branchyard-controls/src/lib.rs), a small Rust library of resume recipes; [branchyard-sandbox](../crates/branchyard-sandbox/src/lib.rs), provider capability admission; and [branchyard-substrate](../crates/branchyard-substrate/src/lib.rs), an unqualified Agent Substrate adapter. None is yet the public task SDK or a server.
