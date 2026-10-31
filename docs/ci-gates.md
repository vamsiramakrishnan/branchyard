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
| `test-engine` | `cargo nextest run --workspace --exclude branchyard-cli --exclude branchyard-server --exclude branchyard-client --exclude branchyard-herdr --profile ci`, then `cargo test --doc` over the same packages: the engine, the drivers, runtime, sandbox, provisioning, workspace, setup, controls and every other crate | `workspace-tests` (the one `nightly.yml` reads) |
| `test-cli` | `cargo nextest run -p branchyard-cli --profile ci` (a binary crate, without doctests) | `test-cli` |
| `test-server` | `cargo nextest run -p branchyard-server -p branchyard-client -p branchyard-herdr --profile ci`, then `cargo test --doc` over the same packages | `test-server` |
| `walkthrough` | builds `by` and `fake-acp-agent`, then `REQUIRE_BINARIES=1 python3 tests/test_getting_started.py` | `walkthrough` |
| `lint` | `python tools/check_missing_docs.py`, then `cargo clippy --workspace --all-targets -- -D warnings` | `lint` |
| `schema …` (three) | for `schema/contract.json`, `schema/server.config.json` and `schema/rig.json`: the crate's tests with `--features schema`, then its Clippy with the feature | `schema-<crate>` |
| `controls` | nothing of its own: it needs the six above, runs even when one failed (`if: ${{ !cancelled() }}`; a skipped job would count as passed for a required check), and fails unless every one succeeded | none |

`--exclude` keeps a crate added later in `test-engine`. A crate under test keeps the features `--workspace` gave it (resolver 2 unifies features over the packages a command selects, and the `testing` and `fake` features the engine group's crates get are the same in both); only the dependencies of `branchyard-cli` and of the server group are built with fewer additive features. `tests/test_check.py` fails when a crate is tested by two jobs or by none, when a step the single job ran is missing, when `controls` does not need every job split from it, when a job has no timeout, and when a job runs nextest without the `ci` profile, without installing the pinned binary, or without the doctests of the packages it tests. Every build in the workflow runs without incremental artifacts and without debug info (`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, as the coverage job alone did before), for smaller caches and faster links; a failing test still names its file and line. `nightly.yml` sets the same, so the cache it reads fits its builds.

### The tests under nextest

The test jobs run [cargo-nextest](https://nexte.st) 0.9.148, a release binary pinned by SHA-256 in `tools/install_ci_tool.py` like the other CI tools, with the `ci` profile of [`.config/nextest.toml`](../.config/nextest.toml). `cargo test` runs a crate's test binaries one after another, so one binary's slowest test held the job while the other cores waited; nextest runs each test as a process of its own, from one pool of threads that every binary shares. The profile reports every failure (`fail-fast = false`) and retries none (`retries = 0`: a test that passes on a second try is a flake to fix, not a pass). A test still running after 60 seconds is reported as slow, and after 5 minutes it is killed and fails by name, where a hang used to hold the job until its timeout. Each run writes a JUnit report, `target/nextest/ci/junit.xml`, which the job uploads as `junit-<job>`.

nextest runs no doctests. Each job that tests a crate with a library then runs `cargo test --doc` with the same packages and features, so every doctest `cargo test` ran still runs; `branchyard-cli` and `branchyard-herdr` are binaries and have none.

A process per test shares no memory with another test: a `static`, a `OnceLock` or an in-process lock does not carry from one test to the next, so a test that needs another's state, or a lock across tests, fails here first. The fake agent was one such case: `branchyard_testkit::fake_agent` ran `cargo build` once per process, and a `cargo build` that builds nothing still holds the target directory's lock for about half a second, so under nextest the tests queued on it one by one (the 76 tests of `branchyard-cli`'s `cli.rs` took 111 seconds where `cargo test` took 39, on 29 October 2026, on a loaded 8-core machine). The first process of a nextest run now records the run (`NEXTEST_RUN_ID`) beside the binary it built, and the run's other processes take the binary as built. That build also leaves out the variables cargo sets for the package under test (`CARGO_MANIFEST_DIR`, `CARGO_PKG_*`, `OUT_DIR`): ring's build script tracks `CARGO_MANIFEST_DIR`, so a build started from a test, and the next one started from the job's shell, rebuilt ring and every crate above it (19 crates, about 90 seconds here, before the doctests of `test-server`). Locally, `cargo test` runs the same tests; to run a job as CI does, `python3 tools/install_ci_tool.py cargo-nextest`, then its commands, such as `cargo nextest run -p branchyard-cli --locked --offline --profile ci`.

### What runs where, and how to run it

| Job | Fails when | Run it locally |
|---|---|---|
| `fast` | any step of `tools/check.py --fast` fails (ruff and mypy over `tools/`, `tests/`, the shipped client and `examples/python/`; vendor, catalog, docs, wire, hygiene, silent-failure and lint ratchets; schema validation; the plugin packaging; the tests of these checks; `test_sdk_examples`, which skips here because nothing is built and runs for real inside `cargo test -p branchyard-cli --test sdk_client` in `test-cli`), or `cargo fmt` would change a file | `python3 tools/check.py --fast && cargo fmt --all -- --check` (`pip install ruff==0.15.8 mypy==2.4.0` first) |
| `test-engine`, `test-cli`, `test-server` | a test or doctest of their crates fails, or a test runs for 5 minutes (the catalog and `docs/compatibility.md` freshness tests are among `test-engine`'s) | `cargo test --workspace --locked --offline` runs all three; the job's own commands run one as CI does |
| `walkthrough` | a tested block of `docs/getting-started.md`, or a `by` command a guide shows, fails against the built programs | `python3 tools/check.py --only build_by,test_getting_started_built` |
| `lint` | the undocumented-items ratchet rises, or Clippy warns | `python3 tools/check.py --only check_missing_docs,cargo_clippy` |
| `schema …` | a generated schema is stale, a test of its crate fails with the `schema` feature, or Clippy warns with it | the job's two commands; `python3 tools/check.py --only schema_contract,schema_server_config,schema_rig` runs the freshness tests alone |
| `controls` | any of the six jobs above did not succeed | `python3 tools/check.py` runs the same cargo steps after the Python ones |
| `postgres 1/3` to `3/3` | the PostgreSQL store, engine or server fails against a `postgres:16` service. Three jobs, one build each, each running a third of the tests under nextest (`--partition slice:N/3`, dealt round-robin) against a database of its own; the first also runs the doctests | `BY_TEST_POSTGRES_URL=postgres://… cargo test -p branchyard -p branchyard-server -p branchyard-cli --features branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres --locked --offline` runs all three |
| `postgres-lint` | Clippy warns with the PostgreSQL features (the step the single `postgres` job ran before its tests) | `cargo clippy --workspace --all-targets --locked --offline --features branchyard/postgres,branchyard-server/postgres,branchyard-cli/postgres -- -D warnings` |
| `microsandbox` | the Microsandbox provider no longer builds, lints or passes its unit tests (its KVM tests stay ignored) | `cargo test -p branchyard-microsandbox --features microsandbox --locked --offline` (needs `libcap-ng-dev`) |
| `deny` | a dependency has a RustSec advisory, is unmaintained or yanked, carries a license outside `deny.toml`'s allow-list, or comes from anywhere but crates.io | `python3 tools/install_ci_tool.py cargo-deny && cargo deny --locked check` |
| `unused-deps` | a crate declares a dependency no source file names (`cargo machete`) | `python3 tools/install_ci_tool.py cargo-machete && cargo machete` |
| `docs` | `cargo doc --workspace --no-deps` warns: a broken or private intra-doc link, bad HTML in a doc comment | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline` |
| `all-features` | the workspace, with every feature at once, no longer checks | `cargo check --workspace --all-targets --all-features --locked --offline` (needs `libcap-ng-dev` for Microsandbox) |
| `coverage 1/3` to `3/3`, `coverage` | a test fails under coverage in a shard, line coverage falls below `tools/coverage_floor.json`, or a new source file has no covered line. Three shards, one instrumented build each, run a third of the tests (`--partition slice:N/3`) and the first also the unit tests; `coverage` joins their reports and holds the floors | see [coverage](#coverage) |
| `fast` (a step) | a dependency version is spelled in more than one crate (`tools/check_workspace_deps.py`) | `python3 tools/check_workspace_deps.py` |

`tools/check.py` is the one table of the Python gates and of the cargo steps the cargo jobs and `docs` run; each cargo step names the job that runs it. The ruff and mypy targets are the table's (`TARGETS`), not a list in the workflow: a new directory of Python goes in `tools/check.py` and `ruff.toml`, nowhere in `check.yml`. `tests/test_check.py` fails when `check.yml` runs a script that table lacks, when a job that compiles has no cache, when an action is not pinned to a commit, or when a change would get two runs.

### The cache

Every job that compiles restores `Swatinem/rust-cache`, pinned to a commit like every other action (v2.9.2, `6323deb`, read from that tag in the action's own repository with `git ls-remote`; the test fails on a tag or a branch name). It keeps the registry and the dependencies' build output, not the workspace's own crates, which rebuild each time. Each job has a key of its own (`workspace-tests` for `test-engine`, `test-cli`, `test-server`, `walkthrough`, `lint`, one `schema-<crate>` per schema job, `postgres`, `postgres-lint`, `microsandbox`, `docs`, `all-features`, `coverage`) and only a push to `main` writes it; a pull request reads what `main` last wrote. Jobs do not share a key: their dependency sets differ, and of several jobs saving one key only the first is kept. The three `postgres` partitions and the three coverage shards are the exception: each set builds the same thing, so it shares one key (`postgres`, `coverage`). GitHub keeps 10 GB of caches per repository and evicts the least recently used: when `main` stops writing a key, runs get slower, never wrong. If eviction becomes constant, drop the `coverage` cache first; its instrumented build is the largest.

To bump the action: list the tags with `git ls-remote --tags https://github.com/Swatinem/rust-cache`, take the commit a tag at least two weeks old points to (the line ending `^{}`), and change every `uses:` line of it in `check.yml` and `nightly.yml` together.

### The nightly lane

`nightly.yml` has two jobs, both on a stock runner and both reading the `workspace-tests` cache (which `test-engine` writes).

- `ignored` runs every `#[ignore]` test a stock runner can run, `cargo test --workspace --tests -- --ignored` (`--tests` leaves out the doctests: a code block marked `ignore` is an example, and `--ignored` would try to compile it), and skips the rest, each skip explained in the workflow. Of the 29: 15 are the children of crash and recovery tests (their parent runs them with `--ignored --exact`; run bare they return at once, which is what the job checks); 7 need Linux with KVM and the Microsandbox `msb` runtime (`crates/branchyard-microsandbox/tests/microsandbox.rs`, behind the `microsandbox` feature, so not built); 4 need an Agent Substrate cluster (`crates/branchyard-substrate/tests/cluster.rs`, [testing live](testing-live.md)); 3 need a built Anvil (`crates/branchyard-cli/tests/anvil_e2e.rs`, run by hand with `ANVIL_BIN`). On a stock runner the Anvil three skip themselves and pass.
- `repeat` runs the suites the flakes came from (bridge, workspace, the server's signals, sync and webhook, the CLI's connectors, devex and cli, the engine's) five times with 16 test threads on a 4-core runner, so that a test that needs a quiet machine fails at night, with its pass number, and not on a pull request. Nothing is scaled with `BY_TEST_TIMEOUT_SCALE`.

Run either by hand: `cargo test --workspace --locked --offline --tests -- --ignored --skip cluster_ --skip create_bridge_template`, and the loop in the `repeat` job.

Measured, read from GitHub's runs with `gh run view`: on 25 October 2026 (run 40194210443, warm cache) the single `controls` job took 24 minutes 34 seconds, `coverage` 13 minutes 23 seconds and `fast` 1 minute 38 seconds; the nightly run of 27 October 2026 (40478758822) passed, `ignored` in 4 minutes 28 seconds and `repeat` in 11 minutes 51 seconds. Not done: the six jobs that replaced `controls` had not run on GitHub's runners when this was written (27 October 2026), so how long they take, and how the per-job caches fit in 10 GB, are not measured. Read the first week of runs before trusting an estimate.

Before nextest, on 29 October 2026 (run 40963883962, a pull request with warm caches, read with `gh api`): `coverage` took 15 minutes 42 seconds (852 s in `cargo llvm-cov --workspace`), `postgres` 14 minutes 48 seconds (150 s of Clippy, then 705 s of tests), `test-engine` 5 minutes 12 seconds, `test-cli` 4 minutes 57 seconds and `test-server` 3 minutes 54 seconds. The tests under nextest, hermetic and local only: from 29 to 31 October 2026, on a shared 8-core machine under other load (load averages from 7 to 28), in an environment stripped to what a runner has, with each job's packages and features, `cargo test` and then nextest ran `test-cli`'s 730 tests in 270 and 140 seconds (398 and 228 in a second run), `test-server`'s 444 tests and its doctest in 206 and 51 seconds (250 and 73), and `test-engine`'s 1860 tests and 5 doctests in 458 and 552 seconds under the heaviest load (695 and 489 in the second run). Both runners ran the same tests. Under that load, tests with budgets of a few seconds failed under either runner: four of `branchyard`'s under nextest in the first run, which passed under nextest in the second, and `fleet`'s `a_failover_keeps_the_entrys_limits` under `cargo test` alone. `branchyard-testkit`'s `hermetic_clears_every_branchyard_variable` failed under both in the stripped environment: with no variable sorting before `BRANCHYARD_`, the child's first line follows libtest's `test ... ` prefix and is not read. Not done: the nextest jobs had not run on GitHub's runners when this was written (31 October 2026); read their first runs before trusting these local ratios.

The `postgres 1/3` to `3/3` partitions and the coverage shards, measured the same way. Against a local PostgreSQL 16.15 (fsync off; CI's is a container on the runner's disk), the three postgres slices of 717 or 718 tests took 464, 287 and 347 seconds on 30 October 2026, and the whole PostgreSQL suite, 2152 tests and a doctest, took 2727 seconds under `cargo test` with 24 timing failures and 1386 under nextest with 2, both of which also failed under `cargo test`, on 31 October. In one slice `graph`'s `a_child_whose_engine_died_after_it_wrote_its_work_recovers_ready` hung and was killed at 5 minutes; it passed in four other runs. For coverage, on `branchyard-provision` and `branchyard-sync` (63 files), `cargo llvm-cov` and `cargo llvm-cov nextest` reported the same lines found and hit in every file, and so did three slices joined line by line, where joined bare one file's lines came out wrong (378 found where there are 77); the unit report of the coverage job gave its 146 files the same lines found, 6 and 4 more lines hit under nextest in two files, and the three watched files the same coverage. A process per test writes a profile per process: about 0.6 MB each, about two per test. Not done: the whole workspace's coverage under nextest was stopped at 896 of 3034 tests after 20 minutes on the loaded machine, and none of this had run on GitHub's runners when this was written (31 October 2026).

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
python3 tools/install_ci_tool.py cargo-nextest
cargo llvm-cov nextest --workspace --locked --offline --profile ci --lcov --output-path lcov.info
cargo llvm-cov nextest -p branchyard -p branchyard-provision -p branchyard-substrate \
    --lib --locked --offline --profile ci --lcov --output-path unit.lcov
python3 tools/check_coverage.py lcov.info --unit unit.lcov
```

The tests run under nextest with its `ci` profile, as in the test jobs. Coverage counts no doctest, as before: `cargo llvm-cov` runs none without `--doctests`, and `cargo llvm-cov nextest` runs none at all, so the report still compares like with like against `tools/coverage_floor.json`. CI runs the first command in three shards (`coverage 1/3` to `3/3`), each with `--partition slice:N/3` and its own report, `lcov-N.info`, and the second in the first shard. The `coverage` job joins the three reports one after another into `lcov.info`, ending each report's last line first (cargo llvm-cov leaves it without a newline, and joined bare its last `end_of_record` swallows the next report's first `SF:` line), then runs the floor check. `check_coverage.py` counts a line hit when any record of its file hits it, and every shard reports every file, since nextest runs each test binary to list its tests, so the joined report is the whole run's.

**Floors come from CI, never from a local run.** A local run (as root, with namespaces and `/dev/kvm`) exercises tests the hosted runner skips, so floors measured there fail in CI (runtime and sandbox did, in PR #12). Seed and update floors only from the `lcov` artifact of the `coverage` job (the joined `lcov.info` and `unit.lcov`), which it uploads even when the floor check fails: download it from the run, unpack `lcov.info` and `unit.lcov`, and run `python3 tools/check_coverage.py lcov.info --unit unit.lcov --update --from-ci`. The runner's absolute `SF:` paths (`/home/runner/work/...`) are read from their first `crates/<name>/src/`, so no `--root` is needed, and `--update` and `--seed` refuse a report that contains no crate source file. Without `--from-ci` (or `GITHUB_ACTIONS`), `--update` and `--seed` refuse; `--local` overrides the guard for a throwaway file and warns.

After adding tests, raise the floors with that command: floors only go up, covered files leave `uncovered`, and nothing is ever added to it. Commit the result with the tests. If a floor must go down (a module was deleted, or tests were moved), edit the JSON by hand and say why in the commit message; the diff is the review.

The instrumented build is a second compile of the workspace; on 25 October 2026 the job took 13 minutes 23 seconds with a warm cache.
