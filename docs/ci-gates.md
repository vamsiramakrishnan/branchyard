# CI gates

Besides the build, the tests and Clippy (see [validation](validation.md)), `.github/workflows/check.yml` runs gates that keep the workspace's health from decaying quietly. Each is a job of its own, so a failure names what broke, and none of the network ones touches the offline `controls` job (which resolves crates once with `cargo fetch --locked` and builds with `--offline --locked`).

## Lanes

A change gets one run. `check.yml` runs on a push to `main` and on a pull request (on every commit pushed to the pull request's branch); a push to a branch with a pull request open is the pull request's run, not a second one. A newer commit cancels the run it replaces.

| Lane | Workflow | Runs | Takes | What it is for |
|---|---|---|---|---|
| 0. Fast | `check.yml`, job `fast` | every change | minutes (nothing compiles) | Every Python gate and `cargo fmt`: the answer a push gets first |
| 1. Gates | `check.yml`, the other jobs | every change, in parallel with lane 0 | `controls` was 28 to 30 minutes without a cache | The cargo gates below: tests, Clippy, the schemas, PostgreSQL, features, docs, dependencies, coverage |
| 2. Nightly | `nightly.yml` | 03:23 UTC and on request | an hour or more | What a push does not need: the ignored tests a stock runner can run, and the timing-sensitive suites under load |
| Live | `qualify.yml` | Mondays and on request | | Real harnesses with real credentials ([qualification](qualification/README.md)) |
| Release | `release.yml` | a `v*` tag | | Prebuilt binaries ([distribution](distribution.md)) |

Lane 0 and lane 1 start together; nothing waits for the fast lane, so a red `fast` says what is wrong within minutes while the long jobs carry on. The Python gates run once, in `fast`: `controls` no longer repeats them, and there is no separate `python` job.

### What runs where, and how to run it

| Job | Fails when | Run it locally |
|---|---|---|
| `fast` | any step of `tools/check.py --fast` fails (ruff and mypy over `tools/`, `tests/`, the shipped client and `examples/python/`; vendor, catalog, docs, wire, hygiene, silent-failure and lint ratchets; schema validation; the plugin packaging; the tests of these checks; `test_sdk_examples`, which skips here because nothing is built and runs for real inside `cargo test -p branchyard-cli --test sdk_client` in `controls`), or `cargo fmt` would change a file | `python3 tools/check.py --fast && cargo fmt --all -- --check` (`pip install ruff==0.15.8 mypy==2.4.0` first) |
| `controls` | a workspace test fails (the catalog and `docs/compatibility.md` freshness tests are among them); the undocumented-items ratchet rises; Clippy warns; a generated schema is stale | `python3 tools/check.py` runs the same cargo steps after the Python ones; or `cargo test --workspace --locked --offline` and `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` |
| `postgres` | the PostgreSQL store, engine or server fails against a `postgres:16` service | `BY_TEST_POSTGRES_URL=postgres://… cargo test -p branchyard -p branchyard-server -p branchyard-cli --features branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres --locked --offline` |
| `microsandbox` | the Microsandbox provider no longer builds, lints or passes its unit tests (its KVM tests stay ignored) | `cargo test -p branchyard-microsandbox --features microsandbox --locked --offline` (needs `libcap-ng-dev`) |
| `deny` | a dependency has a RustSec advisory, is unmaintained or yanked, carries a license outside `deny.toml`'s allow-list, or comes from anywhere but crates.io | `python3 tools/install_ci_tool.py cargo-deny && cargo deny --locked check` |
| `unused-deps` | a crate declares a dependency no source file names (`cargo machete`) | `python3 tools/install_ci_tool.py cargo-machete && cargo machete` |
| `docs` | `cargo doc --workspace --no-deps` warns: a broken or private intra-doc link, bad HTML in a doc comment | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline` |
| `all-features` | the workspace, with every feature at once, no longer checks | `cargo check --workspace --all-targets --all-features --locked --offline` (needs `libcap-ng-dev` for Microsandbox) |
| `coverage` | line coverage falls below `tools/coverage_floor.json`, or a new source file has no covered line | see [coverage](#coverage) |
| `fast` (a step) | a dependency version is spelled in more than one crate (`tools/check_workspace_deps.py`) | `python3 tools/check_workspace_deps.py` |

`tools/check.py` is the one table of the Python gates and of the cargo steps `controls` and `docs` run. The ruff and mypy targets are the table's (`TARGETS`), not a list in the workflow: a new directory of Python goes in `tools/check.py` and `ruff.toml`, nowhere in `check.yml`. `tests/test_check.py` fails when `check.yml` runs a script that table lacks, when a job that compiles has no cache, when an action is not pinned to a commit, or when a change would get two runs.

### The cache

Every job that compiles restores `Swatinem/rust-cache`, pinned to a commit like every other action (v2.9.2, `6323deb`, read from that tag in the action's own repository with `git ls-remote`; the test fails on a tag or a branch name). It keeps the registry and the dependencies' build output, not the workspace's own crates, which rebuild each time. Each job has a key of its own (`workspace-tests`, `postgres`, `microsandbox`, `docs`, `all-features`, `coverage`) and only a push to `main` writes it; a pull request reads what `main` last wrote. GitHub keeps 10 GB of caches per repository and evicts the least recently used: when `main` stops writing a key, runs get slower, never wrong. If eviction becomes constant, drop the `coverage` cache first; its instrumented build is the largest.

To bump the action: list the tags with `git ls-remote --tags https://github.com/Swatinem/rust-cache`, take the commit a tag at least two weeks old points to (the line ending `^{}`), and change every `uses:` line of it in `check.yml` and `nightly.yml` together.

### The nightly lane

`nightly.yml` has two jobs, both on a stock runner and both reading the `workspace-tests` cache.

- `ignored` runs every `#[ignore]` test a stock runner can run, `cargo test --workspace --tests -- --ignored` (`--tests` leaves out the doctests: a code block marked `ignore` is an example, and `--ignored` would try to compile it), and skips the rest, each skip explained in the workflow. Of the 29: 15 are the children of crash and recovery tests (their parent runs them with `--ignored --exact`; run bare they return at once, which is what the job checks); 7 need Linux with KVM and the Microsandbox `msb` runtime (`crates/branchyard-microsandbox/tests/microsandbox.rs`, behind the `microsandbox` feature, so not built); 4 need an Agent Substrate cluster (`crates/branchyard-substrate/tests/cluster.rs`, [testing live](testing-live.md)); 3 need a built Anvil (`crates/branchyard-cli/tests/anvil_e2e.rs`, run by hand with `ANVIL_BIN`). On a stock runner the Anvil three skip themselves and pass.
- `repeat` runs the suites the flakes came from (bridge, workspace, the server's signals, sync and webhook, the CLI's connectors, devex and cli, the engine's) five times with 16 test threads on a 4-core runner, so that a test that needs a quiet machine fails at night, with its pass number, and not on a pull request. Nothing is scaled with `BY_TEST_TIMEOUT_SCALE`.

Run either by hand: `cargo test --workspace --locked --offline --tests -- --ignored --skip cluster_ --skip create_bridge_template`, and the loop in the `repeat` job.

Not done: neither nightly job has run on GitHub's runners, and the times of lane 0, lane 1 and the cache are not measured. The figures above are the last ones taken without a cache (28 to 30 minutes for `controls`) and the fast lane's 2 minutes 43 seconds on a development machine. Read the first week of runs before trusting any of them.

## Dependencies are declared once

Every dependency used by more than one crate lives in the root `Cargo.toml`'s `[workspace.dependencies]`; a crate says `serde = { workspace = true, features = ["derive"] }` and adds only its own features and `optional`. A version in two crates drifts (`serde = "1"` beside `"1.0.229"`), and the hoisted one cannot. `tools/check_workspace_deps.py` fails on a version spelled in two crates, or beside a hoisted entry. Hoisting changes no resolved version: `Cargo.lock` stays as it was.

To add a dependency used in one crate, put it in that crate with its version. When a second crate needs it, move it to `[workspace.dependencies]` in the same change. A hoisted entry may only turn default features off when every user does.

## Supply chain

`deny.toml` is the policy. The licenses allowed are the ones `Cargo.lock` carries today; a new one is a decision, made in the change that brings the dependency in, with the reason in the commit message. Duplicate versions of a crate are a warning (most come from upstream), not a failure. An advisory that does not apply goes in `[advisories] ignore` with the reason, never without one. The job also catches a new advisory against an unchanged lockfile, so a red `deny` on a change that touched no dependency means a crate was reported: update it, or ignore the advisory with the reason.

Current exceptions, each in `deny.toml` with its reason: `RUSTSEC-2025-0141` (bincode 2.0.1 is unmaintained; it reaches the graph only through `microsandbox-filesystem`, behind the optional `microsandbox` feature, and upstream has no fix). Remove the entry when `microsandbox` moves off bincode.

A yanked release is fixed in the lockfile, not ignored: `cargo update -p <crate> --precise <newer version>`, commit `Cargo.lock`, and re-run `cargo deny --locked check`. If the advisory database fetch fails locally (a proxy that blocks libgit2), run the check against a copy of `deny.toml` with `git-fetch-with-cli = true` under `[advisories]`: `cargo deny --locked check --config /path/to/copy.toml`. Do not commit that setting; CI does not need it.

The CI tools are release binaries pinned by SHA-256 in `tools/install_ci_tool.py` rather than `cargo install`, so installing them is fast and a replaced asset fails the job. To bump one, change its version, URL and digest together.

## Coverage

`cargo llvm-cov` measures line coverage over each crate's `src/` files; tests, examples, build scripts and vendored code do not count. `tools/check_coverage.py` compares the report with `tools/coverage_floor.json`:

- `crates`: each crate's floor. Falling more than `tolerance` (1 point, because timing-dependent tests vary a little between runs) below it fails.
- `files`: files watched on their own, because a regression there hides in the crate's total.
- `unit_files`: files whose floor is measured from the unit tests alone (`--lib`), so an integration test cannot stand in for the unit tests a module should have.
- `uncovered`: source files no test reaches. A file with no covered line that is not listed fails (a new module needs a test), and a listed file that gains coverage must leave the list and take a floor under `files`. The list can only shrink.

Run it as the job does:

```sh
rustup component add llvm-tools
python3 tools/install_ci_tool.py cargo-llvm-cov
cargo llvm-cov --workspace --locked --offline --lcov --output-path lcov.info
cargo llvm-cov -p branchyard -p branchyard-provision -p branchyard-substrate \
    --lib --locked --offline --lcov --output-path unit.lcov
python3 tools/check_coverage.py lcov.info --unit unit.lcov
```

**Floors come from CI, never from a local run.** A local run (as root, with namespaces and `/dev/kvm`) exercises tests the hosted runner skips, so floors measured there fail in CI (runtime and sandbox did, in PR #12). Seed and update floors only from the `lcov` artifact of the `coverage` job, which uploads it even when the floor check fails: download it from the run, unpack `lcov.info` and `unit.lcov`, and run `python3 tools/check_coverage.py lcov.info --unit unit.lcov --update --from-ci`. The runner's absolute `SF:` paths (`/home/runner/work/...`) are read from their first `crates/<name>/src/`, so no `--root` is needed, and `--update` and `--seed` refuse a report that contains no crate source file. Without `--from-ci` (or `GITHUB_ACTIONS`), `--update` and `--seed` refuse; `--local` overrides the guard for a throwaway file and warns.

After adding tests, raise the floors with that command: floors only go up, covered files leave `uncovered`, and nothing is ever added to it. Commit the result with the tests. If a floor must go down (a module was deleted, or tests were moved), edit the JSON by hand and say why in the commit message; the diff is the review.

The instrumented build is a second compile of the workspace; expect the job to take as long as `controls`.
