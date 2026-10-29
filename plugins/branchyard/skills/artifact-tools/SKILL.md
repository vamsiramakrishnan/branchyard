---
name: artifact-tools
description: "Find and run Branchyard document tooling, check its runtime, and prepare file manifests for managed artifact capture."
---
# Artifact tools

The installed toolkit entry is `scripts/run.py` relative to this skill. Its package is also available as `python3 -m branchyard_artifacts` in an SDK installation with the artifacts extra. A managed task's capabilities index gives the actual guest-visible entry path; do not assume the source checkout exists.

Run `doctor` to inspect format libraries, fonts, LibreOffice and UNO. Run `python3 scripts/run.py setup` from this skill directory to explicitly create a private, hash-pinned Python environment. Repeating setup repairs an interrupted installation. Native rendering and pivots also need the documented artifact-runtime image or system LibreOffice, poppler, fonts and python3-uno. An SDK installation can use the artifacts extra instead. Tool invocation does not download dependencies. Read the installed package examples and schemas for complex requests.

Local tools create or update files in the task workspace. A schema-valid JSON reply is not evidence that a file exists or a calculation is correct: reopen/check the actual output, and inspect rendered previews when visual quality matters. Preserve original files for requested revisions. Keep formulas distinct from calculated cached values, and a summary table distinct from a native pivot.

For a task with result artifact capture, run `manifest FILE... --out outputs/artifacts.json` after outputs pass checks. Keep every declared file beneath that manifest's directory. The engine collects these bytes only after the producing turn settles and workspace transfer completes. Collection publishes ordinary immutable artifacts under the producer; it does not grant a sibling access or start another turn. Use existing artifact grants and handoffs to share accepted files.

Google skills use the granted connector index and its SDK. Discover real operation names and schemas, selected account, scopes and revision features. Complete writes already authorized by the request and grants. Missing consent needs the person's connect flow; missing upstream permissions need the upstream owner. Do not ask for provider tokens, forward Branchyard bearer tokens, widen a grant or fall back to a workload account when a human connection is unavailable.
