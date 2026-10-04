# Contributing

Start with [the implementation plan](docs/implementation-plan.md). The next deliverable is the domain/provider contract and sandbox qualification, followed by one complete remote task. The workspace has <!-- fact:workspace.crate_count -->22<!-- /fact --> crates. The current crates provide the harness identity registry, unqualified harness protocol drivers with per-capability support reasons, sandbox capability admission, and an unqualified Agent Substrate adapter.

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

- `cargo test -p branchyard-support` scans every crate and fails on the poison idiom; `python3 tools/check_silent_failures.py` does the same. A bare `let _ =` on a fallible value is denied by Clippy (`let_underscore_must_use`): see [Lints and the ratchet](#lints-and-the-ratchet).

## Lints and the ratchet

The root `Cargo.toml` has a `[workspace.lints]` table and every crate says `[lints]` `workspace = true` (and `rust-version.workspace = true`, which equals the channel in `rust-toolchain.toml`). The table denies, through Clippy: `let_underscore_must_use`, `let_underscore_future`, `unwrap_used`, `expect_used`, `panic`, `unwrap_in_result`, `map_unwrap_or`, `too_many_arguments` (more than 7), `todo`, `unimplemented`, `dbg_macro`, and `unused_must_use`. `clippy.toml` lets `unwrap`, `expect` and `panic!` stand in tests (`#[test]` functions and `#[cfg(test)]` modules). Integration tests, examples and the testkit carry a file-level `#![allow(..)] // tests: ...` for the helpers Clippy does not see as tests. `missing_docs` is not in the table: it keeps its own counted ratchet (see Validation).

New code meets the table: propagate with `?`, give the error a type or a message, or, where failure is acceptable, say so with `branchyard_support::best_effort`. Code that predates the table carries a targeted `#[allow(clippy::..)] // ratchet: <crate>` (an item where practical, a whole file otherwise). `python3 tools/check_lint_ratchet.py` (CI, next to the other checks) counts those markers per crate against `tools/lint_ratchet.json`. A count may not rise, and an allow of a denied lint without a marker fails. It also checks that every crate inherits the workspace lints and that the lint table is complete.

To lower a count: delete a `// ratchet:` allow, make Clippy pass without it (`cargo clippy --workspace --all-targets --locked --offline -- -D warnings`, and with the `postgres`, `schema` and `microsandbox` features that CI lints), then run `python3 tools/check_lint_ratchet.py --update` and commit the lower `tools/lint_ratchet.json`. Narrowing an allow to the lints still needed counts as progress too; do not raise a count. A function with more than 7 parameters takes a context struct instead of an allow.

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

## Speaking HTTP/1.1: use `branchyard-wire`

Every hand-written HTTP/1.1 reader or writer goes through [`crates/branchyard-wire`](crates/branchyard-wire). It is the only place that parses a head, decides how a body is framed, decodes `Transfer-Encoding: chunked`, or splits an `http(s)` URL, and it answers malformed input with a typed `WireError` rather than a default. Do not call `httparse`, parse a chunk size, or `parse::<u64>()` a `Content-Length` anywhere else; `python3 tools/check_wire.py` (in CI) fails on the first two and counts the rest.

How to:

- **Read a request** (a server): `read_request_head(&mut reader, MAX_HEAD)?`, then `request_framing(&head.headers)?` (refuses `Content-Length` with `Transfer-Encoding`, duplicate or non-decimal lengths, an unframeable coding), then `read_body(&mut reader, framing, MAX_BODY)?`. Answer any `Err` with `400`.
- **Read a response** (a client): `read_response_head`, `response_framing(status, &headers, head_only)?`, then `Body::new(reader, framing)`, which is an `io::Read` that errors when the body is cut short or a chunk is malformed. `wire_error(&io_error)` gets the typed cause back.
- **Write a request**: `request_head(method, target, headers, content_length)?`, which refuses a header name or value that could split the head. Write chunks with `write_chunk` and end with `LAST_CHUNK`.
- **Parse a URL**: `HttpUrl::parse` (strict: no userinfo, fragment, whitespace or stray `:`), then `host_header`, `origin`, `target`. Do not split on `://` by hand.
- **Reuse the client**: to call an HTTP API from the CLI, sync or server crates, use `branchyard_client::http` (`Endpoint`, `connect`, `send`), which already speaks the codec and TLS. Do not open a raw socket for it.

To add a malformed case (a vector that must be refused, or a valid one that must be read), put it in `crates/branchyard-wire/src/corpus.rs`. The wire crate's tests, the gateway's request reader, the backend and SDK response readers and the sync mock server all run the whole corpus, so one vector tests every consumer.

`tools/wire_ratchet.json` lists what is still hand-written outside the crate, per file (header drop lists and serialisers, and two head readers: the WebSocket handshake in `branchyard-bridge` and the egress proxy in `branchyard-runtime`). A count may only fall. If you remove a site, lower its number (the check fails until you do); never raise one or add a file. Migrate a site to the wire crate instead.
## Adding a provider

A branch's provider is the closed enum `Provider` (`crates/branchyard/src/lib.rs`), and everything the engine needs to know about a variant is a method of the crate-private trait `ProviderKind`, implemented once per variant in `crates/branchyard/src/providers/`. Do not match on `Provider` anywhere else: call `provider.kind().<method>()`, or `providers::of(provider)` for an `Option<&Provider>` where `None` means local.

- To change what a provider does (a check, its recovery, its paths, its lifecycle, its destroy), edit its file in `providers/`.
- To add a provider, add `providers/<name>.rs` implementing `ProviderKind` on its options struct, the variant in `Provider` and its arm in `Provider::kind`. Then add a sample to `providers/tests.rs` (its `index` match will not compile until the variant is listed, and `the_table_covers_every_variant` fails until `samples` has an instance of it; the variant count is read from the enum itself, so nothing needs bumping) and regenerate the schemas:

  ```sh
  cargo run -p branchyard-client --features schema --example generate_contract --offline > schema/contract.json
  cargo run -p branchyard-setup --example generate_schemas --offline
  cargo run -p branchyard-server --features schema --example generate_server_config_schema --offline > schema/server.config.json
  ```
- To add a new thing the engine asks of every provider, add a method to `ProviderKind`; the compiler then lists each implementation that must answer it.

`crates/branchyard/tests/provider_seam.rs` fails when a variant is named outside `providers/`. The few surfaces that still match (the CLI and server) are a ratchet in that file: lower a count when you remove a site, never raise one. See [Adding a provider](docs/providers.md#adding-a-provider).

## Validation

Run the commands in [validation](docs/validation.md). Add regression tests for behavioral changes and contract failures. Keep external provider smoke tests opt-in and document the exact profile they qualify. Use immutable fixtures for protocol parsing; a fake driver cannot establish sandbox isolation.

Describe each change in terms of the developer-visible behavior, the failure condition it handles, and the evidence collected. Update the support matrix only when the corresponding profile has actually passed its gate.

## Use a vetted crate for a codec

Do not write a percent, hex, base64, shell-quoting, URL or tar codec by hand. Five copies of `percent_decode` once disagreed about `+` and all dropped a trailing `%41`. Call the crate, or the one shared helper built on it:

| You need | Call |
| --- | --- |
| Percent-encode or decode a URL component or query value | `branchyard_client::http::{encode, decode, decode_form, decode_form_bytes}` (the `percent-encoding` crate; call `percent_encoding` directly from a crate that cannot depend on `branchyard-client`) |
| Hex | `hex::encode`, `hex::decode` |
| Base64 | `base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}` with `base64::Engine` |
| Quote a word for a POSIX shell | `branchyard_recipe::quote` (`shlex::try_quote`) |
| Split a command line | `branchyard_setup::config::split_words` (`shlex::split`); never `split_whitespace` on text that may hold quotes |
| Parse an http(s) base URL into host, port and path | `branchyard_wire::HttpUrl::parse` (what `branchyard::models::BaseUrl::parse` uses); `url::Url` for any other URL |
| Read or write a tar | the `tar` crate, as in `crates/branchyard/src/tarball.rs` |

A crate that parses a secret-bearing URL must not return the original text when `url::Url::parse` fails: `url` rejects multi-host and empty-host URLs and cannot see a password holding a raw `/`, `#` or `?`. `branchyard::pg::redact` falls back to textual masking for those; keep its `redact_tests` cases (CI's postgres job runs them) when you touch it.

Add the dependency to the crate that needs it, prefer a package already in `Cargo.lock`, and commit the `Cargo.lock` change (CI builds with `--offline --locked`; `cargo fetch --locked` must pass).

`python3 tools/check_handrolled.py` (CI runs it, and `tests/test_check_handrolled.py`) fails on a re-implementation: `fn percent_decode`, `fn hex(bytes: &[u8])`, a base64 alphabet literal, a `'\''` quote idiom, `b"ustar"`, and the other signatures in [`tools/handrolled_banlist.toml`](tools/handrolled_banlist.toml). If a crate truly does not fit, add an `[[allow]]` row there with the file, the number of matches and the reason. The count is a ratchet: it may only fall, and the check fails when a file has fewer matches than its row says, so lower the count (or delete the row) when you remove one. To ban a new signature, add a `[[ban]]` row with its regex and what to use instead, and a case to `tests/test_check_handrolled.py`.

## Facts in the documentation come from the code

A count, list, matrix or date that the code already knows is not typed into the docs. Three mechanisms keep them true; CI runs each.

- **Fact regions.** `tools/docs_facts.py` derives facts from files (`vendor.lock.json`, the workspace manifests, the `#[test]` attributes, the derivative manifests in `patches/`, the server's routes, the runtime's stripped environment prefixes) and writes them to `docs/facts.json` and into any Markdown region marked `<!-- fact:NAME -->...<!-- /fact -->`. To show a fact in a page, put the marker around a placeholder and run `python3 tools/docs_facts.py --write`; the kinds of fact are the keys of `regions()` in the script. To add a kind, derive it in `compute()`, render it in `regions()`, and extend `tests/test_docs_facts.py`. Never edit text inside a region: `python3 tools/docs_facts.py --check` fails on any difference, and after you change the code (add a vendored file, a crate, a test) it fails until you rerun `--write` and commit the result. The test count is every `#[test]` attribute in the source, feature-gated files included, so write it as what is in the source, never as what `cargo test --workspace` runs. A hand-typed copy of a generated list (the stripped prefixes, the derivative counts) is a bug: wrap it in a region instead. Run-by-run results (a dated "N tests passed" row) are records of a run, not facts, and stay prose.
- **Routes.** The same `--check` fails when a server route is mentioned in neither [the server reference](docs/server.md) nor [surfaces](docs/surfaces.md). Document the route; `tools/docs_facts_allow.txt` is only for a route you cannot document yet, and `--check` fails once an entry becomes documented, so the list only shrinks.
- **Tables generated by Rust** keep their own freshness tests: `docs/compatibility.md` (`cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md`, tested by `cargo test -p branchyard-harness --test compatibility`) and `catalog/*.toml` (`BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog`, then review the diff). Do not set `BRANCHYARD_BLESS` outside that command; it makes the test write instead of compare.
- **Dates.** A page that opens with "Written ..." or "Prepared ..." must name a date no earlier than any `**Status, DATE.**` paragraph or dated table row below it (`tools/check_docs.py`). Update the opening line when you add a newer section.
- **Rustdoc.** Public items need docs. `tools/check_missing_docs.py` counts undocumented public items per crate against `tools/missing_docs_ratchet.json`: a crate over its count fails, and one under it fails until you lower the file with `python3 tools/check_missing_docs.py --write`, so the counts only fall. A crate at zero also carries `#![warn(missing_docs)]`; add it when you bring a crate to zero.
## CI gates

Besides tests and Clippy, CI gates the workspace's health; [CI gates](docs/ci-gates.md) says what each one fails on and how to run it. The ones you will meet:

- **A dependency.** Declare it in your crate with its version. When a second crate uses it, move it to the root `Cargo.toml`'s `[workspace.dependencies]` and write `dep = { workspace = true }` in both (add `features` and `optional` per crate). `python3 tools/check_workspace_deps.py` fails on a version spelled twice. A new license goes in `deny.toml` with the reason in the commit message; `cargo deny --locked check` and `cargo machete` (install with `python3 tools/install_ci_tool.py <tool>`) check licenses, advisories and unused dependencies.
- **A red `deny` job.** Run `cargo deny --locked check` locally. A yanked crate: `cargo update -p <crate> --precise <fixed version>` and commit `Cargo.lock`. An advisory with no fix that does not apply to us: add `{ id = "RUSTSEC-...", reason = "..." }` to `[advisories] ignore` in `deny.toml`; an entry without a reason is not accepted. If the database fetch fails locally, use a copy of `deny.toml` with `git-fetch-with-cli = true` (see [Supply chain](docs/ci-gates.md#supply-chain)).
- **A new source file or function.** Write its test in the same change. A source file with no covered line fails the coverage job. After adding tests, run the coverage commands in [CI gates](docs/ci-gates.md#coverage) and `python3 tools/check_coverage.py lcov.info --unit unit.lcov --update` to raise the floors in `tools/coverage_floor.json`, and commit the result. Floors only go up; the `uncovered` list only shrinks.
- **A doc comment.** `cargo doc` runs with `-D warnings`: do not link to private or feature-gated items with `[`...`]`, and put `<placeholders>` in backticks. Doc comments on clap types are `--help` text, and doc comments on schema types are schema descriptions; keep their wording, and use an item-level `#[allow(rustdoc::...)]` with a comment where a lint cannot apply.
- **A feature.** CI checks the workspace with `--all-features`; a feature that cannot be enabled beside the others fails there.
- **Python in `tools/` or `tests/`.** `ruff check`, `ruff format` and `mypy` (versions in `.github/workflows/check.yml`); `ruff.toml` holds the settings.
