# Contributing

Start with [the implementation plan](docs/implementation-plan.md). The next deliverable is the domain/provider contract and sandbox qualification, followed by one complete remote task. The current crates provide the harness identity registry, unqualified harness protocol drivers with per-capability support reasons, sandbox capability admission, and an unqualified Agent Substrate adapter.

## Boundaries to preserve

- Keep the public SDK a remote client. Runtime state and harness execution belong on servers.
- Let the meta-harness propose topology at runtime. Static environment profiles do not prescribe a worker graph.
- Keep task, run, attempt, session, workspace, sandbox, and candidate identities separate.
- Reject unsupported required capabilities. Do not silently weaken isolation, permission handling, or resume semantics.
- Treat uncertain effects as unknown until reconciled. A retryable queue message does not make a model turn idempotent.
- Bind acceptance to the exact candidate, environment, and policy. Check target movement before promotion.

## Failures that may be ignored

Never write `let _ = fallible();` or `.lock().unwrap_or_else(|e| e.into_inner())`. When a failure is acceptable (cleanup on an error path, a `Drop`, a process that may already be gone), say so with [`branchyard-support`](crates/branchyard-support/src/lib.rs), which logs it with a name so it is seen. Add `branchyard-support = { path = "../branchyard-support" }` to the crate and use:

| You want | Use |
| --- | --- |
| to survive a failing step, and get its value if it works | `branchyard_support::best_effort("what you were doing", step())` (or `best_effort!`); returns `Option<T>` and logs a `warn` on `Err` |
| to remove a temp directory or file | `cleanup_dir(path)`, `cleanup_file(path)` (already gone is fine) |
| to stop a process or a process group | `kill_process(pid)`, `kill_group(pgid)`, `terminate_group(pgid)` |
| to take a `Mutex` | `use branchyard_support::LockExt as _;` then `lock.lock_recovering("name")`; likewise `RwLockExt` (`read_recovering`, `write_recovering`) and `CondvarExt` (`wait_timeout_recovering`, ...) |
| to wait for a thread you do not need a result from | `join_reporting("what the thread did", handle)` |
| a thread whose panic should reach a log | `spawn_named(name, |panic| record(panic), body)`; the sink gets the panic message (the delegation engine writes it to the branch's event log) |

Name the lock after what it guards (`"claims"`, `"conn"`), not its type. Nothing in the crate panics, so all of it is safe in a `Drop`. A test that must show a failure was logged uses `branchyard_support::testing::capture` (feature `testing`, as a dev-dependency): see `a_failed_release_on_drop_is_logged_not_lost` in `crates/branchyard/src/state.rs`.

Two checks enforce this, in CI and locally:

- `cargo test -p branchyard-support` scans every crate and fails on the poison idiom; `python3 tools/check_silent_failures.py` does the same and also counts bare `let _ =` in non-test code per crate against `tools/silent_failures.json`. A count may not rise. When you remove some, run `python3 tools/check_silent_failures.py --update` and commit the lower baseline, so the gain is kept. Do not raise it: convert the new site instead. Where discarding really is right (a `write!` to a closed pipe, a `send` to a receiver that hung up), a `let _ =` is allowed only if the baseline is raised for that crate in the same change and the line says why.

## Time, ids and randomness

Do not read the system clock, split a date, or write a random generator yourself. [`branchyard-support`](crates/branchyard-support/src/lib.rs) has one of each, on `jiff` and `getrandom`; add `branchyard-support = { path = "../branchyard-support" }` to the crate if it is not there. Times are milliseconds since the epoch in a `u64`, as everywhere in Branchyard.

| You want | Use |
| --- | --- |
| the time now | `branchyard_support::time::now_ms()` (seconds: `now_ms() / 1000`; a unique-name salt: `now_nanos()`); a clock set before 1970 reads as 0 and logs once |
| the time of a file | `time::system_time_ms(metadata.modified()?)` |
| to write a time | `rfc3339(ms)` (`2026-09-26T12:34:56.789Z`), `rfc3339_secs(ms)`, `utc_minute(ms)` (`2026-10-01 14:30 UTC`), `amz_date(ms)`, `http_date(ms)` |
| to read a time | `parse_rfc3339(text)` or `parse_http_date(text)`; both return `Result<u64, TimeError>`, so a malformed time is an error with its text, not a silent `None` (`.ok()` where you really mean "absent") |
| the start of the UTC day and month | `period_starts(ms)` |
| a duration (`30s`, `5m`, `1.5h`, `7d`) | `parse_duration(text)` and `human_duration(d)` |
| a seeded generator, the same sequence on every platform and release | `rng::SplitMix64::new(seed)` (`next_u64`, `below`, `uniform`); `rng::splitmix64(state)` is a `const fn` for fixed tables |
| a seed that differs on every run | `rng::fresh_seed()` |
| random bytes, with the failure | `rng::fill_random(&mut buf)?` (returns `EntropyError`; it converts into `std::io::Error`) |
| an id that sorts by creation time | `branchyard_support::new_ulid()?` (fails, rather than minting an id from zeros, when the system generator does) |

A seeded generator is `SplitMix64` and not `rand`'s `SmallRng` on purpose: a seeded route, a retry schedule in a test and the chunk table in `tasks/large.rs` promise to repeat, and `rand` does not promise its stream across versions. Keys and tokens do not come from here; they use `ring`.

`cargo test -p branchyard-support` (test `one_clock`) fails on a crate that spells the calendar or SplitMix64 constants, defines its own `now_ms`, `now_secs` or `unix_now`, or names `SystemTime::now` or `UNIX_EPOCH`. There are no exceptions to ratchet: the count is zero. A new date format goes into `time.rs` with a row in the golden table of `crates/branchyard-support/tests/clock.rs` (produced by an independent implementation, not by the code under test), so a format that drifts fails there before it reaches a stored file.

## Changing copied controls

`vendor/` is a pinned reference snapshot of upstream files, not a promise to stay byte-identical with upstream. `vendor.lock.json` pins every file to an upstream commit with its Git blob ID and SHA-256 as fetched; keep those pins as they are (they describe upstream, not the local copy), and keep each file's path and license.

A vendored file may carry a local patch when that is the clearest place for a fix. Record it in `vendor.patches.json` in the same change: the file's `path`, the `reason`, and the `upstream_commit` it is pinned at. `tools/verify_vendor.py` checks that every file not listed still matches its pin, and that every changed file is listed (and every listed file has changed). Prefer adaptations outside `vendor/`, with source attribution and a patch, when the code is built into Branchyard. Run the verification scripts and review [the vendoring procedure](docs/vendoring.md).

The Warp collection is AGPL source reference material and excluded from the Cargo workspace. Do not copy it into an Apache-licensed module, not even through a local patch: its files stay under `vendor/warp-agpl/`, and `tools/verify_vendor.py` fails when a Rust or Cargo file outside `vendor/` refers to that directory or a workspace member lives under `vendor/`. Do not enable vendored permission-bypass arguments or hook commands as application defaults.

## Validation

Run the commands in [validation](docs/validation.md). Add regression tests for behavioral changes and contract failures. Keep external provider smoke tests opt-in and document the exact profile they qualify. Use immutable fixtures for protocol parsing; a fake driver cannot establish sandbox isolation.

Describe each change in terms of the developer-visible behavior, the failure condition it handles, and the evidence collected. Update the support matrix only when the corresponding profile has actually passed its gate.
