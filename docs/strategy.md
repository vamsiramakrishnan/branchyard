# Product strategy

Branchyard runs *work*: it decides nothing about how agents think, and everything about how their work is delegated, isolated, bounded and merged. Runtimes such as [Agent Substrate](substrate.md) run *agents*. Scion and OpenRig orchestrate teams of agents, and Herdr and Warp present them in a terminal. Scion, OpenRig and Herdr drive most harnesses through their interactive terminal UIs. Branchyard drives them through their machine protocols, and should be the layer those surfaces can sit on.

The table below summarizes [the comparison](comparison.md), which cites each upstream claim at a pinned commit and sets out what to absorb.

## Positioning

| | Owns | Leaves to others |
|---|---|---|
| Agent Substrate | Density, suspend/resume, routing, isolation at cluster scale | Harness semantics, delegation, merging |
| Scion | Teams of agents in containers, locally or through a Hub on Kubernetes or Cloud Run; provisioning, identity, messaging, chat bridges | Per-invocation approvals (it launches harnesses with permission bypass), cost budgets, validated merging |
| OpenRig | Declarative rigs of seats, startup delivery, honest restore, runtime grow and shrink, queues and workflows | Per-invocation approvals (its policies set a launch posture), budgets, sandboxing, merging |
| Herdr | Watching and driving many terminal agents; hooks for some agents, screen manifests for the rest; resume after restart | Structured control of Claude Code and Codex, approvals, budgets, isolation, merging |
| Warp | A polished terminal experience around agents | An embeddable, headless, permissively licensed contract |
| **Branchyard** | The contract: typed harness events, permission answers, budgets, and branch/merge semantics | Virtualization, terminals, model loops, team conventions |

Substrate is a backend, not a rival: `branchyard-substrate` already maps its actors onto Branchyard's provider contract. Scion's harness provisioning is being ported; OpenRig's declarative rigs and Herdr's terminal view are the next candidates, as an idea and as a client respectively ([absorption plan](comparison.md#absorption-plan)).

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
