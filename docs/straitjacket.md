# What Branchyard takes from Straitjacket

Reviewed 16 September 2026 at
[`b47e7af0861afe638b48444b04e5829f9c21a842`](https://github.com/vamsiramakrishnan/straitjacket/tree/b47e7af0861afe638b48444b04e5829f9c21a842).
The repository's first-party license is Apache-2.0. This change reuses design
lessons; it does not copy its implementation or add Straitjacket as a runtime
dependency. Existing Branchyard vendor pins remain unchanged.

## Mechanisms to keep

| Source inspected | Finding | Branchyard application |
|---|---|---|
| [`src/ctx/task_runtime.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/src/ctx/task_runtime.py) and [ADR 007](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/spec/adr/007-durable-task-execution.md) | The runtime owns durable reservations, outcomes and checkpoints; controllers select operations. Child scopes share the allowance. | Keep domain mechanics out of the meta-harness policy. Persist request identity before submission; preserve unknown outcomes. Server reservations are a later gate. |
| [`src/ctx/plan_ops.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/src/ctx/plan_ops.py) | Registered operations carry capability and effect metadata and wrap shared engines. | Typed command variants and one protocol crate under SDK/CLI/skill; future server handlers and tools reuse the same types. |
| [`src/ctx/hosts.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/src/ctx/hosts.py) and [host capabilities](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/docs/HOST-CAPABILITIES.md) | Detecting an executable is distinct from being able to control it. Hook capabilities differ by host. | Registered, qualified and effective capabilities stay distinct. A protocol name or installed binary cannot authorize a profile. |
| [ADR 004](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/spec/adr/004-plugin-contains-skill.md), [`src/ctx/installer.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/src/ctx/installer.py) | The plugin contains the skill; standalone delivery must share its source. | One canonical skill, two manifests, generated standalone archive, explicit installer that refuses replacement. |
| [`scripts/check_distribution.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/scripts/check_distribution.py) | An editable/source install can hide missing release inputs. | Extract archives outside the checkout and run the shipped installer and launcher; compare canonical skill bytes and file hashes. |
| [`src/ctx/mcp.py`](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/src/ctx/mcp.py) | Tool delivery is bounded and delegates to existing operations. | Bound requests and responses, return artifact identities, disclose schema when needed. Future MCP uses the SDK rather than another execution engine. |
| [ADR 006](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/spec/adr/006-acp-orchestration-transport.md) | ACP controls workers; native hooks and MCP serve other boundaries. Fixtures are separate from live compatibility. | Keep inbound SDK/CLI tools separate from outbound ACP/native drivers. State exactly what fixture tests establish. |
| [ADR 008](https://github.com/vamsiramakrishnan/straitjacket/blob/b47e7af0861afe638b48444b04e5829f9c21a842/spec/adr/008-cross-harness-relay.md) | Messages carry addresses and bounded notes; delivery receipts name the actual host boundary. | Bounded event summaries with artifact IDs now; future delivery needs explicit receipts and host-specific interruption semantics. |

## Boundaries that change on a server

Straitjacket's local `TaskRuntime` uses a POSIX coordinator lock and deliberately
does not advertise parallel dispatch within one task. Its ledger, worktrees and
advisory relay are not a distributed scheduler or tenant isolation boundary.
Branchyard requires database transactions, queue reconciliation, attempt fencing
and server-enforced resource policy before those guarantees can be claimed.
A shared journal format alone cannot give existing orchestration stronger semantics.

The relay's optional hook delivery can degrade to silence. Branchyard's admission
and cancellation records cannot: persistence failure must prevent dispatch.
A native permission hook is also not a replacement for sandbox network/storage
isolation. Keep infrastructure, agent session and host UI capabilities independent.

Straitjacket's ACP implementation is useful research, but Branchyard should use
the existing Rust ACP SDK instead of porting a new JSON-RPC transport. Similarly,
a later MCP facade should use the maintained Rust MCP SDK. The current plugin
uses the CLI and needs neither a custom MCP server nor host interception hooks.

Its evidence engine may later be an optional tool available inside a task's
sandbox, with artifacts imported through a typed adapter. Do not copy the whole
retrieval engine, local scheduler, proxy or model loop into Branchyard to acquire
its packaging pattern.

## Roadmap consequence

Deliver the caller contract and portable skill first. Prove durable admission and
operation reconciliation next. Qualify one sandbox runtime and one harness driver
before claiming execution. Only then connect runtime graph changes to recursive
spawn, shared resource fencing and validated integration. Native host hooks and a
larger tool catalog are justified only by a missing capability with a concrete
acceptance test. See [the implementation plan](implementation-plan.md).
