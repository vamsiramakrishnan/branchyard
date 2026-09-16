# Documentation

| Read | What it answers |
|---|---|
| [SDK and packaging](sdk.md) | How do Rust callers and existing harnesses use the implemented SDK, CLI and skill/plugin? |
| [Control API](control-api.md) | What is the wire contract, and what must the future server enforce? |
| [Straitjacket review](straitjacket.md) | Which inspected mechanisms inform this SDK and roadmap? |
| [Architecture](design.md) | What does the SDK own, where do harnesses run, and how do topology, budgets, storage, recovery, and merging work? |
| [Harness integration](harness-integration.md) | How do we control sixteen harnesses through ACP and native interfaces? |
| [Implementation plan](implementation-plan.md) | What do we build next, in what order, and what proves each milestone works? |
| [Vendoring](vendoring.md) | Which controls have been copied, why, and how are they adapted and upgraded? |
| [Validation](validation.md) | What has actually passed, what failed upstream, and what remains untested? |
| [Third-party notices](../THIRD_PARTY.md) | Which revisions and licenses apply to the copied sources? |

The architecture and driver documents specify the future execution backend. The SDK, CLI and skill/plugin implement the caller contract. The [admission server](server.md) persists graph edits and dispatch in PostgreSQL/PGMQ; execution remains unimplemented. See [runtime qualification](runtime-qualification.md) for the next gate.
