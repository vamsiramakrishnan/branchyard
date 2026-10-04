# Contributing

Start with [the implementation plan](docs/implementation-plan.md). The next deliverable is the domain/provider contract and sandbox qualification, followed by one complete remote task. The current crates provide the harness identity registry, unqualified harness protocol drivers with per-capability support reasons, sandbox capability admission, and an unqualified Agent Substrate adapter.

## Boundaries to preserve

- Keep the public SDK a remote client. Runtime state and harness execution belong on servers.
- Let the meta-harness propose topology at runtime. Static environment profiles do not prescribe a worker graph.
- Keep task, run, attempt, session, workspace, sandbox, and candidate identities separate.
- Reject unsupported required capabilities. Do not silently weaken isolation, permission handling, or resume semantics.
- Treat uncertain effects as unknown until reconciled. A retryable queue message does not make a model turn idempotent.
- Bind acceptance to the exact candidate, environment, and policy. Check target movement before promotion.

## Changing copied controls

`vendor/` is a pinned reference snapshot of upstream files, not a promise to stay byte-identical with upstream. `vendor.lock.json` pins every file to an upstream commit with its Git blob ID and SHA-256 as fetched; keep those pins as they are (they describe upstream, not the local copy), and keep each file's path and license.

A vendored file may carry a local patch when that is the clearest place for a fix. Record it in `vendor.patches.json` in the same change: the file's `path`, the `reason`, and the `upstream_commit` it is pinned at. `tools/verify_vendor.py` checks that every file not listed still matches its pin, and that every changed file is listed (and every listed file has changed). Prefer adaptations outside `vendor/`, with source attribution and a patch, when the code is built into Branchyard. Run the verification scripts and review [the vendoring procedure](docs/vendoring.md).

The Warp collection is AGPL source reference material and excluded from the Cargo workspace. Do not copy it into an Apache-licensed module, not even through a local patch: its files stay under `vendor/warp-agpl/`, and `tools/verify_vendor.py` fails when a Rust or Cargo file outside `vendor/` refers to that directory or a workspace member lives under `vendor/`. Do not enable vendored permission-bypass arguments or hook commands as application defaults.

## Writing tests

Shared test infrastructure lives in one crate, `crates/branchyard-testkit`, a dev-dependency only (add `branchyard-testkit = { path = "../branchyard-testkit" }` to a crate's `[dev-dependencies]`). Do not copy a helper into a test file: each test crate is built separately, so copies drift, and `tools/check_test_hygiene.py` (run in CI) fails when one comes back.

| You need | Use |
|---|---|
| To wait for something | `wait::until("what", \|\| condition)`. The check returns a `bool`, an `Option<T>` (returned when `Some`) or a `Result<T, E>` (returned when `Ok`). It polls with one 60 s default timeout, returns the moment the condition holds, and on timeout fails with what was awaited and the last value observed. `wait::until_for` takes a timeout of your own; `wait::until_with_context` appends a log to the failure; `wait::try_until_for` returns `Err(last)` instead of failing, for a step inside a retry. |
| A process to start or end | `wait::exec(pid, "name")`, `wait::gone(pid)`, `wait::alive(pid)` |
| Time itself to pass | `wait::settle("why", duration)`: a quiet period in which nothing may happen, a lease or token expiring. Never to wait *for* something: that is `wait::until`, which cannot be too short on a slow machine. |
| The fake ACP agent | `fake_agent!()` in `branchyard-cli` tests (it uses that crate's `by` binary), `fake_agent_here()` elsewhere. Built once per target directory. |
| A temporary git repository driven through `by` | `branchyard_testkit::repo!()` in `branchyard-cli` tests: a `Repo` with `git`, `by`, `ok`, `json` and `by_agent`, hermetic (no user git or Branchyard configuration), showing the command's status, stdout and stderr when it fails. Wrap it in a newtype with `Deref` for what your file adds. |
| A scratch directory | `Scratch::new("name")`, removed on drop. Not `std::env::temp_dir()` plus a counter. |
| A mock HTTP server | `MockHttp::start(\|request\| Response::json(200, &body))`, then `.url()`, `.requests()` and `.await_requests(n)`. Every connection runs on a thread whose result is joined when the server is finished or dropped, so an I/O error, a truncated request or a handler panic fails the test that owns the server. `Response::hang()` and `Response::close()` are the receiver that never answers and the one that closes without a response; `MockHttp::start_v6` returns `None` where there is no IPv6 loopback. |

`BY_TEST_TIMEOUT_SCALE=3` multiplies every wait's timeout (`wait::until` and friends, and the mock server's read timeout) by three; use it to check a test against a slow machine. It does not stretch `wait::settle`: that is time against the code under test's own clocks. A test you write must pass with it.

The hygiene check also keeps three counted ratchets in `tools/test_hygiene_ratchet.json`: calls to `wait::settle`, hand-rolled `std::env::temp_dir()` directories and hand-rolled `TcpListener::bind` servers, per file. A count above the file fails; a count below it fails until the file is lowered (`python3 tools/check_test_hygiene.py --lower`). They can only fall: when you touch one of the listed files, move it onto the kit and lower its count.

## Validation

Run the commands in [validation](docs/validation.md). Add regression tests for behavioral changes and contract failures. Keep external provider smoke tests opt-in and document the exact profile they qualify. Use immutable fixtures for protocol parsing; a fake driver cannot establish sandbox isolation.

Describe each change in terms of the developer-visible behavior, the failure condition it handles, and the evidence collected. Update the support matrix only when the corresponding profile has actually passed its gate.
