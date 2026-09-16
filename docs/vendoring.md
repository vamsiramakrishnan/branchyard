# Vendoring decisions

Branchyard vendors selected control sources rather than copying whole orchestrators. The immutable upstream layer makes subsequent changes reviewable. Runtime integration is explicit: retaining a source file does not establish production support.

## Included material

| Project | Files | Selection | Current integration |
|---|---:|---|---|
| Scion | 44 | Shared helper and tests; nine provisioners and their adjacent helpers/configuration; six provisioner test files; Claude settings fixture; license and harness overview | Python suites run directly; no production launch path enabled |
| Herdr | 25 | 22 detection TOMLs, manifest loader source, resume source, license | Resume logic extracted into the Rust crate; manifests parse through the catalog checker |
| OpenRig | 6 | Runtime adapter contract, four runtime fragments, license | Design input for projection/readiness handling; TypeScript imports require the upstream application |
| Warp | 7 | Process control, exit escalation, JSON utilities, two test files, two license texts | AGPL reference collection, outside Cargo's build |

The Scion provisioners cover Antigravity, Claude, Codex, Copilot, Gemini CLI, Grok Build, Hermes, Muse Code, and OpenCode. Their local copies of `scion_harness.py` are preserved: the root and adjacent helper files are not all identical at this revision. Do not consolidate them without a compatibility test.

## Scion: reuse environment projection

The reusable unit is its provisioner bundle: a manifest, staged inputs, a harness-specific translator, and output configuration. Preserve that boundary. Static tools and templates can be baked into images. Dynamic instructions and session-specific MCP configuration are projected into the sandbox before launch. A small Python provisioning step can coexist with a Rust server; rewriting it immediately would duplicate existing behavior.

Before enabling a bundle, the Branchyard projection adapter must construct trusted inputs from a registered profile. It must reject path escapes, unsupported scope translations, unregistered hook commands, and implicit credential inheritance. Keep any per-run secret-bearing outputs in private ephemeral storage; exclude them from shared image layers and generic artifact capture.

The upstream Claude and Codex launch configurations include permission-bypass arguments. Those files are examples of Scion's execution assumptions, not defaults for Branchyard. The server creates an explicit launch plan for the chosen protocol and policy. A required approval capability that the driver cannot enforce causes admission to fail.

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
