# Working in this repository

Branchyard is a Rust workspace (the `by` CLI, the delegation engine, a server), a Python SDK in `sdk/python`, and plugins for the supported harnesses in `plugins/`. This file is the short version of the rules; [CONTRIBUTING.md](CONTRIBUTING.md) has the reasons and the long form of each, and every command below exists in this checkout.

## Gates

- Before you push, run `python3 tools/check.py --fast`: the Python gates, ruff, mypy, every Python test and the docs checks, in about 40 seconds. Without `--fast` it also runs the cargo steps (`cargo fmt`, rustdoc with warnings denied, the missing-docs ratchet, clippy, the schema freshness tests). `--fix` rewrites what would fail first (formatting, generated schemas, `docs/facts.json`); `--only NAME` and `--keep-going` narrow or widen a run.
- `python3 tools/install_hooks.py` makes `git push` run the fast checks first. Do not push with `--no-verify` to get past a failing check.
- Cargo runs with `--locked --offline`, as CI does. A dependency change commits `Cargo.lock`.
- Every behaviour change ships its regression test and the docs that describe it, in the same commit. A new source file or function has a test.
- A test never sleeps for a fixed time, races a free port, or calls a model. Wait with `wait::until`; use the fake agent and fake binaries on `PATH`. A harness or sandbox code path stays hermetic in tests.
- New code propagates errors with `?`. No `unwrap`, `expect` or `panic!` outside tests, no `let _ =` on a fallible value (use `branchyard_support::best_effort`), and no `unwrap_or_else(PoisonError::into_inner)` (use `lock_recovering("name")`). Time, ids and randomness come from `branchyard-support`; a codec, an HTTP/1.1 reader or a column conversion comes from the crate CONTRIBUTING names for it.
- Public items need doc comments, and rustdoc runs with `-D warnings`: no links to private or feature-gated items, and `<placeholders>` in backticks.

## Ratchets only go down

A counted file records what is still wrong. A count may fall and may not rise, and a file may not join a list. Remove the cause; never raise a number or add an allow to get green.

- `tools/lint_ratchet.json`: `// ratchet:` allows per crate. After removing one, `python3 tools/check_lint_ratchet.py --update`.
- `tools/missing_docs_ratchet.json`: undocumented public items. `python3 tools/check_missing_docs.py --write` lowers it.
- `tools/test_hygiene_ratchet.json`: fixed waits, hand-made temp directories, hand-made servers. `python3 tools/check_test_hygiene.py --lower`.
- `tools/wire_ratchet.json` and `tools/handrolled_banlist.toml`: hand-written HTTP and codecs. Migrate a site; lower its count.
- `tools/failure_message_ratchet.json`: CLI failures still built as a bare `Failure::Message`; a new one is a `Report` with a kind and a `next:` (`crates/branchyard-cli/src/errors.rs`). `python3 tools/check_failure_messages.py --update`.
- `tools/coverage_floor.json` only goes up: `python3 tools/check_coverage.py lcov.info --unit unit.lcov --update`.

## Facts in docs come from code

- A count, list, matrix or date the code knows sits in a `<!-- fact:NAME -->...<!-- /fact -->` region. Never edit inside one. After a change that a fact reads (a crate, a test, a route, a vendored file), run `python3 tools/docs_facts.py --write` and commit what it rewrites.
- A page that opens with "Written ..." or "Prepared ..." names a date no earlier than any dated status below it (`python3 tools/check_docs.py` checks).
- A hand-typed copy of a generated list is a bug. Wrap it in a region.

## Status paragraphs say what ran

- A status paragraph names its evidence: hermetic (fakes only), replayed (recorded payloads), or live (a real CLI or service, with its version, the date and where the record is). It ends with a "Not done:" clause for what was not run.
- A harness is "run live" only with a dated record of a run with a model. Written from documentation is a different claim and is labelled so.
- A child branch's result, a subagent's "done" and your own memory of a passing run are claims. Run the check, then write what you saw.

## Shipped text is tested

- The skills in `plugins/branchyard/skills/` are embedded byte for byte (`crates/branchyard-setup/src/skills.rs`; the delegate skill is also in `crates/branchyard/src/projection.rs`). After editing one, run `cargo test -p branchyard-setup skills`, `cargo test -p branchyard projection` and `cargo test -p branchyard-cli --bin by operations`: they compare the embeds with the files, and check that every `by` command and flag that the skills, `plugins/branchyard/leaf.md`, the slash commands, `plugins/branchyard/harnesses/generic/AGENTS.md` and this file name exists.
- A new flag, subcommand or MCP tool updates the same change: `docs/surfaces.md`, the delegate skill, `plugins/branchyard/leaf.md`, the MCP instructions in `crates/branchyard-mcp/src/lib.rs`, and the hook lines if a session should hear of it. Doc comments on the clap types are `--help` text, so word them as help; generated schemas are rewritten by `python3 tools/check.py --fix`.
- A hook line (`crates/branchyard-cli/src/hook_cmd.rs`) is one fact and the command that deals with it, at most 240 characters; a test parses every command it names. Its fixtures are `crates/branchyard-cli/tests/fixtures/hooks/<id>/`.

## Building in parallel worktrees

Cargo keys artifacts by package, features and profile, not by worktree, so worktrees that share one `CARGO_TARGET_DIR` overwrite each other's libraries and `target/debug/by`. A test that reports a missing subcommand, or behaviour your change lacks, is often this.

- Use one `CARGO_TARGET_DIR` per worktree when the disk allows it.
- When it does not, share one and `touch` the `.rs` files you changed in the same command as the build, so cargo rebuilds them from your tree; take a lock (`flock`) around the touch and the build so two worktrees do not interleave.
- `crates/branchyard-setup/build.rs` embeds `plugins/branchyard` by absolute path and reruns only when that path changes, so after another worktree built the crate, its install tests read that tree's manifests (`unknown field` on a manifest you did not touch). `touch` the build script with your sources. The same goes for every `include_str!`: `crates/branchyard-controls/src/catalog.rs` embeds `catalog/harnesses.toml` and `crates/branchyard/src/harness.rs` embeds `docs/qualification/*.json`, so touch those sources too when you change what they embed, or a test binary carries another worktree's copy.
- Build the crates you need (`-p branchyard-cli`), not the workspace. Set `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0` to keep the cache small, and delete the test executables you made when you finish.
- "Blocking waiting for file lock" means another build is using the directory. Wait; do not kill it.

## Commits and pull requests

- A short imperative title, then a body that says what changed and why. Describe developer-visible behaviour, the failure it handles and the evidence.
- No "Generated by ..." or "Generated with ..." footer in a commit message or a pull request description.

## Do not

- Edit a file under `vendor/` without recording the patch in `vendor.patches.json`; `python3 tools/verify_vendor.py` fails otherwise. Do not copy anything from `vendor/warp-agpl/` into the application's own code; CONTRIBUTING says why.
- Raise a ratchet, or mark a harness "run live" without a dated record.
- Set `BRANCHYARD_BLESS` outside the catalog command that documents it.
- Print, log or commit a key or token; read your diff for one before you commit.
- Call a model from a test.
