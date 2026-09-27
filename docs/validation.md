# Validation record

Prepared 16 September 2026 and updated 27 September 2026. Everything here ran on one Linux host or in CI; nothing ran against a Substrate cluster, KVM, or a real harness beyond the live rows. It is not a claim of production harness support.

## Passed locally

| Check | Result |
|---|---|
| Vendored source integrity | 86 files match both their pinned Git blob IDs and SHA-256 hashes; Scion's 46 are pinned at `d9b9e6a` |
| Derivative provenance | The Herdr resume patch is the exact diff from its vendored source; 13 Scion derivatives name their origin, revision, sources, license and modification, and the 32 vendored sources they follow still have the blob IDs recorded at translation (`patches/scion-provision.json`) |
| Herdr catalog | 22 TOML manifests parse; harness and per-manifest rule IDs are unique |
| Rust compilation and tests | 532 tests pass on Rust 1.90.0 with `cargo test --workspace --locked --offline`, 6 more ignored (4 need a Substrate cluster, 2 are child-process helpers): 15 controls, 77 harness drivers (the conformance contract across all 17 profiles, recorded replays, the compatibility-matrix freshness check), 74 provisioning, 18 workspace, 28 runtime (provider conformance on the local provider), 110 SDK engine, 72 CLI (22 running the built `by`), 30 server, 12 client, 7 MCP, 10 sandbox contract, 11 Microsandbox mapping, 41 Substrate and 27 bridge (2 only as root). Three consecutive runs passed. The run leaves no harness, bridge or zombie process behind |
| PostgreSQL store | Clippy builds with `--features branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres`. The tests were not re-run against PostgreSQL for provisioning. At the last run, with `--features postgres` and `BY_TEST_POSTGRES_URL` against PostgreSQL 16: 212 tests passed in `branchyard`, `branchyard-server` and `branchyard-cli`, none skipped. One storage conformance suite (fencing, expiry, steps, cancels, records, reservations, events, concurrent appends, races) runs on both SQLite and PostgreSQL; engine turns, waits across yards, cancel and recovery, and the server's restart and `by serve --database` run on PostgreSQL. CI runs them against a `postgres:16` service |
| Delegation and surfaces | Hermetic: a fake-agent harness delegates with `by` in its shell, with the Python module and over MCP; envelopes bound budget, depth and harnesses; waits span processes. `by --remote` spawn, inspect, events, integrate, children and `send --json` print the same JSON as local mode; the server refuses providers, delegation and unapproved tools unless its operator allows them. See [surfaces](surfaces.md) |
| SDK engine and CLI | Hermetic: temporary git repositories and the runtime's fake ACP agent, launched through the `gemini-cli-acp` and `qwen-code-acp` profiles with a command override. 21 engine tests cover `run` with a diffstat and without changes, parallel `run_on` with planned unique names and an observer seeing both branches, `send` resuming the session, a resume the harness cannot find, ACP fork refused without a fresh session and working with one, merge onto `main`, a target that moves during the check, a failing check, conflicts, deny-by-default and rule and asked decisions in the event log, `max_turns`, a duration budget interrupting a hanging turn, a harness exiting mid-turn, descendants killed and recorded, the event log reloaded in a new `Yard`, `.branchyard/` excluded from git including from a linked worktree, `remove`, `harnesses()` and the inherited and isolated environments. 5 CLI tests run the built `by` binary: `run`, `ls --json`, `diff`, `log`, `merge`, `rm`, `fan`, `send`, `--max-minutes`, permission output without a terminal, and error exit codes. The cost budget, Claude Code and Codex profiles, and forking a session across worktrees are not exercised: the fake agent reports no cost and speaks only ACP, which cannot fork |
| Durable execution | Hermetic, on Linux: an engine process killed with SIGKILL mid-turn is recovered on the next open, its orphaned harness and that harness's child are killed by process group after a pid and start-time match, the branch ends `interrupted` with a `recovered` event, and the prompt reached the harness exactly once; a crash before submit reports that the turn never ran; two yards on one repository refuse each other's sends, merges and removals while a lease is held, and one's cancel stops the other's turn; a superseded lease fences the running turn; recovery reuses a journaled snapshot and recognises a merge cut short after moving the target; cursor reads and waits; import of JSON records and JSONL logs; cancel through `by`, HTTP and `by --remote`. See [durability](durability.md) |
| Live local mode | On 27 September 2026, `by run` with Claude Code 2.1.283 fixed a one-line bug on branch `by/fix-add` in a scratch repository. The edit went through a recorded permission decision, and the harness estimated the cost at $0.07. `by ls` and `by diff` showed the change. `by merge` ran the branch's check (`python3 test_calc.py`) in a temporary worktree, then merged into `main`. An earlier run found that inheriting the environment let the child Claude Code adopt the launching Claude Code session's identity through its remote-session variables. Local mode now removes every `CLAUDE*` variable except Claude Code configuration, and the re-run child reported its own session. |
| Git workspaces | Temporary repositories cover branch creation, snapshots, diffs, successful merges, conflicts, failing and timed-out checks, a target that moved before or during checks, dirty and clean checked-out targets, and cleanup of temporary worktrees |
| Claude Code driver | Replays a recorded Claude Code 2.1.283 stream-json session: launch, `initialize` and user frames match what the CLI accepted, and output maps to acknowledgment, session, message, cumulative usage and completion events. Permission, interrupt, limit, resume-mismatch and fork-identity cases use frames shaped by Agent SDK 0.3.283 types |
| Codex driver | Replays a recorded codex-cli 0.157.1 app-server session without credentials: all four outgoing frames equal the frames the binary accepted, and the turn ends as a failed 401. Approval, interrupt, usage, resume and fork cases use shapes from the binary's generated JSON Schema |
| Live driver qualification | `claude-code-stream-json` against Claude Code 2.1.283 and `claude-code-acp` against claude-agent-acp 0.81.2 each pass 9 of 9 scenarios: turns, permission denial and approval, interrupts during a permission wait and a tool, clean close, resume, fork (rejected as declared for ACP) and a lost connection. Local processes, not a sandbox. See [driver qualification](qualification/README.md) |
| ACP driver | Its `initialize` equals the one claude-agent-acp 0.81.2 accepted, and its response opens a session. Every outgoing frame deserializes as the official `agent-client-protocol-schema` 1.9.1 type |
| Harness identity registry | 27 harnesses; every Herdr manifest (22), Herdr resume source (18), Scion harness (9) and integration target (16) maps to exactly one ID, and Herdr's aliases agree with those mappings |
| Substrate client generation | `ateapi.proto` compiles unmodified with pinned `protoc` from `protoc-bin-vendored` |
| Substrate adapter over gRPC | Create/adopt, resume, inspect, suspend, checkpoint, branch, revert and UID-fenced destroy against an in-process fake `Control` service |
| Substrate provider through the bridge | The provider conformance checks (mount-less mode), per-attempt credential refusal, git and home transfer with the harness's commits kept and rewritten history refused, TLS on both hops with a client certificate (a wrong authority refused), the bridge reaping orphans as process 1 and running execs as another user (as root, under `unshare`), UID checks around actor operations, quiescence, and `by run --provider substrate` with the fake ACP agent through merge, recovery of a killed engine's work, and `by --remote`, against the in-process fake cluster whose actors run the real `branchyard-bridge` on the test host |
| Provisioning | Hermetic. `branchyard-provision` translates Scion's Claude, Codex, Gemini CLI, OpenCode, Copilot, Hermes and Antigravity provisioners: Scion's test cases (auth selection, managed instruction blocks, TOML helpers, Claude model aliases, Codex reasoning effort and `[otel]`, Hermes auth and Vertex regions, the telemetry cases) as unit tests; plans applied to temporary homes seeded with user files and compared with golden files; re-provisioning changes nothing; file modes; links in the home refused or replaced; unparseable user files kept; no plan contains an approval or sandbox bypass. Through the engine, the fake ACP agent run as `codex-acp` and `claude-code-acp` sees its provisioned files and variables in an isolated home and in a fake Substrate actor; the secret is found nowhere under the repository but the branch's private home, nor in the event log or the stored record; the server resolves secrets from its own table and refuses named sources and undefined secrets, over HTTP and through `by --remote`. See [provisioning](provisioning.md) |
| Rust formatting and Clippy | Formatting passes; Clippy has no warnings across all targets |
| Runnable example | `plan_resume` prints an argument vector without starting a harness |
| Scion shared helper | 59 tests pass |
| Scion telemetry provisioning | 7 tests: 6 pass, 1 skips (it needs the pinned Codex CLI 0.154.0) |
| Scion Claude provisioner | 13 tests pass |
| Scion Codex provisioner | 14 tests pass |
| Scion Copilot provisioner | 37 tests pass |
| Scion Grok Build provisioner | 88 tests pass |
| Scion Hermes provisioner | 29 tests pass |
| Scion Muse Code provisioner | 14 tests pass |

The eight Scion suites at `d9b9e6a` total **261 tests**, 260 passing and 1 skipped. These use temporary fixtures and mocked operations; they do not authenticate to model providers or launch production harness sessions. Catalog parsing does not validate every terminal regex against live harness output.

## Resolved upstream incompatibility

At Scion revision `54b9387549ea2f673376c94b5d992010ebe9100a`, the Claude test suite expected Python-side model alias resolution (`_resolve_model_alias` and shorthand constants) that the provisioner did not have: 11 of its 12 tests failed or errored, and `tools/test_scion.py --qualified` excluded the suite while `tools/check_scion_compatibility.py` held that exact baseline.

At `d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338`, the current pin, Scion's provisioner resolves aliases itself and all 13 Claude tests pass. `vendor/scion` was re-pinned to that revision for every file, `--qualified` excludes no suite, and the compatibility check's baseline records no incompatibility: it still fails if one appears, so a regression at the next pin is caught. Branchyard's Rust translation of the alias resolver carries the same test cases.

Passing its own tests does not qualify Scion's Claude provisioner, or Branchyard's translation, against Claude Code.

## Not validated

- Codex and every ACP harness other than claude-agent-acp against a live binary with credentials. The Codex recording ran without credentials. Claude recordings and qualification runs made real model calls: under about $1 in total by the harnesses' own estimates.
- Any profile inside a Branchyard sandbox: isolation, egress, credential projection and node recovery.
- Provisioning against any real harness: no native file, variable or setting it writes has been checked against the harness it targets. The paths are Scion's; see [live testing](testing-live.md#7-provisioning-model-calls).
- The ten ACP harnesses other than the Claude adapter, and the Codex ACP adapter. Their profiles use launch commands from the integration matrix and are protocol-tested only.
- Any Agent Substrate cluster. The fake `Control` service and router model documented behavior only; the router's addressing and WebSocket forwarding, identity projection on restore, TLS, authentication, tag readiness and real snapshot semantics are untested.
- Runtime isolation, KVM deployment, storage cloning, network enforcement, or startup latency.
- Any live harness session, provider credential path, ACP exchange, or native driver.
- The SDK engine and `by` against any real harness, including with the developer's own login, and whether Claude Code or Codex can resume or fork a session from another worktree.
- The sixteen-profile matrix as deployed support.
- OpenRig's application-dependent TypeScript contract or Warp's application-dependent AGPL modules as standalone binaries.
- PostgreSQL/PGMQ execution, recursive budgets, dynamic graph mutation, failover, or Git promotion.
- Recovery on macOS (the `ps -o lstart=` start-time path), across hosts, after a reboot, or of a real harness that was killed mid-turn; and SQLite store performance under a real harness's event rate.

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

`python3 tools/test_scion.py` without `--qualified` runs the same suites at this revision. CI runs the suites and the compatibility check as separate required steps.
