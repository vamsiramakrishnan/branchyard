# Validation record

Prepared 16 September 2026. This record covers the controls foundation and SDK/CLI/skill slice, not production harness support.

## Passed locally

| Check | Result |
|---|---|
| Vendored source integrity | 82 files match both their pinned Git blob IDs and SHA-256 hashes |
| Herdr catalog | 22 TOML manifests parse; harness and per-manifest rule IDs are unique |
| Rust controls | 9 resume-control tests pass on Rust 1.90.0 |
| Protocol | 5 tests cover shape/version rejection, normalization, bounds and example round-trips |
| SDK HTTP contract | 10 tests cover idempotent replay, changed input, lost acknowledgment, dropped futures, redirects, bounded bodies, cursor checks and concurrent submissions |
| Distribution and CLI | 6 tests cover extraction, canonical skill parity, deterministic archives, installer refusal, launcher, generated-schema drift and CLI error behavior |
| Plugin and skill metadata | Plugin-creator validator and skill-creator validator pass locally |
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

- Live loading of the plugin in Codex/Claude or the standalone skill in other hosts.
- Production HTTP authentication, idempotency retention or graph transactions; SDK tests use fixtures.
- Runtime isolation, KVM deployment, storage cloning, network enforcement, or startup latency.
- Any live harness session, provider credential path, ACP exchange, or native driver.
- The sixteen-profile matrix as deployed support.
- OpenRig's application-dependent TypeScript contract or Warp's application-dependent AGPL modules as standalone binaries.
- PostgreSQL/PGMQ execution, recursive budgets, dynamic graph mutation, failover, or Git promotion.

Those are explicit gates in [the implementation plan](implementation-plan.md), not capabilities implied by the passing unit tests.

## Reproduce

Use Python 3.12 and the checked-in Rust toolchain. Repository Python tools use the standard library. The controls crate remains dependency-free; the SDK uses locked Rust dependencies. Fetch those once before offline checks. A C compiler and CMake are needed by the selected TLS dependency.

```sh
python3 tools/verify_vendor.py
python3 tools/verify_derivatives.py
python3 tools/check_catalog.py
python3 tools/check_docs.py
python3 tools/test_scion.py --qualified
python3 tools/check_scion_compatibility.py
cargo fetch --locked
cargo fmt --all -- --check
cargo test --workspace --locked --offline
cargo build --locked --offline -p branchyard-cli
python3 -m unittest discover -s tests -p 'test_*.py' -v
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo run --locked --offline -p branchyard-controls --example plan_resume
```

For the unfiltered upstream result, run `python3 tools/test_scion.py`. It exits nonzero for the recorded Claude mismatch. The repository's CI runs the passing suites and the explicit incompatibility check as separate required steps.
