# CI gates

Besides the build, the tests and Clippy (see [validation](validation.md)), `.github/workflows/check.yml` runs gates that keep the workspace's health from decaying quietly. Each is a job of its own, so a failure names what broke, and none of the network ones touches the offline cargo jobs (each resolves crates once with `cargo fetch --locked` and builds with `--offline --locked`).

## Lanes

A change gets one run. `check.yml` runs on a push to `main` and on a pull request (on every commit pushed to the pull request's branch); a push to a branch with a pull request open is the pull request's run, not a second one. A newer commit cancels the run it replaces.

| Lane | Workflow | Runs | Takes | What it is for |
|---|---|---|---|---|
| 0. Fast | `check.yml`, job `fast` | every change | minutes (nothing compiles) | Every Python gate and `cargo fmt`: the answer a push gets first |
| 1. Gates | `check.yml`, the other jobs | every change, in parallel with lane 0 | the one `controls` job they replaced took about 25 minutes with a warm cache; see [the split](#the-cargo-gates-in-parallel) | The cargo gates below: tests, Clippy, the schemas, PostgreSQL, features, docs, dependencies, coverage |
| 2. Nightly | `nightly.yml` | 03:23 UTC and on request | an hour or more | What a push does not need: the ignored tests a stock runner can run, and the timing-sensitive suites under load |
| Live | `qualify.yml` | Mondays and on request | | Real harnesses with real credentials ([qualification](qualification/README.md)) |
| Release | `release.yml` | a `v*` tag (drafts a release); a pull request that changes the release files, and on request (dry runs) | | Prebuilt binaries, the Python wheel and the plugin archives ([distribution](distribution.md#prebuilt-binaries)) |

Lane 0 and lane 1 start together; nothing waits for the fast lane, so a red `fast` says what is wrong within minutes while the long jobs carry on. The Python gates run once, in `fast`: no cargo job repeats them, and there is no separate `python` job.

### The cargo gates in parallel

Until October 2026 one job, `controls`, ran every cargo gate one after another: the workspace's tests (13 of its 25 minutes in a run of 25 October with a warm cache: 4 to compile, 9 to run the test binaries one at a time), then a build for the walkthrough, the undocumented-items count, Clippy, and three schema checks each followed by a Clippy pass with the schema feature (11 minutes more). Those steps now run as six jobs at once, and `controls` stays as the check that is green only when all six are:

| Job | Runs | Cache key |
|---|---|---|
| `test-engine` | `cargo test --workspace --exclude branchyard-cli --exclude branchyard-server --exclude branchyard-client --exclude branchyard-herdr`: the engine, the drivers, runtime, sandbox, provisioning, workspace, setup, controls and every other crate | `workspace-tests` (the one `nightly.yml` reads) |
| `test-cli` | `cargo test -p branchyard-cli` | `test-cli` |
| `test-server` | `cargo test -p branchyard-server -p branchyard-client -p branchyard-herdr` | `test-server` |
| `walkthrough` | builds `by` and `fake-acp-agent`, then `REQUIRE_BINARIES=1 python3 tests/test_getting_started.py` | `walkthrough` |
| `lint` | `python tools/check_missing_docs.py`, then `cargo clippy --workspace --all-targets -- -D warnings` | `lint` |
| `schema …` (three) | for `schema/contract.json`, `schema/server.config.json` and `schema/rig.json`: the crate's tests with `--features schema`, then its Clippy with the feature | `schema-<crate>` |
| `controls` | nothing of its own: it needs the six above, runs even when one failed (`if: ${{ !cancelled() }}`; a skipped job would count as passed for a required check), and fails unless every one succeeded | none |

`--exclude` keeps a crate added later in `test-engine`. A crate under test keeps the features `--workspace` gave it (resolver 2 unifies features over the packages a command selects, and the `testing` and `fake` features the engine group's crates get are the same in both); only the dependencies of `branchyard-cli` and of the server group are built with fewer additive features. `tests/test_check.py` fails when a crate is tested by two jobs or by none, when a step the single job ran is missing, when `controls` does not need every job split from it, and when a job has no timeout. Every build in the workflow runs without incremental artifacts and without debug info (`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, as the coverage job alone did before), for smaller caches and faster links; a failing test still names its file and line. `nightly.yml` sets the same, so the cache it reads fits its builds.

### What runs where, and how to run it

| Job | Fails when | Run it locally |
|---|---|---|
| `fast` | any step of `tools/check.py --fast` fails (ruff and mypy over `tools/`, `tests/`, the shipped client and `examples/python/`; vendor, catalog, docs, wire, hygiene, silent-failure and lint ratchets; schema validation; the plugin packaging; the tests of these checks; `test_sdk_examples`, which skips here because nothing is built and runs for real inside `cargo test -p branchyard-cli --test sdk_client` in `test-cli`), or `cargo fmt` would change a file | `python3 tools/check.py --fast && cargo fmt --all -- --check` (`pip install ruff==0.15.8 mypy==2.4.0` first) |
| `test-engine`, `test-cli`, `test-server` | a test of their crates fails (the catalog and `docs/compatibility.md` freshness tests are among `test-engine`'s) | `cargo test --workspace --locked --offline` runs all three; the job's own command runs one |
| `walkthrough` | a tested block of `docs/getting-started.md`, or a `by` command a guide shows, fails against the built programs | `python3 tools/check.py --only build_by,test_getting_started_built` |
| `lint` | the undocumented-items ratchet rises, or Clippy warns | `python3 tools/check.py --only check_missing_docs,cargo_clippy` |
| `schema …` | a generated schema is stale, a test of its crate fails with the `schema` feature, or Clippy warns with it | the job's two commands; `python3 tools/check.py --only schema_contract,schema_server_config,schema_rig` runs the freshness tests alone |
| `controls` | any of the six jobs above did not succeed | `python3 tools/check.py` runs the same cargo steps after the Python ones |
| `postgres` | the PostgreSQL store, engine or server fails against a `postgres:16` service | `BY_TEST_POSTGRES_URL=postgres://… cargo test -p branchyard -p branchyard-server -p branchyard-cli --features branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres --locked --offline` |
| `microsandbox` | the Microsandbox provider no longer builds, lints or passes its unit tests (its KVM tests stay ignored) | `cargo test -p branchyard-microsandbox --features microsandbox --locked --offline` (needs `libcap-ng-dev`) |
| `deny` | a dependency has a RustSec advisory, is unmaintained or yanked, carries a license outside `deny.toml`'s allow-list, or comes from anywhere but crates.io | `python3 tools/install_ci_tool.py cargo-deny && cargo deny --locked check` |
| `unused-deps` | a crate declares a dependency no source file names (`cargo machete`) | `python3 tools/install_ci_tool.py cargo-machete && cargo machete` |
| `docs` | `cargo doc --workspace --no-deps` warns: a broken or private intra-doc link, bad HTML in a doc comment | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline` |
| `all-features` | the workspace, with every feature at once, no longer checks | `cargo check --workspace --all-targets --all-features --locked --offline` (needs `libcap-ng-dev` for Microsandbox) |
| `coverage` | line coverage falls below `tools/coverage_floor.json`, or a new source file has no covered line | see [coverage](#coverage) |
| `fast` (a step) | a dependency version is spelled in more than one crate (`tools/check_workspace_deps.py`) | `python3 tools/check_workspace_deps.py` |

`tools/check.py` is the one table of the Python gates and of the cargo steps the cargo jobs and `docs` run; each cargo step names the job that runs it. The ruff and mypy targets are the table's (`TARGETS`), not a list in the workflow: a new directory of Python goes in `tools/check.py` and `ruff.toml`, nowhere in `check.yml`. `tests/test_check.py` fails when `check.yml` runs a script that table lacks, when a job that compiles has no cache, when an action is not pinned to a commit, or when a change would get two runs.

### The cache

Every job that compiles restores `Swatinem/rust-cache`, pinned to a commit like every other action (v2.9.2, `6323deb`, read from that tag in the action's own repository with `git ls-remote`; the test fails on a tag or a branch name). It keeps the registry and the dependencies' build output, not the workspace's own crates, which rebuild each time. Each job has a key of its own (`workspace-tests` for `test-engine`, `test-cli`, `test-server`, `walkthrough`, `lint`, one `schema-<crate>` per schema job, `postgres`, `microsandbox`, `docs`, `all-features`, `coverage`) and only a push to `main` writes it; a pull request reads what `main` last wrote. Jobs do not share a key: their dependency sets differ, and of several jobs saving one key only the first is kept. GitHub keeps 10 GB of caches per repository and evicts the least recently used: when `main` stops writing a key, runs get slower, never wrong. If eviction becomes constant, drop the `coverage` cache first; its instrumented build is the largest.

To bump the action: list the tags with `git ls-remote --tags https://github.com/Swatinem/rust-cache`, take the commit a tag at least two weeks old points to (the line ending `^{}`), and change every `uses:` line of it in `check.yml` and `nightly.yml` together.

### The nightly lane

`nightly.yml` has two jobs, both on a stock runner and both reading the `workspace-tests` cache (which `test-engine` writes).

- `ignored` runs every `#[ignore]` test a stock runner can run, `cargo test --workspace --tests -- --ignored` (`--tests` leaves out the doctests: a code block marked `ignore` is an example, and `--ignored` would try to compile it), and skips the rest, each skip explained in the workflow. Of the 29: 15 are the children of crash and recovery tests (their parent runs them with `--ignored --exact`; run bare they return at once, which is what the job checks); 7 need Linux with KVM and the Microsandbox `msb` runtime (`crates/branchyard-microsandbox/tests/microsandbox.rs`, behind the `microsandbox` feature, so not built); 4 need an Agent Substrate cluster (`crates/branchyard-substrate/tests/cluster.rs`, [testing live](testing-live.md)); 3 need a built Anvil (`crates/branchyard-cli/tests/anvil_e2e.rs`, run by hand with `ANVIL_BIN`). On a stock runner the Anvil three skip themselves and pass.
- `repeat` runs the suites the flakes came from (bridge, workspace, the server's signals, sync and webhook, the CLI's connectors, devex and cli, the engine's) five times with 16 test threads on a 4-core runner, so that a test that needs a quiet machine fails at night, with its pass number, and not on a pull request. Nothing is scaled with `BY_TEST_TIMEOUT_SCALE`.

Run either by hand: `cargo test --workspace --locked --offline --tests -- --ignored --skip cluster_ --skip create_bridge_template`, and the loop in the `repeat` job.

Measured, read from GitHub's runs with `gh run view`: on 25 October 2026 (run 40194210443, warm cache) the single `controls` job took 24 minutes 34 seconds, `coverage` 13 minutes 23 seconds and `fast` 1 minute 38 seconds; the nightly run of 27 October 2026 (40478758822) passed, `ignored` in 4 minutes 28 seconds and `repeat` in 11 minutes 51 seconds. Not done: the six jobs that replaced `controls` had not run on GitHub's runners when this was written (27 October 2026), so how long they take, and how the per-job caches fit in 10 GB, are not measured. Read the first week of runs before trusting an estimate.

## Dependencies are declared once

Every dependency used by more than one crate lives in the root `Cargo.toml`'s `[workspace.dependencies]`; a crate says `serde = { workspace = true, features = ["derive"] }` and adds only its own features and `optional`. A version in two crates drifts (`serde = "1"` beside `"1.0.229"`), and the hoisted one cannot. `tools/check_workspace_deps.py` fails on a version spelled in two crates, or beside a hoisted entry. Hoisting changes no resolved version: `Cargo.lock` stays as it was.

To add a dependency used in one crate, put it in that crate with its version. When a second crate needs it, move it to `[workspace.dependencies]` in the same change. A hoisted entry may only turn default features off when every user does.

## Supply chain

`deny.toml` is the policy. The licenses allowed are the ones `Cargo.lock` carries today; a new one is a decision, made in the change that brings the dependency in, with the reason in the commit message. Duplicate versions of a crate are a warning (most come from upstream), not a failure. An advisory that does not apply goes in `[advisories] ignore` with the reason, never without one. The job also catches a new advisory against an unchanged lockfile, so a red `deny` on a change that touched no dependency means a crate was reported: update it, or ignore the advisory with the reason.

Current exceptions, each in `deny.toml` with its reason: `RUSTSEC-2025-0141` (bincode 2.0.1 is unmaintained; it reaches the graph only through `microsandbox-filesystem`, behind the optional `microsandbox` feature, and upstream has no fix). Remove the entry when `microsandbox` moves off bincode.

**Patched, not ignored: zstd 0.13.** Microsandbox's `msb_krun_vmm` (every release up to 0.1.42) asks for `zstd = "0.13"`, whose `zstd-safe` 7 carries RustSec advisories RUSTSEC-2026-0391 and RUSTSEC-2026-0395 to -0406, and zstd has no fixed 0.13 release. Instead of ignoring them, the root `Cargo.toml`'s `[patch.crates-io]` puts `patches/zstd-0.13` in its place: a zstd 0.13.3 that re-exports zstd 0.14.3, on the fixed `zstd-safe` 8.2. `msb_krun_vmm` calls one function of zstd, `stream::copy_decode` (to unpack an x86_64 kernel image), whose signature 0.14 keeps, and the patch offers every zstd 0.13 feature; no `zstd-safe` 7 is built. The patch is not a workspace member. `tests/test_supply_chain.py` keeps the lockfile on it and off the vulnerable releases. Remove the patch and its `exclude` entry when `msb_krun_vmm` asks for zstd 0.14 (cargo then warns that the patch is unused).

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

The instrumented build is a second compile of the workspace; on 25 October 2026 the job took 13 minutes 23 seconds with a warm cache.
