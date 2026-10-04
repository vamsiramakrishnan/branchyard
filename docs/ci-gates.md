# CI gates

Besides the build, the tests and Clippy (see [validation](validation.md)), `.github/workflows/check.yml` runs gates that keep the workspace's health from decaying quietly. Each is a job of its own, so a failure names what broke, and none of the network ones touches the offline `controls` job (which resolves crates once with `cargo fetch --locked` and builds with `--offline --locked`).

| Job | Fails when | Run it locally |
|---|---|---|
| `deny` | a dependency has a RustSec advisory, is unmaintained or yanked, carries a license outside `deny.toml`'s allow-list, or comes from anywhere but crates.io | `python3 tools/install_ci_tool.py cargo-deny && cargo deny --locked check` |
| `unused-deps` | a crate declares a dependency no source file names (`cargo machete`) | `python3 tools/install_ci_tool.py cargo-machete && cargo machete` |
| `docs` | `cargo doc --workspace --no-deps` warns: a broken or private intra-doc link, bad HTML in a doc comment | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline` |
| `all-features` | the workspace, with every feature at once, no longer checks | `cargo check --workspace --all-targets --all-features --locked --offline` (needs `libcap-ng-dev` for Microsandbox) |
| `coverage` | line coverage falls below `tools/coverage_floor.json`, or a new source file has no covered line | see [coverage](#coverage) |
| `python` | `ruff check`, `ruff format --check` or `mypy` fails on `tools/` or `tests/` | `pip install ruff==0.15.8 mypy==2.4.0`, then the commands in the job |
| `controls` (a step) | a dependency version is spelled in more than one crate (`tools/check_workspace_deps.py`) | `python3 tools/check_workspace_deps.py` |

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
