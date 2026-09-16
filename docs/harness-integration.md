# Harness integration

Research baseline: 16 September 2026. This document specifies drivers to implement. None of the sixteen profiles below has passed Branchyard runtime qualification. A documented upstream interface is evidence of an integration path, not a claim of tested compatibility.

## Decision

Build one ACP client and a small set of native protocol drivers. Keep the harness process, protocol connection, sandbox allocation, conversation, and durable task separate. The same harness can have multiple profiles; profile identity includes its protocol and pinned executable version.

Use the existing [Rust ACP SDK](https://docs.rs/agent-client-protocol/latest/agent_client_protocol/) for ACP framing, requests, callbacks, and negotiation. Its transport abstractions can carry bytes from the sandbox's execution stream. Drive the connection continuously; a server deployment does not require an editor. Pin the SDK and protocol version together. Experimental session or protocol features are separate capabilities.

| Boundary | Interface | Authority |
|---|---|---|
| Application → Branchyard | Rust SDK over HTTPS commands and SSE | Authenticated user or delegated run |
| Branchyard server → node | Versioned RPC over mTLS | Allocation lease and attempt generation |
| Node → harness | ACP or qualified native protocol | Specific sandbox, session, and permission policy |
| Harness → Branchyard tools | MCP or thin CLI | Attenuated run identity; same domain API |
| Harness → other systems | Registered tools and allowed network routes | Explicit tool/service credentials |

MCP offers tools to a harness. ACP controls an agent session. Neither defines Branchyard's scheduling, durable graph mutations, root budgets, or Git promotion protocol. A2A may later expose remote peer agents, but is not required to supervise these local-to-the-server processes.

## Sixteen initial integration targets

Commands describe upstream entry points, not ready-to-run deployment recipes. Authentication, executable versions, images, and required capabilities must be qualified before activation.

| Harness | Preferred profile | Documented entry point / integration | Qualification focus |
|---|---|---|---|
| Claude Code | Existing ACP adapter; native SDK helper for deeper controls | [Claude Agent ACP](https://github.com/agentclientprotocol/claude-agent-acp), built on the [Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview) | Permission callbacks, hooks, explicit resume/fork, helper and CLI version pairing |
| Codex | Native App Server | [`codex app-server`](https://developers.openai.com/codex/app-server/); existing [Codex ACP adapter](https://github.com/zed-industries/codex-acp) as an alternate profile | Bidirectional approvals, thread/turn IDs, cancellation, reconnect |
| Antigravity | Native NDJSON or official SDK | [`agy --input-format stream-json --output-format stream-json`](https://antigravity.google/docs/cli/headless/) | `event`-based frames, cumulative usage, cached authentication, headless permission behavior |
| Oh My Pi | ACP; native RPC when required | [`omp acp` / `omp --mode rpc`](https://github.com/can1357/oh-my-pi) | RPC completion versus command acknowledgment; extension interaction; resume syntax |
| DeepSeek Harness | ACP | [`dsh --profile acp`](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/bundle/acp-app/README.md) | Trusted profile composition, stdout purity, session persistence, startup-only configuration |
| Gemini CLI | ACP | [`gemini --experimental-acp`](https://geminicli.com/docs/cli/cli-reference/) | Experimental mode/version pairing, auth and policy configuration |
| OpenCode | ACP | [`opencode acp`](https://opencode.ai/docs/acp/) | Optional methods, cwd, MCP, cancellation; keep native server API as a separate profile |
| Pi | Native RPC | [`pi --mode rpc`](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md) | Prompt acknowledgments versus agent completion; confined session-file references |
| Goose | ACP | [`goose acp`](https://goose-docs.ai/docs/gdk/acp/) | Stdout transport, provider identity, permission and tool callbacks |
| Aider | Batch process | [Scripted `--message` execution](https://aider.chat/docs/scripting.html) | Exit/result mapping and artifact validation; no assumed persistent RPC or native fork |
| Cursor CLI | ACP | [`agent acp`](https://cursor.com/docs/cli/acp) | Authentication and executable aliases; permission handling in unattended operation |
| GitHub Copilot CLI | ACP | [`copilot --acp`](https://github.blog/changelog/2026-01-28-acp-support-in-copilot-cli-is-now-in-public-preview/) | Protocol/version qualification; SDK backend is a distinct alternate interface |
| Amp | Native NDJSON | [`amp --execute --stream-json`](https://ampcode.com/docs/cli/streaming-json), with documented stream input mode when needed | Keep execution in our sandbox; turn boundaries and server-compatible credentials |
| Qwen Code | ACP | [`qwen --acp`](https://qwenlm.github.io/qwen-code-docs/en/users/integration-zed/) | ACP package version, scope projection, approvals, optional session methods |
| Kimi CLI | ACP | [`kimi acp`](https://github.com/MoonshotAI/kimi-cli) | Pin the legacy CLI or separately qualify its successor; do not silently migrate saved sessions |
| Hermes | ACP | [`hermes acp`](https://hermes-agent.nousresearch.com/docs/user-guide/features/acp) | Install ACP extra; isolate persistent home; qualify resume and approval scopes |

This roster does not imply sixteen custom schedulers. Profiles share the task lifecycle and sandbox provider, while drivers translate only the relevant wire contract. Terminal automation is a separate, lower-confidence compatibility mode, not the default for a protocol-capable harness.

## Native interfaces that need specific treatment

### Codex

App Server exposes initialization, thread lifecycle, turn control, notifications, and requests that the client must answer. Keep native thread and turn IDs in the session record. Schema generation and parsing must match the installed binary. Do not assume the ACP and App Server envelopes are identical. The client must service approval requests while awaiting a turn; blocking that path deadlocks progress. [App Server contract](https://developers.openai.com/codex/app-server/)

Branchyard should store the last observed native turn and its outcome before allowing another mutating prompt after reconnect. Resuming a thread is not proof that an uncertain previous turn did no work. Use a fresh attempt only after reconciling the prior attempt's authority and effects.

### Claude Code

Reuse the maintained ACP adapter for the common profile. For SDK-specific hooks, permissions, and richer session control, use a small helper built on the official Python or TypeScript SDK. Keep that helper inside the execution boundary and pin it with the CLI. The Rust worker owns its lifecycle; Rust need not reimplement the SDK's private subprocess protocol. [Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview)

Structured print mode remains useful for bounded batch runs, but its existence does not establish a complete bidirectional control contract. Choose the driver according to required capabilities. Do not accidentally disable required hooks or Branchyard tools when selecting a reduced-discovery launch mode. [Headless operation](https://code.claude.com/docs/en/headless)

### Antigravity

The documented streaming CLI accepts NDJSON user events and emits a result for each turn. Wait for that result before submitting the next prompt. Use the exact conversation ID for continuation. Its schema uses `event`; Claude-style control request/response frames are not supported. Usage fields can be cumulative, so billing observations require deltas rather than summing every result. Headless authentication must be provisioned before launch. [Headless contract](https://antigravity.google/docs/cli/headless/)

Branchyard must record which policy controls this mode actually supports. If a task requires per-tool decisions that the selected mode cannot expose, reject that profile for the task or use a separately qualified SDK profile. Streaming JSON alone is not a permission boundary.

### Pi and Oh My Pi

Treat these as separate versioned profiles. Both expose machine interfaces, but their resume syntax and extensions differ. Herdr's selected recipes use `pi --session <value>` and `omp --resume=<value>`. Those are resume recipes, not proof of protocol startup flags for every version. [Vendored recipes](../vendor/herdr/src/agent_resume.rs)

RPC commands can be acknowledged before the agent finishes. Track the documented terminal event rather than marking success on the request response. If a mode emits interactive extension requests, the profile must declare and implement their responses; otherwise fail explicitly. Never let a session file path choose a host file outside the sandbox's authorized store.

### DeepSeek Harness

DSH has separate ACP and SDK profiles. Select the shipped ACP composition first. Its profile startup, persistence, and shutdown contract is already documented; avoid building another launcher that bypasses it. Custom plugins are trusted executable composition and can contaminate protocol stdout, so pin the complete profile, not just the `dsh` binary. [ACP profile](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/bundle/acp-app/README.md)

The [SDK profile](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/bundle/sdk-app/README.md) exposes another JSON-RPC interface with its own outcome configuration. A provider's success mapping, including token-limit handling, does not determine Branchyard's acceptance decision.

## Driver contract

The planned Rust trait should return typed operations and a stream of normalized events. Keep provider-specific values available without leaking provider types into the public SDK.

| Operation | Required semantics |
|---|---|
| `probe` | Report binary identity, protocol version, auth readiness, and negotiated capabilities |
| `prepare` | Project instructions, registered tools, and isolated session state; report effective configuration |
| `open` | Explicit fresh, resume, or fork mode; incompatible modes are rejected |
| `submit` | Return a turn handle; distinguish local write, protocol acceptance, and completion |
| `observe` | Stream correlated events; report transport gaps and unknown outcomes |
| `interrupt` | Request turn cancellation; distinguish request delivery from confirmed terminal state |
| `close` | Bounded shutdown through the provider's lifecycle API |

Fresh, resume, and fork are an enum, not three optional fields that can conflict. Fork must produce a new native session identity. A driver that cannot fork must reject the operation; it may offer a separately named context handoff into a fresh session. Never silently substitute one for the other.

Resume references contain tenant/run ownership, driver profile, native identifier or artifact reference, workspace checkpoint, and schema version. Persist secrets separately. The public task ID must never be assumed to be a native harness session ID.

### Capabilities

Effective capability equals the intersection of provider support, pinned profile conformance, sandbox support, and policy. Advertise each independently:

- Persistent turns, explicit resume, native fork, steering, and cancellation.
- Tool approvals, filesystem/terminal callbacks, hooks, and MCP projection.
- Usage scope and units; whether reports cover native subagents.
- Session portability and required private files.
- Input modalities and provider-specific output types.

ACP's baseline prompt loop does not guarantee optional session loading, forking, or every extension. Negotiate capabilities and test them. ACP cancellation is a notification; receiving it is not a cancellation acknowledgment. [ACP lifecycle](https://agentclientprotocol.com/protocol/v1/overview)

### Events and state

Every normalized event carries task, run, attempt, generation, connection epoch, local sequence, observed time, and native correlation IDs when available. Attach a schema version and source profile. Store provider payloads as access-controlled artifacts after redaction; never force rich native events into a lossy lowest-common-denominator record.

Separate `Ready`, `TurnAccepted`, `MessageDelta`, `ToolStarted`, `PermissionRequested`, `UsageObserved`, `TurnEnded`, `SessionClosed`, and `OutcomeUnknown`. Model stop, process exit, task completion, and candidate acceptance are different transitions. Missing usage is unknown, not zero.

Transient display deltas may be coalesced under load. Permission requests, terminal outcomes, durable commands, and artifact publication may not be silently discarded. A reconnecting client gets a durable cursor; raw process byte streams are not assumed replayable.

## Transport and callback placement

Launch a process with an argument vector through `SandboxProvider.exec`. Keep stdin/stdout as protocol pipes and stderr as diagnostics. Do not wrap structured protocols in a PTY. Bound frame size, pending requests, spool size, callback concurrency, and idle deadlines. Unknown extensions are preserved or rejected according to the negotiated profile.

The ACP client may be on the host, but its filesystem and terminal callbacks must operate inside the selected sandbox. Absolute ACP paths are interpreted in that sandbox namespace. Never call host `std::fs` or spawn a host shell on a harness-supplied path. Path validation must handle symlinks and races; string-prefix checks alone are insufficient.

Keep a dedicated protocol reader active while requests are outstanding. Route callbacks into a bounded executor; do not hold the session mutation lock while waiting for permission or a tool. A slow artifact consumer must not block cancellation. Reconnect creates a new connection epoch so old responses cannot satisfy new request IDs.

On transport loss after sending a prompt, record uncertainty. Query native session state where possible. Otherwise stop and reconcile before retrying. At-least-once queue delivery does not authorize at-least-once arbitrary model turns.

## Permissions and delegated tools

The server evaluates a permission request against the current policy, tool identity, arguments, workspace binding, and attempt generation. The decision is scoped to that request and recorded. A policy may allow unattended operations; no interactive UI is required. An unsupported required gate is an admission error.

Hooks provide observations and, where documented, interception. They are not equivalent to a network or VM boundary. An MCP tool list does not restrict built-in shell tools. Pair tool policy with execution and egress limits.

The Branchyard tool surface initially needs `spawn`, `inspect`, `events`, `send`, `publish`, `propose_integration`, and `cancel`. Each maps to the same domain command as the SDK. A small instruction package can teach a harness how to use these tools; it is not the scheduler and cannot grant itself new authority.

Native subagents are a separate concern. If the harness launches invisible descendants, attribute them to its existing sandbox allocation and report incomplete model-level accounting. For tasks requiring individually fenced descendants, disable or qualify native delegation and use Branchyard's spawn operation. A prompt asking the harness to obey a budget is not hard enforcement.

## Packaging and extension

Build one immutable image per harness profile or a deliberately chosen small bundle. Include executable, adapter/helper, runtime libraries, verified version output, and configuration templates. Pin image digest, executable version, adapter version, schema revision, and conformance result. Resolve packages during image build, never through `npx latest` on the spawn path.

Each attempt receives a private home, temp directory, session store, and workspace. Share immutable tools and dependencies where supported. Do not share OAuth state, credentials, transcript directories, or mutable package caches across tenants. Native session restore and VM restore must both rebind current identity and policy.

Use Rust traits for built-in drivers and a versioned process/RPC boundary for external drivers. The first non-Rust helper should reuse an official SDK. Avoid a Rust dynamic-library ABI. Portable Wasmtime hooks may later rank eligible placements or inspect bounded metadata; they do not replace arbitrary coding harness processes.

## Qualification suite

Run keyless protocol tests first, then explicitly configured live-provider smoke tests. Do not mark a harness supported merely because `--version` succeeds.

| Area | Required cases |
|---|---|
| Startup | Missing auth, unsupported version, dirty stdout, slow initialization, trust/update gate |
| Session | Fresh session, exact resume, unsupported fork, fork returns child ID, wrong-tenant reference |
| Turn | Streaming tools, empty output, token limit, provider error, acknowledgment before completion |
| Policy | Denied tool, permission timeout, callback replay, unsupported mandatory control |
| Isolation | Host-path callback attempt, symlink escape, cross-session files, inherited credential state |
| Recovery | Transport loss before/after acceptance, process crash, stale response epoch, node partition |
| Cancellation | During a tool, during permission wait, descendants still running, forced sandbox teardown |
| Pressure | Large frames, log flood, slow subscribers, full spool, many idle sessions |
| Accounting | Cumulative counters, duplicate events, unknown cost, native subagent visibility |
| Result | Successful turn with failing checks, target movement, stale candidate, explicit rejection |

The support unit is a tested profile and its guarantees. Additional harnesses should mostly add profiles and conformance fixtures to an existing driver family.
