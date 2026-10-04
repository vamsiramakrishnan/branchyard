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

## Storing values in a state store

Anything a store keeps in a column (SQLite, PostgreSQL, the server's operation, trigger and companion stores) converts through [`crates/branchyard/src/store_codec.rs`](crates/branchyard/src/store_codec.rs), re-exported to the server as `branchyard::store_codec`. Both databases hold signed 64-bit integers; the code holds unsigned ones. Do not write `as i64`, `as u64`, `as u32`, `as u16`, `as i32`, `as usize`, `i64::try_from(x).unwrap_or(i64::MAX)` or `u32::try_from(x).unwrap_or(0)`: they turn a value the column cannot hold into a different value (a saturated timestamp, PID `0`, a negative stored PID wrapped into someone else's). Instead:

```rust
params![name, to_db("created_ms", record.created_ms)?]      // u64 -> i64, Err naming the column
created_ms: from_db("created_ms", row.get(3)?)?             // i64 -> u64, Err on a negative
pid: from_db_u32("pid", row.get(6)?)?                       // also from_db_u16, from_db_i32, from_db_usize, from_db_opt, to_db_opt, to_db_usize, to_usize
let expires = to_db("expires_ms", deadline(now, ttl))?;     // a lease's end: an absurd TTL is an error
let until = deadline_capped(now, wait);                     // a wait or limit that may mean "forever"
after: parse_text("after", &text)?                          // stored text -> enum, Err on unknown
```

A `wait`, `max_duration` or similar whose huge value (`Duration::MAX`, an enormous `wait_seconds`) means "no deadline in practice" must not be stored with `now.saturating_add(...)`: that is `u64::MAX`, which `to_db` refuses, so the call would fail after it had already started. Use `deadline_capped`, which caps at `FOREVER_MS` (`i64::MAX`, storable), and do not use it for a lease TTL, where an absurd value should still fail. When the value is read in a PostgreSQL `self.with(|c| ...)` closure, read the raw `i64` out of the closure and convert it after, since `?` there can only return `postgres::Error`.

`?` converts the error into `branchyard::Error`, `std::io::Error` and `rusqlite::Error`, so these work in engine code, row-mapping closures and the services registry. In PostgreSQL's `self.query(|c| ...)` / `self.with(|c| ...)` closures (which return `postgres::Error`) compute `to_db(...)?` before the closure and pass the `i64`.

Enum text stored in a column comes from the enum, not a hand-written `match` with a default arm: derive `strum::Display`, `strum::EnumString`, `strum::EnumIter` and `strum::IntoStaticStr` with `#[strum(serialize_all = "snake_case")]` next to serde's `rename_all = "snake_case"`, write it with `Display` (or `<&'static str>::from`) and read it with `parse_text`. A test in `store_codec.rs` checks that strum's text equals serde's for each such enum; add yours there.

When you add a table or column, add it to **both** `sqlite.rs` (`SCHEMA`) and `pg.rs` (`TABLES`/`STEPS`); `schema_parity.rs` fails otherwise, and a real difference goes in its `ALLOWED` list with the reason. Test a new column's edge in `conformance.rs` (it runs on SQLite always, and on PostgreSQL when `BY_TEST_POSTGRES_URL` is set): see `limits` and `corrupt`. `store_codec.rs` also holds a guard that fails on a lossy conversion in the store files and a per-file ratchet (`CAST_BUDGET`) of the `as i64`/`as u64`/`as u32`/`as u16`/`as i32`/`as usize` casts left in each guarded file, each budget justified in a comment; lower a number when you remove a cast, never raise it. A stored PID, port, turn, count or priority is never one of the casts that are left.

## Validation

Run the commands in [validation](docs/validation.md). Add regression tests for behavioral changes and contract failures. Keep external provider smoke tests opt-in and document the exact profile they qualify. Use immutable fixtures for protocol parsing; a fake driver cannot establish sandbox isolation.

Describe each change in terms of the developer-visible behavior, the failure condition it handles, and the evidence collected. Update the support matrix only when the corresponding profile has actually passed its gate.
