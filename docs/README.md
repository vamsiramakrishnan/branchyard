# Documentation

| Read | What it answers |
|---|---|
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Implementation plan](implementation-plan.md) | What do we build next, in what order, and what proves each milestone works? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |

The architecture and driver documents are specifications. The current executable component is [branchyard-controls](../crates/branchyard-controls/src/lib.rs), a small Rust library of resume recipes. It is not yet the public task SDK or a server.
