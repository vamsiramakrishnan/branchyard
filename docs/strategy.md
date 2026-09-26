# Product strategy

Branchyard runs *work*: it decides nothing about how agents think, and everything about how their work is delegated, isolated, bounded and merged. Runtimes such as [Agent Substrate](substrate.md) run *agents*. Terminal tools such as Herdr, Warp and OpenRig present agents on one machine. Branchyard should be the layer every one of those surfaces can sit on.

## Positioning

| | Owns | Leaves to others |
|---|---|---|
| Agent Substrate | Density, suspend/resume, routing, isolation at cluster scale | Harness semantics, delegation, merging |
| Herdr | Watching many terminal agents at once | Structured state (it scrapes screens), remote execution |
| OpenRig | Declarative rigs of agent seats, startup projection | Runtime-grown topology, per-invocation permissions |
| Warp | A polished terminal experience around agents | An embeddable, headless, permissively licensed contract |
| **Branchyard** | The contract: typed harness events, permission answers, budgets, and branch/merge semantics | Virtualization, terminals, model loops |

Substrate is a backend, not a rival: `branchyard-substrate` already maps its actors onto Branchyard's provider contract.

## The developer model: branches

Developers already understand git. Branchyard uses its vocabulary for agent work:

| Concept | Meaning |
|---|---|
| **Task** | What the developer asked for, with its budget and policy |
| **Branch** | One agent working in its own git worktree with its own harness session. A child task is a branch. |
| **Fork** | A new branch from another branch's latest candidate, with the conversation forked where the harness supports it |
| **Candidate** | The exact commit a branch proposes |
| **Merge** | Promotion of a candidate only after checks pass against the exact target revision |

Five concepts, versus Substrate's actor, template, atespace, worker pool, sandbox config, tag and worker.

## Local first, same semantics at scale

`design.md` keeps harness execution on servers. Local mode does not relax that: the same engine runs in-process against a local process provider, with identical task, branch, event and merge semantics. Moving to Microsandbox or Substrate changes the provider, not the program. Local mode offers no isolation beyond the operating system user, and says so.

## Measurable targets

| Metric | Target |
|---|---|
| Time from install to a first multi-harness run | Under 2 minutes, with no infrastructure |
| Code to fan one task out to two harnesses and merge the winner | About 15 lines |
| Cost to add a harness driver | Under a day with the conformance kit and qualification runner |
| Human approvals for in-policy work | Zero; out-of-policy requests are denied or asked, never bypassed |
| Evidence behind each support claim | A published qualification report per harness version |

## Principles

1. **One contract, many surfaces.** CLI, TUI, web, IDE and harness tools are clients of the same typed events and operations.
2. **Unattended but bounded.** Policies answer permissions per invocation, budgets stop runaway work, and nothing is ever run with a permission bypass.
3. **Trust is published.** Compatibility claims come from qualification reports, and any recorded session can become a regression test.
4. **Extension points stay small.** Harness drivers, sandbox providers and policies each have a narrow contract and a conformance suite anyone can run.
