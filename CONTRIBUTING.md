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
