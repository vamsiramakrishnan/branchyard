# Writing a driver

How to add a harness to `crates/branchyard-harness`: implement or reuse a driver, prove it against a recorded transcript and the driver contract, register a profile, and qualify it against the live binary. The [compatibility matrix](compatibility.md) shows where every target stands.

Most harnesses need only a profile. If the harness speaks ACP v1, add an ACP profile (see [Register the profile](#register-the-profile)) and skip to [Record a fixture](#record-a-fixture). Write a driver only for a protocol no existing driver speaks.

## The sans-IO model

A driver is a state machine implementing `branchyard_harness::Driver`. It never spawns a process, reads a file, holds a credential or blocks. The caller starts `Opened.launch.argv` without a shell in `launch.cwd` (through `SandboxProvider.exec` in production, a local process in `branchyard-qualify`), writes the driver's frames to stdin, and passes each stdout line to `receive`. Stderr is diagnostics, never protocol.

A frame is one JSON value followed by one newline. Every method that can write returns its frames; the caller writes them in order. Events are the normalized `Event` enum; keep native identifiers in `native` fields and `PermissionRequest.input` rather than inventing new event types.

## Method obligations

| Method | Must |
|---|---|
| `capabilities` | Return what the profile offers before negotiation. A capability that depends on negotiation, like ACP resume, is declared possible and enforced again in `open`/`receive`. `admit` uses this to reject a task whose requirements are unmet. |
| `open` | Reject an empty `cwd` and a second `open` with `Rejected::InvalidOpen`. Reject a mode or option the protocol cannot honor with `Rejected::Unsupported` before launch (ACP rejects `Fork` and `model` this way). Return the argument vector, the working directory and the handshake frames. |
| `receive` | Handle exactly one line. Ignore blank lines. Report a non-JSON line as `ProtocolViolation`, and a response to an unknown request ID likewise. Emit `Ready` when the handshake completes, `OpenFailed` if it fails, and `SessionStarted` once the native identity is verified. Emit `Unrecognized` for well-formed messages the driver does not interpret. |
| `submit` | Return `Rejected::NotReady` before `Ready` and after the transport closes, and `Rejected::TurnInProgress` while a turn is in flight. Otherwise return a new local turn number and the prompt frames. Emit `TurnAccepted` only when the harness acknowledges the turn, and only if `turn_acknowledgment` is declared. |
| `interrupt` | Return `Rejected::NoTurn` with no turn in flight. Return the cancellation frames. Do not end the turn here. |
| `respond` | Return `Rejected::UnknownPermission` for a key that is not outstanding, including one already answered or withdrawn. Answer exactly one invocation. |
| `transport_closed` | Drop pending requests and permissions, stop accepting turns, and return `OutcomeUnknown` for the turn in flight, if any, then `SessionClosed`. |

Drivers inside this crate use the helpers in `src/lib.rs`: `frame` serializes a frame, `parse` implements the blank-line and non-JSON rules, `Turns` implements turn numbering, `TurnInProgress` and the close sequence, and `rpc_error` keeps JSON-RPC `error.data`, which often holds the cause.

## Rules the drivers follow

- **Never substitute a session mode.** `SessionMode` is `Fresh`, `Resume` or `Fork`. A driver that cannot honor the mode rejects it in `open` or fails the open; it never starts a fresh session instead. ACP resume uses `session/resume` or `session/load` only when advertised, else `OpenFailed`.
- **Verify native session identity.** A resume must come back under the requested ID; a fork must come back under a new ID, reported with `forked_from`. Claude Code reports a mismatch as `ProtocolViolation` (and a later `system/init` naming another session likewise); Codex reports it as `OpenFailed`.
- **Answer every request the profile does not implement.** Reply with a protocol error immediately and emit `UnsupportedRequest`, so the harness never waits forever: Claude hook callbacks and other control requests, Codex `item/tool/requestUserInput` and other server requests, ACP `fs/*` and `terminal/*` requests. The ACP client advertises no filesystem or terminal capability.
- **Per-invocation permissions only.** Map `PermissionDecision::Allow` and `Deny` to the protocol's one-time answer: Claude `allow` with the original input, Codex `accept`/`decline`, ACP `allow_once`/`reject_once`. Never select a standing rule such as ACP `allow_always`; if the harness offers no one-time option, return `Rejected::Unsupported`. Report a harness-side withdrawal as `PermissionWithdrawn`.
- **Separate interrupt acknowledgment from the terminal state.** `InterruptAcknowledged` means the harness confirmed the request; the turn ends only with `TurnEnded`. ACP cancellation is a notification with no acknowledgment, so the ACP driver emits none; it also answers every outstanding permission request as cancelled, as the protocol requires.
- **Transport loss is `OutcomeUnknown`.** Whether an in-flight turn did work is unknown and must be reconciled before retrying. Never report it as failed or completed.
- **Missing usage is unknown, not zero.** Fields the protocol does not report are `None`. Set `Usage.cumulative` when the values are session totals, as Claude Code's `modelUsage` and Codex's `tokenUsage.total` are. Declare `usage: false` when the protocol has no stable usage report.
- **Unstable protocol features stay off.** `agent-client-protocol-schema` is built with `default-features = false`, so its `unstable_*` features, including `session/fork` and end-of-turn usage, are unavailable. Do not consume a field the schema marks unstable, even when an agent sends it.
- **Keep model stop, process exit and acceptance apart.** `TurnEnded` carries `Completed`, `Interrupted`, `Failed`, `LimitReached` or `Refused`; process exit is `SessionClosed`.

## Record a fixture

A fixture is a transcript of the driver's real exchange with the binary, in `tests/fixtures/<harness>-<version>-<what>.jsonl`. The first row is a provenance note, then one row per frame:

```json
{"note":"codex-cli 0.157.1 app-server, isolated CODEX_HOME, no credentials: the turn fails with 401. Paths and installation ID redacted."}
{"dir":"out","frame":{"id":1,"method":"initialize","params":{"clientInfo":{"name":"branchyard","version":"0.0.1"}}}}
{"dir":"in","frame":{"id":1,"result":{"userAgent":"branchyard/0.157.1 (Ubuntu 24.4.0; x86_64) linux (branchyard; 0.0.1)","codexHome":"/home/agent/.codex","platformFamily":"unix","platformOs":"linux"}}}
```

`out` rows are frames the client wrote; `in` rows are lines the harness printed. Record one short exchange: the handshake and one turn with a fixed prompt such as `Say hello.`. A turn that fails for lack of credentials is still useful evidence of framing, as the Codex fixture shows.

`branchyard-qualify` writes a transcript per session to `<workdir>/<label>.transcript.jsonl` (`main`, `resume`, `fork`, `connection-lost`) with raw `line` strings instead of `frame` values. Convert the rows you keep:

```sh
python3 -c 'import json, sys
for r in map(json.loads, sys.stdin):
    if r["line"].strip():
        print(json.dumps({"dir": r["dir"], "frame": json.loads(r["line"])}, separators=(",", ":")))' \
  < main.transcript.jsonl > fixture.jsonl
```

Then redact, and say what you redacted in the note:

- Local paths: the working directory becomes `/workspace`, the home `/home/agent`.
- Installation and machine identifiers: all-zero UUIDs, as Codex's `installationId`.
- Account and rate-limit details, and request-tracing IDs such as `cf-ray` and request IDs: `REDACTED` or removed.
- Environment-specific lists: installed commands, tools, skills, plugins, agents and models become `[]`, as in the Claude Code fixture.

Keep every field the driver reads: session and turn IDs, UUIDs the driver correlates, usage and cost. The replay must pass on the redacted file.

## Use the conformance kit

`branchyard_harness::conformance` holds the helpers the crate's own tests use. They panic like `assert_eq!`, naming the frame or rule that failed.

**Replay** a fixture: `Replay` feeds recorded `in` frames to the driver and compares each recorded `out` frame with the next frame the driver wrote. When nothing is pending, it submits the next queued prompt. Declare fields that legitimately differ per run:

- `.alias("/pointer")`: a value the driver chooses, such as a request ID or a random UUID. The pair is learned from the outgoing frame; later incoming frames get the driver's value, anywhere for strings and at the same pointer for other values such as numeric JSON-RPC IDs.
- `.ignore("/pointer")`: a value not compared, such as `/params/clientInfo/version`, which is the crate version.
- `.answer_permissions(decision)`: answer permission requests during the replay so a recorded answer is compared too.

```rust
let transcript = Transcript::load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude-code-2.1.283-stream-json-turn.jsonl"));
let replayed = Replay::new(&transcript)
    .alias("/request_id")
    .alias("/uuid")
    .prompt("Say hello.")
    .run(&mut driver, &opened);
assert_eq!(replayed.sent, 2);
assert!(replayed.unsent.is_empty());
// then assert on replayed.events
```

`Replayed.unsent` holds frames the driver wrote after the last recorded one; `Replayed::ours` maps a recorded aliased value to the driver's.

**Check frame shapes** with `decode` (newline-terminated, one line, one JSON value) and `feed`, which decodes every response frame. `assert_conforms::<T>(&frame, "/params")` checks that part of an outgoing frame deserializes as a published schema type, as the ACP tests do with `agent-client-protocol-schema`.

**Check the contract** with `assert_contract(driver, open, answer)`, where `answer` plays the harness side of the handshake by returning the messages it would print for each frame the driver writes. It checks that submitting before `Ready` and after close is rejected, one turn at a time, `OutcomeUnknown` then `SessionClosed` on transport loss mid-turn, unknown permission keys and interrupts without a turn are rejected, and a non-JSON line is one `ProtocolViolation`. If the harness prints lines unprompted before any frame (such as Antigravity's `init`), use `assert_contract_greeted`, which feeds that greeting first. `tests/conformance.rs` runs the contract for every profile; add an `answer` arm for a new protocol there.

Beyond the replay and the contract, test each rule above with scripted messages: identity mismatches for resume and fork, unsupported requests, permission round trips and withdrawal, interrupt acknowledgment followed by the terminal outcome, limit and error outcomes, and missing usage. `tests/claude_code.rs`, `tests/codex.rs` and `tests/acp.rs` are the models.

## Register the profile

**Harness identity.** Every profile names a harness ID from `branchyard_controls::harness::HARNESSES`. If the harness is not registered, add it there. An integration target also needs `.target("Row name")` and a row in the [integration matrix](harness-integration.md#sixteen-initial-integration-targets), in the same order; `every_integration_target_is_registered_once` compares them and asserts the count, 16. If a vendored Herdr manifest, Herdr resume source or Scion harness names it, record that name too; the controls tests fail when a vendored name is unmapped or maps to two harnesses. The controls crate has its own owners; coordinate a new target with them.

**Profile.** Add an entry to `PROFILES` in `crates/branchyard-harness/src/profiles.rs`. The first profile for a harness is its default. Set `checked_against` to the harness version your fixture or generated schema came from, and nothing if there is none. For an ACP harness, `acp(id, harness, command)` suffices; pin agent-specific session options with `acp_session_meta`, as `claude-code-acp` does to keep permission bypass unavailable. A new driver family needs a `Protocol` variant, an arm in `Profile::driver_with` and a label in `examples/compat_matrix.rs`; adding a variant breaks exhaustive matches on `Protocol` in other crates, so say so in the change.

The profile tests then require:

- unique profile IDs and a registered harness;
- each integration target either has a default profile or is listed in `NOT_IMPLEMENTED` with a reason, never both, so remove the target from `NOT_IMPLEMENTED` when you implement it;
- an ACP profile's command, in backticks, appears in the integration matrix (the Claude and Codex adapters are exempt);
- `tests/conformance.rs` passes the contract for the new profile;
- `docs/compatibility.md` is current: regenerate it with `cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md`.

## Qualify live

Protocol tests prove framing, not behavior against the binary. `branchyard-qualify` runs nine scenarios against the real harness: a fresh turn, permission denial and approval, interrupts during a permission wait and during a tool, clean close, resume, fork, and a lost connection. The [qualification page](qualification/README.md#scenarios) states when each passes.

```sh
mkdir -p /tmp/qual
cargo run -p branchyard-qualify -- --profile gemini-cli-acp --workdir /tmp/qual \
    --max-cost-usd 3 --report docs/qualification/gemini-cli-acp.json
```

The work directory must exist. `--command` replaces the executable when it is installed elsewhere. The runner strips `ANTHROPIC*`, `CLAUDE*`, `OPENAI*` and `CODEX*` variables unless kept with `--keep-env NAME`. Scenarios make real model calls; `--max-cost-usd` applies only to profiles that report cost. The runner exits non-zero if any scenario fails, and still writes the report.

Then:

1. Add `"date": "YYYY-MM-DD"` to the report by hand, and `"installed_from"` if the harness came from a package registry. The runner does not write them; the matrix shows a report without a date as undated.
2. Keep one report per profile in `docs/qualification/`. A failing report is evidence too; the matrix shows passes out of total.
3. Add the result and any findings to the [qualification page](qualification/README.md), and regenerate `docs/compatibility.md`.

Live qualification uses a local process with a scrubbed environment and private home. It does not qualify sandbox isolation, credentials or recovery; do not describe a profile as supported on its strength.

## Before sending the change

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
python3 tools/check_docs.py
```
