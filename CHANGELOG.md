# Changelog

## 0.3.1 - 2026-10-31

Operators can name the environments console sessions run in.
`execution_presets` in the server configuration lists each environment's
provider, harnesses, models, secrets by name, tenants and worker labels, and
`require_workspace_execution_presets` makes workspace sessions choose one. The
browser sends a preset ID, and the server resolves the rest and validates it
through the existing checks. A session on a remote preset never falls back to
local execution, and members see placeholders where an error would name a
preset's paths, endpoints or secrets. `GET /v1/execution-presets` reports each
environment's readiness from configuration and offline checks only, and the
manager MCP starts sessions on a preset.

The console guides a workspace start: choose sources, an environment, a harness
and model, and the outputs to collect, then follow several sessions side by
side, each with its own draft and outputs. Sources open in the inspector like
outputs, outputs are grouped by session and turn, and only the current turn's
outputs can be accepted. Source titles may be any Unicode text, sent with
`X-Source-Title-Encoded`, and downloads keep the title. XLSX and CSV previews no
longer warn about columns they did not cut, a session's first message shows
what the person typed, and follow-ups in a session that collects outputs keep
collecting them.

A repository's own skills in `.agents/skills/` reach a branch once a person
trusts that set with `by skills trust` or a server operator allows them. Every
turn records the skills it ran with, and copied sandboxes record their ready
time and transfer sizes. `by up`, `by doctor`, `by setup` and
`by substrate check` report remote readiness as seven separate checks and never
treat a found binary as ready to run; `by up --plan --smoke` adds one bounded,
read-only call to the control plane.

CI runs the test jobs under cargo-nextest and splits the PostgreSQL and coverage
jobs into partitions. Pull requests plan releases instead of building every
target, and main rehearses the build. The README says why Branchyard exists and
shows the console.

The evidence for this release is hermetic: fake agents, fake providers and
recorded fixtures. No live model, GKE or Substrate run was made for it.

## 0.3.0 - 2026-10-29

One desktop console now handles coding and knowledge work. Projects contain
workspaces and named native sessions. People can upload sources, follow
conversations, preview CSV and Office outputs, download originals and accept
current results. `by serve --app --knowledge` starts without a repository.

Workspace access combines token scopes, membership and the session creator's
identity. Queued actions recheck exact device credentials. Runtime and pull-request
services use the existing durable queue; manager MCP carries workspace revisions
for lifecycle actions. AGY tool approval compatibility requires an explicit
acknowledgment and operator permission.

Remote inputs have one bounded metadata index linked from the native prompt.
Copied providers stage selected files before their initial upload. Concurrent
session starts retry rolled-back metadata conflicts, and follow-up turns retain
the validated session policy. The pre-push hook clears inherited Git repository
context before running checks in scratch repositories.

Live checks covered parallel document work on GCE and a captured CSV deliverable
in a GKE Substrate gVisor actor. The GKE helper offers a Docker-free agentgateway
path with a compatible immutable gateway image. Temporary cloud resources were
verified removed. See the [validation record](docs/console-validation-2026-10-29.md)
for versions, setup failures and the limits on retained runtimes, microVMs and
other harness/model combinations.

## 0.2.1 - 2026-10-29

Operation lookups recheck the caller's current repository grants, including
idempotency-key reconciliation and waiting requests. A tenant match alone does
not expose an operation from another repository.

The README now follows installation, an AGY task, review and merge, document
work and remote setup. Package descriptions cover coding and knowledge work.
A shared vector icon, wordmarks and sharing card give the CLI's companion and
repository a consistent identity. The CLI lists readiness with setup and doctor.

The companion keeps search focus during updates, distinguishes loading, empty,
failed and filtered lists, and keeps full status reasons and navigation usable
on small screens.

## 0.2.0 - 2026-10-29

Branchyard now supports cooperative harness teams for coding and knowledge work,
with frozen handoffs, result contracts and parallel deck assembly that preserve
each harness's native planning and delegation.

Document skills create and check editable Office and PDF artifacts. Knowledge
skills capture exact source bytes, extract tables, clean CSV data with explicit
policies, calculate quantities with units, check citation integrity and render
layout previews. Design skills cover composition, charts and diagrams. Small
folder indexes progressively disclose instructions and references. Reused
upstream material is pinned with licenses and installed-content checks.

Managed file capture supports declared ignored outputs on copied providers,
producer fencing, bounded receipts and recovery. Operator model/effort changes
retain stored credential sources. Failed waiting JSON sends return a failing
shell status. Native identity uses Google ADC and exact Google ID-token bindings;
the merged Anvil companion adds protected upstream MCP authentication.

Bar and column charts include zero, and local evidence citations retain checked
file hashes and locators. Python 3.9 remains supported for the SDK and most
artifact operations; patched PDF table extraction requires Python 3.10 or newer.

Installation remains a single shell command using the latest GitHub release.
See [knowledge skills](docs/knowledge-skills.md),
[atomic knowledge validation](docs/atomic-knowledge-validation-2026-10-29.md), and
[remote validation](docs/remote-validation-2026-10-28.md) for commands, evidence and
limits. Quotation matching does not establish factual support; rendering does
not establish visual approval. Live Google Workspace human OAuth consent and
the agentic compute engine remain unqualified.
