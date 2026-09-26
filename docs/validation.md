# Validation record

Prepared 16 September 2026. This record describes the initial foundation, not production harness support.

## Passed locally

| Check | Result |
|---|---|
| Vendored source integrity | 84 files match both their pinned Git blob IDs and SHA-256 hashes |
| Herdr catalog | 22 TOML manifests parse; harness and per-manifest rule IDs are unique |
| Rust compilation and tests | 64 tests pass on Rust 1.90.0: 9 resume tests including inherited upstream cases, 6 harness-registry tests, 35 harness-driver tests, 4 capability-admission tests, and 10 Substrate adapter tests |
| Claude Code driver | Replays a recorded Claude Code 2.1.283 stream-json session: launch, `initialize` and user frames match what the CLI accepted, and output maps to acknowledgment, session, message, cumulative usage and completion events. Permission, interrupt, limit, resume-mismatch and fork-identity cases use frames shaped by Agent SDK 0.3.283 types |
| Codex driver | Replays a recorded codex-cli 0.157.1 app-server session without credentials: all four outgoing frames equal the frames the binary accepted, and the turn ends as a failed 401. Approval, interrupt, usage, resume and fork cases use shapes from the binary's generated JSON Schema |
| Live driver qualification | `claude-code-stream-json` against Claude Code 2.1.283 and `claude-code-acp` against claude-agent-acp 0.81.2 each pass 9 of 9 scenarios: turns, permission denial and approval, interrupts during a permission wait and a tool, clean close, resume, fork (rejected as declared for ACP) and a lost connection. Local processes, not a sandbox. See [driver qualification](qualification/README.md) |
| ACP driver | Its `initialize` equals the one claude-agent-acp 0.81.2 accepted, and its response opens a session. Every outgoing frame deserializes as the official `agent-client-protocol-schema` 1.9.1 type |
| Harness identity registry | 27 harnesses; every Herdr manifest (22), Herdr resume source (18), Scion harness (9) and integration target (16) maps to exactly one ID, and Herdr's aliases agree with those mappings |
| Substrate client generation | `ateapi.proto` compiles unmodified with pinned `protoc` from `protoc-bin-vendored` |
| Substrate adapter over gRPC | Create/adopt, resume, inspect, suspend, checkpoint, branch, revert and UID-fenced destroy against an in-process fake `Control` service |
| Rust formatting and Clippy | Formatting passes; Clippy has no warnings across all targets |
| Runnable example | `plan_resume` prints an argument vector without starting a harness |
| Scion shared helper | 59 tests pass |
| Scion Codex provisioner | 12 tests pass |
| Scion Copilot provisioner | 37 tests pass |
| Scion Grok Build provisioner | 88 tests pass |
| Scion Hermes provisioner | 29 tests pass |
| Scion Muse Code provisioner | 14 tests pass |

The six passing Scion suites total **239 tests**. These use temporary fixtures and mocked operations; they do not authenticate to model providers or launch production harness sessions. Catalog parsing does not validate every terminal regex against live harness output.

## Known upstream incompatibility

At Scion revision `54b9387549ea2f673376c94b5d992010ebe9100a`, the Claude test suite expects Python-side model alias resolution, including `_resolve_model_alias` and shorthand constants. The corresponding `provision.py` documents that Scion's Go host resolves model aliases before setting `SCION_MODEL`. The selected source and tests therefore disagree when run as a standalone bundle.

The original 12-test suite produces three failure observations and eight error observations, including subtests. The required settings fixture is present; no failure is attributed to an omitted fixture. The files remain unchanged so the mismatch is reproducible.

`tools/test_scion.py --qualified` explicitly excludes that suite. CI separately runs `tools/check_scion_compatibility.py`, which compares the actual test IDs, failure kinds, and exception summaries with the checked-in baseline. An unexpected failure, changed failure, or resolved incompatibility fails that check and requires review. There is no blanket `continue-on-error` gate.

This compatibility check is not a passing qualification of the Claude provisioner. Keep it unqualified until the host/provisioner contract is deliberately resolved. Do not add a second alias resolver merely to make stale tests green; first choose the authoritative resolution boundary.

## Not validated

- Codex and every ACP harness other than claude-agent-acp against a live binary with credentials. The Codex recording ran without credentials. Claude recordings and qualification runs made real model calls: under about $1 in total by the harnesses' own estimates.
- Any profile inside a Branchyard sandbox: isolation, egress, credential projection and node recovery.
- The ten ACP harnesses other than the Claude adapter, and the Codex ACP adapter. Their profiles use launch commands from the integration matrix and are protocol-tested only.
- Any Agent Substrate cluster. The fake `Control` service models documented behavior only; TLS, authentication, tag readiness and real snapshot semantics are untested.
- Runtime isolation, KVM deployment, storage cloning, network enforcement, or startup latency.
- Any live harness session, provider credential path, ACP exchange, or native driver.
- The sixteen-profile matrix as deployed support.
- OpenRig's application-dependent TypeScript contract or Warp's application-dependent AGPL modules as standalone binaries.
- PostgreSQL/PGMQ execution, recursive budgets, dynamic graph mutation, failover, or Git promotion.

Those are explicit gates in [the implementation plan](implementation-plan.md), not capabilities implied by the passing unit tests.

## Reproduce

Use Python 3.12 and the checked-in Rust toolchain. Python tools use the standard library. `branchyard-controls` and `branchyard-sandbox` have no external dependencies; `branchyard-substrate` uses Tonic and Prost, fetched once from `Cargo.lock`.

```sh
cargo fetch --locked
python3 tools/verify_vendor.py
python3 tools/verify_derivatives.py
python3 tools/check_catalog.py
python3 tools/check_docs.py
python3 tools/test_scion.py --qualified
python3 tools/check_scion_compatibility.py
cargo fmt --all -- --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo run --locked --offline -p branchyard-controls --example plan_resume
```

For the unfiltered upstream result, run `python3 tools/test_scion.py`. It exits nonzero for the recorded Claude mismatch. The repository's CI runs the passing suites and the explicit incompatibility check as separate required steps.
