# Vendoring decisions

Branchyard vendors selected control sources rather than copying whole orchestrators. The immutable upstream layer makes subsequent changes reviewable. Runtime integration is explicit: retaining a source file does not establish production support.

## Included material

| Project | Files | Selection | Current integration |
|---|---:|---|---|
| Scion | 46 | Shared helper and tests; telemetry provisioning tests; nine provisioners and their adjacent helpers/configuration; six provisioner test files; Claude settings fixture; license, harness overview and authoring guide | Python suites run directly; seven provisioners translated into `branchyard-provision`, which the engine runs before every turn; the Python is never executed by Branchyard |
| Herdr | 25 | 22 detection TOMLs, manifest loader source, resume source, license | Resume logic extracted into the Rust crate; manifests parse through the catalog checker |
| OpenRig | 6 | Runtime adapter contract, four runtime fragments, license | Design input for projection/readiness handling; TypeScript imports require the upstream application |
| Agent Substrate | 2 | Public `ateapi.proto` and license | Rust client generated at build time; adapter in `branchyard-substrate`, unqualified |
| Warp | 7 | Process control, exit escalation, JSON utilities, two test files, two license texts | AGPL reference collection, outside Cargo's build |

The Scion provisioners cover Antigravity, Claude, Codex, Copilot, Gemini CLI, Grok Build, Hermes, Muse Code, and OpenCode, pinned at `d9b9e6a` (re-pinned from `54b9387` on 27 September 2026, when the translation was made, so that the vendored files are the ones the Rust follows; at this revision the Claude suite passes against its provisioner). Their local copies of `scion_harness.py` are preserved: the root and adjacent helper files are not all identical at this revision. Do not consolidate them without a compatibility test.

## Harness identity

Each source names harnesses its own way. Herdr's `grok` manifest and Scion's `grok-build` provisioner describe the same harness; Herdr's `copilot` manifest is stored as `github-copilot.toml`; the integration matrix uses display names. `branchyard_controls::harness` gives every harness one Branchyard ID and records each source's name for it.

Mappings rest on upstream evidence: Herdr's manifest aliases (`grok-build`, `muse-code`, `github-copilot`, `claude-code`) and Scion's launch commands (`grok`, `muse`, `gemini`, `copilot`). The registry's tests read the vendored files directly. They fail when an upstream update adds an unmapped manifest, provisioner or resume source, when a mapped name disappears, when the integration matrix changes, or when an upstream alias contradicts a mapping. Map new names in the same change that updates the upstream.

A registry entry records naming only. It is not a support claim.

## Scion: reuse environment projection

The reusable unit is its provisioner bundle: a manifest, staged inputs, a harness-specific translator, and output configuration. Branchyard keeps that boundary in Rust: [`branchyard-provision`](../crates/branchyard-provision/src/lib.rs) plans each harness's native files and variables without I/O, and the engine applies the plan to the branch's private home before launch, on every provider ([provisioning](provisioning.md)). It was translated rather than run as Python so that a harness image needs no interpreter, the planner is testable as data, and secrets never pass through staged files.

The translation is a derivative, not a vendored copy. Each derived file names its origin and changes in its header; `patches/scion-provision.json` records the blob ID of every upstream file each one follows, and `tools/verify_derivatives.py` fails when a re-pin changes one. To update: re-pin `vendor/scion` (all files at one revision), run the upstream suites, read the upstream diff of each flagged source, port what applies (with its tests), and record the new blobs.

The Branchyard adapter constructs trusted inputs from a registered profile. It rejects path escapes and links, keeps MCP servers on the driver's session channel where one exists (so no scope is translated), writes no hook commands, and takes credentials only from secrets the task names. Keep any per-run secret-bearing outputs in private ephemeral storage; exclude them from shared image layers and generic artifact capture.

The upstream Claude and Codex launch configurations include permission-bypass arguments, and Scion's seed files and Hermes provisioner turn approvals off. Those are Scion's execution assumptions, not defaults for Branchyard: none is ported, and a test checks that no provisioning plan contains one. The server creates an explicit launch plan for the chosen protocol and policy. A required approval capability that the driver cannot enforce causes admission to fail.

Some upstream MCP mappings demote project scope to global scope. Branchyard must reject that translation unless the selected profile deliberately uses a private per-run home with equivalent authority. It must never silently broaden access across sessions.

## Herdr: reuse resume recipes, keep observations separate

The Rust extraction retains the upstream resume recipes and tests, removes serialization dependencies, substitutes an agent-name string for the application-specific enum, and validates session references again before returning arguments. It additionally rejects flag-shaped session IDs. Its patch is checked in.

These recipes generate argument vectors. They do not spawn processes and do not constitute a persistent protocol driver. A caller must pass arguments directly to the qualified executable, without shell concatenation, and bind the referenced session to the correct tenant, harness, workspace, and version. An absolute session path alone proves neither authorization nor confinement. Prefer artifact IDs that the sandbox resolves within its private session store.

The `herdr:*` labels identify recipe families. They are not authenticated event origins. Branchyard's authenticated connection supplies identity.

Terminal detection TOMLs are useful for an optional observation or recovery interface. Spinner text, pane titles, prompt recognition, and idle states must never authorize a tool call, certify completion, or trigger a merge. Structured protocol events take precedence.

## OpenRig: retain the launch contract

OpenRig distinguishes installed resources, projection, startup delivery, launch outcomes, and readiness. Its explicit trust/auth/update gates and fork-source semantics provide useful implementation requirements. The vendored TypeScript contract is not a standalone module: its application imports are intentionally not copied into this Rust starter.

Translate these semantics into Rust types and driver contract tests. A failed resume must not silently become a fresh conversation. Fork must return a new native session token. Preserve evidence for a blocked launch while avoiding token leakage. Map `retry_fresh` to a policy-reviewed new attempt, not an automatic retry of uncertain work. Upstream `full_bypass` does not override Branchyard capabilities.

## Warp: preserve the license boundary

The copied application helpers show bounded shutdown escalation and ownership checks before signaling a process group. They are AGPL sources and remain unlinked reference material. Their tests also depend on Warp's surrounding modules. This is not a vendored Warp Factory server: the public client repository does not establish that the hosted factory control plane is included.

For the Apache-licensed worker, prefer the existing sandbox runtime's process supervision and termination operations. Implement Branchyard's lease and cancellation semantics around those operations. Do not paste Warp helper code into an Apache file or mechanically rewrite it while discarding attribution.

## Agent Substrate: generate from the contract, do not translate the runtime

[Agent Substrate](https://github.com/agent-substrate/substrate) is a Kubernetes-based runtime that suspends idle sandboxes to object storage and resumes them on warm workers. Branchyard consumes it through its public gRPC API. The vendored proto is the whole interface; its only imports are protobuf well-known types.

Do not translate Substrate's Go services into Rust. The control plane, node agent, sandbox coordinators and router are its implementation, tied to Kubernetes, gVisor and Kata, and would become an unsynchronized fork. If Branchyard later needs a small, self-contained Substrate package in-process, vendor the Go file and its tests unchanged, translate the tests first, keep the upstream header with a modification notice, and extend `tools/verify_derivatives.py` to fail when the upstream blob changes.

The adapter always creates atespace-scoped tags; it never publishes a tag beyond its atespace. Map each tenant authorization domain to one atespace. See [Agent Substrate](substrate.md) for the capability mapping and gaps.

## Replicas and Microsandbox

[Replicas](https://replicas.dev/) provides a useful product reference for server workspaces, selectable harnesses, and reviewable outputs. No licensed implementation of its runtime was identified in the supplied materials; no Replicas code is included.

[Microsandbox](https://microsandbox.dev/) is a runtime dependency candidate. Use its published Rust SDK/runtime at a qualified revision; do not fork a VMM, rebuild its guest agent, or depend on private-beta cloud access. Its control plane is not among the vendored files in this starter.

## Updating an upstream

1. Select a commit and fetch its tree and licenses. Review changes to license scope, public interfaces, auth handling, hook execution, and defaults.
2. Fetch the selected files from that exact commit. Verify each Git blob ID and record SHA-256. Keep the original paths.
3. Reapply adaptations outside `vendor/`; regenerate a human-readable patch against the new source.
4. Run source-integrity checks, upstream unit suites, and Branchyard driver contract tests. Record failures and disable affected profiles; do not mask them as supported.
5. Build a new immutable harness image and pass sandbox conformance before promotion. Retain the previous image and compatibility record for rollback.

The lock file records what was inspected, not whether an upstream version is safe to execute in every environment. Harness support is attached to a complete versioned profile and qualification result.
