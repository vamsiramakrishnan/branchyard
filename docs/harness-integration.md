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

Each row maps to one ID in `branchyard_controls::harness`, together with the Herdr and Scion names for the same harness. Its tests fail if this table and the registry diverge.

Commands describe upstream entry points, not ready-to-run deployment recipes. Authentication, executable versions, images, and required capabilities must be qualified before activation.

| Harness | Preferred profile | Documented entry point / integration | Qualification focus |
|---|---|---|---|
| Claude Code | Existing ACP adapter; native SDK helper for deeper controls | [Claude Agent ACP](https://github.com/agentclientprotocol/claude-agent-acp), built on the [Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview) | Permission callbacks, hooks, explicit resume/fork, helper and CLI version pairing |
| Codex | Native App Server | [`codex app-server`](https://developers.openai.com/codex/app-server/); existing [Codex ACP adapter](https://github.com/zed-industries/codex-acp) as an alternate profile | Bidirectional approvals, thread/turn IDs, cancellation, reconnect |
| Antigravity | Native NDJSON (`antigravity-stream-json`, implemented, unqualified) or official SDK | [`agy --input-format stream-json --output-format stream-json`](https://antigravity.google/docs/cli/headless/) | `event`-based frames, cumulative usage, cached authentication, headless permission behavior |
| Oh My Pi | ACP; native RPC when required | [`omp acp` / `omp --mode rpc`](https://github.com/can1357/oh-my-pi) | RPC completion versus command acknowledgment; extension interaction; resume syntax |
| DeepSeek Harness | ACP | [`dsh --profile acp`](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/bundle/acp-app/README.md) | Trusted profile composition, stdout purity, session persistence, startup-only configuration |
| Gemini CLI | ACP | [`gemini --experimental-acp`](https://geminicli.com/docs/cli/cli-reference/) | Experimental mode/version pairing, auth and policy configuration |
| OpenCode | ACP | [`opencode acp`](https://opencode.ai/docs/acp/) | Optional methods, cwd, MCP, cancellation; keep native server API as a separate profile |
| Pi | Native RPC (`pi-rpc`, implemented, unqualified) | [`pi --mode rpc`](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md) | Prompt acknowledgments versus agent completion; confined session-file references |
| Goose | ACP | [`goose acp`](https://goose-docs.ai/docs/gdk/acp/) | Stdout transport, provider identity, permission and tool callbacks |
| Aider | Batch process | [Scripted `--message` execution](https://aider.chat/docs/scripting.html) | Exit/result mapping and artifact validation; no assumed persistent RPC or native fork |
| Cursor CLI | ACP | [`agent acp`](https://cursor.com/docs/cli/acp) | Authentication and executable aliases; permission handling in unattended operation |
| GitHub Copilot CLI | ACP | [`copilot --acp`](https://github.blog/changelog/2026-01-28-acp-support-in-copilot-cli-is-now-in-public-preview/) | Protocol/version qualification; SDK backend is a distinct alternate interface |
| Amp | Native NDJSON (`amp-stream-json`, implemented from documentation, unqualified) | [`amp --execute --stream-json`](https://ampcode.com/docs/cli/streaming-json), with documented stream input mode when needed | Keep execution in our sandbox; turn boundaries and server-compatible credentials |
| Qwen Code | ACP | [`qwen --acp`](https://qwenlm.github.io/qwen-code-docs/en/users/integration-zed/) | ACP package version, scope projection, approvals, optional session methods |
| Kimi CLI | ACP | [`kimi acp`](https://github.com/MoonshotAI/kimi-cli) | Pin the legacy CLI or separately qualify its successor; do not silently migrate saved sessions |
| Hermes | ACP | [`hermes acp`](https://hermes-agent.nousresearch.com/docs/user-guide/features/acp) | Install ACP extra; isolate persistent home; qualify resume and approval scopes |

This roster does not imply sixteen custom schedulers. Profiles share the task lifecycle and sandbox provider, while drivers translate only the relevant wire contract. Terminal automation is a separate, lower-confidence compatibility mode, not the default for a protocol-capable harness.

## Native interfaces that need specific treatment

### Codex

App Server exposes initialization, thread lifecycle, turn control, notifications, and requests that the client must answer. Keep native thread and turn IDs in the session record. Schema generation and parsing must match the installed binary. Do not assume the ACP and App Server envelopes are identical. The client must service approval requests while awaiting a turn; blocking that path deadlocks progress. [App Server contract](https://developers.openai.com/codex/app-server/)

Branchyard should store the last observed native turn and its outcome before allowing another mutating prompt after reconnect. Resuming a thread is not proof that an uncertain previous turn did no work. Use a fresh attempt only after reconciling the prior attempt's authority and effects.

### Claude Code

The default profile drives print mode directly: `claude -p` with stream-json input and output and `--permission-prompt-tool stdio`, the launch the Agent SDK itself uses. The Agent SDK publishes this stdout protocol as typed frames (`StdoutMessage`, with `control_request`/`control_response` for the handshake, interrupts and `can_use_tool` permission prompts), so the Rust driver follows those types rather than an undocumented stream. It pins the pair it was checked against, Claude Code 2.1.283 with Agent SDK 0.3.283, and must be rechecked on upgrade. The maintained ACP adapter remains an alternate profile. A helper built on the official SDK is still the route for SDK-only features such as hook callbacks and in-process MCP servers, which the stream-json profile refuses. [Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview)

Structured print mode remains useful for bounded batch runs, but its existence does not establish a complete bidirectional control contract. Choose the driver according to required capabilities. Do not accidentally disable required hooks or Branchyard tools when selecting a reduced-discovery launch mode. [Headless operation](https://code.claude.com/docs/en/headless)

### Antigravity

The documented streaming CLI accepts NDJSON user events and emits a result for each turn. Wait for that result before submitting the next prompt. Use the exact conversation ID for continuation. Its schema uses `event`; Claude-style control request/response frames are not supported. Usage fields can be cumulative, so billing observations require deltas rather than summing every result. Headless authentication must be provisioned before launch; [provisioning](provisioning.md) sets `GEMINI_API_KEY` or `GOOGLE_API_KEY` from `--secret`, and does not yet cover `AGY_TOKEN` or Vertex AI. [Headless contract](https://antigravity.google/docs/cli/headless/)

Branchyard must record which policy controls this mode actually supports. If a task requires per-tool decisions that the selected mode cannot expose, reject that profile for the task or use a separately qualified SDK profile. Streaming JSON alone is not a permission boundary.

Checked against Antigravity CLI 1.2.11 without a model call ([fixtures](../crates/branchyard-harness/tests/fixtures)): `init` arrives at startup, before any prompt, so it is the handshake. A `--conversation` ID the CLI does not know starts a new conversation with only a stderr warning; the driver fails the open when the `init` ID differs. Without credentials the CLI prints one `ERROR` result and no `init`. A prompt such as `/model` ends the session unless `--disable-slash-commands` is passed, which the driver always does. The stream has no permission requests and no cancellation (SIGINT ends the process), so the profile declares neither; Branchyard must write `permissions.allow` rules into the private `~/.gemini/antigravity-cli/settings.json` and never pass `--dangerously-skip-permissions`. There is no fork.

### Pi and Oh My Pi

Treat these as separate versioned profiles. Both expose machine interfaces, but their resume syntax and extensions differ. Herdr's selected recipes use `pi --session <value>` and `omp --resume=<value>`. Those are resume recipes, not proof of protocol startup flags for every version. [Vendored recipes](../vendor/herdr/src/agent_resume.rs)

RPC commands can be acknowledged before the agent finishes. Track the documented terminal event rather than marking success on the request response. If a mode emits interactive extension requests, the profile must declare and implement their responses; otherwise fail explicitly. Never let a session file path choose a host file outside the sandbox's authorized store.

The `pi-rpc` profile follows [Pi's RPC documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md) and `rpc-mode.js` from `@earendil-works/pi-coding-agent` 0.87.1, and replays transcripts recorded from that version without a model call. RPC mode has no greeting, so `get_state` is the handshake and names the session. A `prompt` response is the acknowledgment; the turn ends at `agent_settled`, after retries, and the `abort` response arrives only after that. Resume uses `--session <id>`, fork `--fork <id>`; since `--session` also matches ID prefixes, the returned ID is checked. References that Pi would read as file paths (a path separator or `.jsonl`) are refused before launch. Pi has no approval step, so the profile declares no tool approvals: restrict tools with `--tools` and confine the process with the sandbox. Extension dialogs are answered as cancelled. Prompts starting with `/` are refused, because an extension command can consume one without an agent run and never settle. A `--session` ID found only in another project makes Pi print a fork question on stdout; the driver reports it as a protocol violation.

### Amp

The `amp-stream-json` profile rests on the [streaming JSON documentation](https://ampcode.com/docs/cli/streaming-json) alone. amp 0.0.1790467310-ge147a9 installs from npm, but without an account it prints a device login on stdout, and with a placeholder key it exits before printing a protocol line, so its fixtures are derived from documentation, not recorded. With `--stream-json-input`, `result` arrives once, after stdin closes, so the driver ends a turn at the first top-level assistant message with a terminal `stop_reason` (`end_turn`, `stop_sequence`, `max_tokens`, `refusal`), or at an error. It assumes `init` arrives before the first message; if it does not, the open never becomes ready. Amp does not ask before running tools and has no cancellation message, fork or model flag: restrict tools in the private settings file (`amp.tools.disable`, `amp.mcpPermissions` or a plugin), never set `amp.dangerouslyAllowAll`, and never pass `-ox` or `--executor`, which run the thread off Branchyard's sandbox.

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
| `steer` | Deliver user input into the turn in flight through the harness's own mid-turn input; never an interrupt and resubmit. Refuse, with the reason, where the protocol has none |
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

## Implemented drivers

`crates/branchyard-harness` implements the driver contract above as sans-IO state machines. A driver builds the argument vector and the frames to write, and turns each line the harness prints into normalized events. The process runs through `SandboxProvider.exec`. Profiles map each harness ID in `branchyard_controls::harness` to a driver and launch command. [Writing a driver](writing-a-driver.md) explains how to add a profile or driver, test it with the conformance kit, and qualify it.

| Driver | Profiles | Evidence |
|---|---|---|
| Claude Code stream-json | `claude-code-stream-json` (default for Claude Code) | Live protocol qualification, 9 of 9 scenarios, against Claude Code 2.1.283; replay of a recorded session; frames follow Agent SDK 0.3.283 types |
| Codex App Server | `codex-app-server` (default for Codex) | Replay of a recorded codex-cli 0.157.1 session; every outgoing frame equals one the binary accepted; shapes from `codex app-server generate-json-schema` |
| ACP v1 | `claude-code-acp` (live protocol qualification, 9 of 9, against claude-agent-acp 0.81.2), `codex-acp`, Oh My Pi, DeepSeek Harness, Gemini CLI, OpenCode, Goose, Cursor CLI, GitHub Copilot CLI, Qwen Code, Kimi CLI, Hermes | Recorded claude-agent-acp 0.81.2 `initialize`; every outgoing frame deserializes as the `agent-client-protocol-schema` type |
| Antigravity stream-json | `antigravity-stream-json` (default for Antigravity) | Replay of Antigravity CLI 1.2.11 transcripts recorded without a model call: a failed turn, resume, resume of an unknown ID, missing credentials; shapes from the [headless documentation](https://antigravity.google/docs/cli/headless/) |
| Pi RPC | `pi-rpc` (default for Pi) | Replay of pi 0.87.1 transcripts recorded without a model call: a retried failed turn, abort, resume, fork, missing credentials; shapes from Pi's RPC documentation and `rpc-mode.js` |
| Amp stream-json | `amp-stream-json` (default for Amp) | Documentation only: fixtures derived from the [streaming JSON documentation](https://ampcode.com/docs/cli/streaming-json), not recorded |

The drivers implement `open`, `submit`, `observe` (as `receive`), `interrupt`, `steer` (where the harness supports it; see [below](#steering-a-running-turn)) and permission answers. `probe` and `prepare` are not implemented yet, and `close` is process termination through the sandbox provider, reported as `SessionClosed`.

Fifteen of the sixteen targets have a default profile. Aider has no persistent protocol; it needs a separate batch profile, not a session driver. A test fails if a target is neither implemented nor listed with its reason.

The drivers enforce the contract rather than trusting the harness:

- A resume that comes back under another session ID, or a fork that keeps the parent's, is a protocol violation or a failed open, never a silent fresh session.
- ACP fork is rejected because `session/fork` is unstable. ACP resume uses `session/resume` or `session/load` only when the agent advertises it; history replayed by `session/load` is not reported as a new turn.
- Permission requests surface as events and are answered per invocation. ACP answers select only `allow_once` or `reject_once`, never a standing rule. Requests a profile does not implement (Claude hook callbacks, Codex user-input prompts, ACP filesystem and terminal callbacks) are answered with an error immediately.
- Cancellation distinguishes acknowledgment from the turn's terminal state. ACP cancel also answers outstanding permission requests as cancelled, as the protocol requires.
- Usage is reported as cumulative session totals where the harness reports it (Antigravity's `result.usage` too), per model response where that is all it reports (Pi, Amp), and as unknown where it does not. A closed connection during a turn yields `OutcomeUnknown`.
- Antigravity, Pi and Amp route no tool approvals and declare `tool_approvals: false`; Antigravity and Amp also declare no cancellation. `admit` rejects them for a task that requires either. Their permission boundary is the harness's own configuration, which Branchyard must write into the private home, plus the sandbox; none of them is ever launched with a bypass flag or setting. The engine refuses these profiles for every run, fan, send, fork and delegated spawn unless the task sets `TaskOptions::unapproved_tools` (`by --allow-unapproved-tools`); with it, their tools run without Branchyard's answers. The server never sets it, so it does not run them.

`claude-code-stream-json` and `claude-code-acp` have passed live protocol qualification against Claude Code 2.1.283 and claude-agent-acp 0.81.2: turns, permission denial and approval, interrupts during a permission wait and during a tool, clean close, resume, fork and a lost connection. See [driver qualification](qualification/README.md) for the reports and findings. That run used local processes, not a Branchyard sandbox. Every profile remains unqualified for isolation, credentials and recovery, and the Codex and remaining ACP profiles have not run live.

`antigravity-stream-json`, `pi-rpc` and `amp-stream-json` are unqualified: none has completed a model turn. The Antigravity and Pi transcripts prove framing, handshake, session identity and error turns against the real binaries with model endpoints pointed at a closed local port; Amp has no recording at all. `branchyard-qualify` assumes per-invocation approvals, and cancellation for its interrupt scenarios; it does not skip them for profiles that declare neither, so qualifying these profiles needs scenarios for configuration-based permissions first.

## Steering a running turn

`Driver::steer(text)` delivers user input into the turn in flight without ending it; the turn keeps its number and ends once, with `TurnEnded`, after the harness has answered the steered input too. The harness confirms or refuses each steer with `SteerAccepted` or `SteerRejected`, numbered in call order; a refusal covers input an interrupt cancelled or the turn ended before delivering. `Capabilities::steer` says whether a profile offers it (`Requirements::steer` asks for it in admission); a profile without it refuses every steer with `Rejected::Unsupported` and a reason, whatever its state. A driver that needs the harness to acknowledge the turn first answers `Rejected::SteerNotYet`, and the engine retries. Nothing is ever turned into an interrupt followed by a new prompt. The conformance kit checks these rules for every profile (`assert_steer_contract`), and `Replay::steer` and `Replay::interrupt` replay recorded exchanges that steer. The engine's durable steer queue, which lets any process steer a turn another runs, is described in [durability](durability.md#steered-input); `Branch::steer`, `by send --steer`, `POST …/steer` and the `steer` delegation tool reach it ([surfaces](surfaces.md)).

Where a harness binary was available, its behavior was checked on 27 September 2026 without a model call: the harness ran with a scrubbed environment, a private home and a fake key, pointed at a local stand-in API on 127.0.0.1 (Anthropic Messages for Claude Code, Pi and claude-agent-acp; OpenAI Responses for Codex) that streams a slow response over 4 seconds; input was written 2 seconds in, and the requests the stand-in then received showed where the model saw it. The exchanges are recorded as [fixtures](../crates/branchyard-harness/tests/fixtures) the drivers replay.

| Profile | Mechanism | When the model sees the input | With an interrupt | Evidence |
|---|---|---|---|---|
| `claude-code-stream-json` | Another stream-json `user` message on stdin, as the Agent SDK's streaming input writes one | Queued (`command_lifecycle` `queued`) and delivered before the next model call. If the turn continues after a tool result, it goes into that same request as a system reminder that the user sent a message while the model was working, and the turn's one `result` lists both messages in `user_message_uuids`. If the turn would end without another model call, the CLI runs the message right after as a follow-up cycle with its own `result`; the driver keeps the turn open until a `result` has answered every steered message | Sent with `cancel_queued: true` (advertised as `interrupt_cancel_queued_v1`), which cancels queued messages; without it the CLI reports them `still_queued` and runs them after the interrupt | Verified against Claude Code 2.1.283: tool boundary, follow-up and interrupt recorded |
| `codex-app-server` | `turn/steer` with `expectedTurnId` (the turn in flight) and a `clientUserMessageId` | Once the model response in progress finishes, Codex records it as a `userMessage` item carrying that client ID and samples again within the same turn; one `turn/completed` | Accepted input not yet recorded is dropped with no further model call; the driver reports it rejected when the turn completes | Verified against codex-cli 0.157.1: steer and interrupt recorded; a wrong turn ID (`expected active turn id … but found …`) and no active turn (`no active turn to steer`) are JSON-RPC errors |
| `pi-rpc` | The `steer` command | Queued (`queue_update`) and delivered after the assistant message in progress and its tool calls, before the next model call, within the same run; one `agent_settled` | Pi keeps queued messages through `abort` and delivers them with the next prompt, and also accepts a `steer` when no run is active and holds it the same way; the driver sends `clear_queue` before `abort` and again at `agent_settled` if any is still queued, reports them rejected, and refuses to steer without a turn | Verified against pi 0.87.1: steer, and steer with `clear_queue` and `abort`, recorded; without `clear_queue` the next prompt carried the message |
| `claude-code-acp` | The `_session/steering` extension request, used only when the agent's `initialize` response advertises `_meta.steering.supported`, always with `idleBehavior: "promptRequired"` | Answered `injected`: the model response in progress is aborted (its partial text kept) and the same prompt continues with the steered message; one prompt response. After the prompt ended it answers `promptRequired` and delivers nothing | A `session/cancel` after an injected steer ends the prompt `cancelled` with no further model call | Verified against claude-agent-acp 0.81.2 (advertised in the [recorded](../crates/branchyard-harness/tests/fixtures/claude-agent-acp-0.81.2-initialize.jsonl) `initialize`): steer and a steer after the prompt ended recorded |
| Other ACP profiles | The same extension, only when advertised | As the agent implements it | As the agent implements it | Unverified: ACP v1 has no method for input into a running prompt (a second `session/prompt` is a new turn, which claude-agent-acp queues, `promptQueueing`); an agent that does not advertise the extension refuses steering with the reason |
| `antigravity-stream-json` | None: refused | — | — | Unverified, from the [headless documentation](https://antigravity.google/docs/cli/headless/), which says to wait for a turn's result before writing the next input |
| `amp-stream-json` | None: refused | — | — | Unverified. The [documentation](https://ampcode.com/docs/cli/streaming-json) describes `steer: true` on a streaming input message, handled "at the next interruption point while the agent is busy", but not how the output then marks the end of a turn, which this driver infers; it stays refused until recorded against a binary |

Claude Code also takes context mid-turn through hooks: a `PostToolUse` hook command's `additionalContext` reached the next model call after a tool (`PostToolUse:Read hook additional context: …`) in the same stand-in setup. Warp's Claude Code harness delivers a lead agent's messages that way, staging them in files. That arrives only at a tool boundary and needs a file the hook reads; since stream-json input also covers a turn with no further tool call, the driver does not use hooks.

Steered input runs under the turn's own budget and permission policy: its tool calls ask like any other, and its cost counts toward the same limits.

Each driver names its own mechanism above as `Driver::steer_boundary()` (`crates/branchyard-harness/src/lib.rs`) — `claude_next_model_call`, `codex_turn_steer`, `pi_steer`, `acp_session_steering`, or `unsupported` for a driver that refuses every steer — recorded on a delivered inbox message's `DeliveredVia::Steer { boundary, .. }` so `by log`/JSON says where it actually landed; see [delegation](delegation.md#delivery). It is also read as the "Steer" column's evidence level in [compatibility](compatibility.md): `live-tested` for the profiles verified against a real binary above, `not verified` for an ACP agent other than claude-agent-acp that only ever advertised the extension here.

## Provisioning a harness's home

Before each turn, on every provider, the harness's provisioner prepares its home and environment: credentials from `--secret`, MCP servers, instructions, model, reasoning effort and telemetry, translated from Scion's per-harness provisioners into [`branchyard-provision`](../crates/branchyard-provision/src/lib.rs). It is also the one path by which Branchyard's delegation server and skill reach a harness. MCP servers and instructions stay on the driver's session channel where it has one (Claude Code, Codex, every ACP profile); a driver without one gets them in the harness's native configuration in a private home, which is how the Antigravity profile now takes MCP servers and instructions it used to refuse. Pi and Amp still refuse them: Scion has no provisioner for either, and their configuration formats are unverified here. Nothing secret goes on a harness's command line: Claude Code's stream-json driver gets its MCP servers as the path of a 0600 file, and refuses variables or headers inline. Each plan says how every secret reaches the harness and whether its tools inherit it; Claude Code's API key reaches it through `apiKeyHelper` and a 0600 file rather than its environment. [Provisioning](provisioning.md) gives the per-harness mapping, which secrets each harness's tools can see, what was not ported and why, and the security rules; of it, only Claude Code's key, MCP file and MCP headers have been checked against a real binary, offline.

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
