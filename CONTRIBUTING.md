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
| Parse a base URL into host, port and path | `branchyard::models::BaseUrl::parse`, or `url::Url` |
| Read or write a tar | the `tar` crate, as in `crates/branchyard/src/tarball.rs` |

A crate that parses a secret-bearing URL must not return the original text when `url::Url::parse` fails: `url` rejects multi-host and empty-host URLs and cannot see a password holding a raw `/`, `#` or `?`. `branchyard::pg::redact` falls back to textual masking for those; keep its `redact_tests` cases (CI's postgres job runs them) when you touch it.

Add the dependency to the crate that needs it, prefer a package already in `Cargo.lock`, and commit the `Cargo.lock` change (CI builds with `--offline --locked`; `cargo fetch --locked` must pass).

`python3 tools/check_handrolled.py` (CI runs it, and `tests/test_check_handrolled.py`) fails on a re-implementation: `fn percent_decode`, `fn hex(bytes: &[u8])`, a base64 alphabet literal, a `'\''` quote idiom, `b"ustar"`, and the other signatures in [`tools/handrolled_banlist.toml`](tools/handrolled_banlist.toml). If a crate truly does not fit, add an `[[allow]]` row there with the file, the number of matches and the reason. The count is a ratchet: it may only fall, and the check fails when a file has fewer matches than its row says, so lower the count (or delete the row) when you remove one. To ban a new signature, add a `[[ban]]` row with its regex and what to use instead, and a case to `tests/test_check_handrolled.py`.
