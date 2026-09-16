# Contributing

Start with [the implementation plan](docs/implementation-plan.md). Durable admission is implemented. The next deliverables are sandbox qualification, fenced execution ownership and one complete remote task. Read [the SDK guide](docs/sdk.md) and [control API obligations](docs/control-api.md).

## Boundaries to preserve

- Keep the public SDK a remote client. Runtime state and harness execution belong on servers.
- Let the meta-harness propose topology at runtime. Static environment profiles do not prescribe a worker graph.
- Keep task, run, attempt, session, workspace, sandbox, and candidate identities separate.
- Reject unsupported required capabilities. Do not silently weaken isolation, permission handling, or resume semantics.
- Treat uncertain effects as unknown until reconciled. A retryable queue message does not make a model turn idempotent.
- Bind acceptance to the exact candidate, environment, and policy. Check target movement before promotion.

## Extending the caller surface

Keep one operation contract in `branchyard-protocol`; SDK, CLI and future tools share it. Regenerate `schema/contract.json` using `branchyard describe` when types change. Preserve strict version negotiation and uncertain-outcome behavior. The canonical skill lives in `plugins/branchyard/skills/branchyard`; generate standalone copies with `tools/package.py`. Run extraction tests from an empty directory, not only the source checkout. Never add local execution as a client fallback.

## Changing copied controls

Do not edit files under `vendor/`. Preserve their paths, licenses, and recorded hashes. Place adaptations outside that directory with source attribution and a patch. Run the verification scripts and review [the vendoring procedure](docs/vendoring.md).

The Warp collection is AGPL source reference material and excluded from the Cargo workspace. Do not copy it into an Apache-licensed module. Do not enable vendored permission-bypass arguments or hook commands as application defaults.

## Validation

Run the commands in [validation](docs/validation.md). Add regression tests for behavioral changes and contract failures. Keep external provider smoke tests opt-in and document the exact profile they qualify. Use immutable fixtures for protocol parsing; a fake driver cannot establish sandbox isolation.

Describe each change in terms of the developer-visible behavior, the failure condition it handles, and the evidence collected. Update the support matrix only when the corresponding profile has actually passed its gate.
