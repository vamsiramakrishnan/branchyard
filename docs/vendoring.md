# Vendoring decisions

Branchyard vendors selected control sources rather than copying whole orchestrators. Runtime integration is explicit: retaining a source file does not establish production support.

## Pins and local patches

`vendor/` is a pinned reference snapshot. It no longer has to stay byte-identical with upstream: a file may carry a local patch, as long as the patch is recorded.

- **Pins.** [`vendor.lock.json`](../vendor.lock.json) pins every file under `vendor/` to its upstream repository, commit and path, with the Git blob ID and SHA-256 of the file as fetched, its license and its use. A pin always describes upstream (`"modified": false`); a local change never rewrites it.
- **Patches.** [`vendor.patches.json`](../vendor.patches.json) lists each patched file: `path`, `reason`, and `upstream_commit`, which must be the commit the file is pinned at, so a re-pin forces each patch to be reviewed against the new upstream. It is empty today.
- **Verification.** [`tools/verify_vendor.py`](../tools/verify_vendor.py) (no network) fails when a file not listed as patched differs from its pin; when a listed file matches its pin (a stale entry); when an entry lacks a reason, names another commit, or names a file that is not pinned; and when a file under `vendor/` is not pinned or a pin has no file. `tests/test_verify_vendor.py` exercises each case on small fixture trees.
- **Derived code follows upstream.** `tools/verify_derivatives.py` compares each Scion translation's recorded blobs with the pins, not with the working file, so a local patch to a vendored copy does not change what a translation follows. Herdr's `patches/herdr-resume.patch` is the diff from the vendored file as checked in; regenerate it if that file is patched.

To patch a file: edit it, add its entry to `vendor.patches.json` with a reason a reviewer can check, and run `python3 tools/verify_vendor.py`. When re-pinning an upstream, re-fetch the file, then re-apply or drop each patch and update its `upstream_commit`.

The license boundary is unchanged, and the verifier enforces the mechanical part of it: files licensed AGPL live only under `vendor/warp-agpl/`, no Cargo workspace member lives under `vendor/`, and no Rust or Cargo file outside `vendor/` refers to `vendor/warp-agpl` (so no path dependency, `include!` or `#[path]` can pull Warp code into an Apache-licensed crate). Copying or paraphrasing Warp code into an Apache file is still forbidden, and remains a review matter; see [Warp](#warp-preserve-the-license-boundary).

## Included material

| Project | Files | Selection | Current integration |
|---|---:|---|---|
| Scion | 46 | Shared helper and tests; telemetry provisioning tests; nine provisioners and their adjacent helpers/configuration; six provisioner test files; Claude settings fixture; license, harness overview and authoring guide | Python suites run directly; seven provisioners translated into `branchyard-provision`, which the engine runs before every turn; the Python is never executed by Branchyard |
| Herdr | 25 | 22 detection TOMLs, manifest loader source, resume source, license | Only the official-agent-source registry check is kept in the Rust crate; manifests parse through the catalog checker |
| OpenRig | 6 | Runtime adapter contract, four runtime fragments, license | Design input for projection/readiness handling; TypeScript imports require the upstream application |
| Agent Substrate | 2 | Public `ateapi.proto` and license | Rust client generated at build time; adapter in `branchyard-substrate`, unqualified |
| Warp | 7 | Process control, exit escalation, JSON utilities, two test files, two license texts | AGPL reference collection, outside Cargo's build |
| emdash | 41 | 37 agent plugin definitions, their install helpers, the MCP catalog, the project configuration schema, license | Read by `branchyard-controls`' tests to map harnesses and generate `catalog/harnesses.toml` and `catalog/connectors.toml`; `emdash-config.ts` translated into `branchyard-setup`'s importer |
| Orca | 15 | Agent table and names, Claude resume guard, diff-comment format and types, review-thread resolution, `orca.yaml` parser and hook types, `.worktreeinclude` and worktree placement helpers, the automation precheck runner, its rules and run-target resolution, license | Agent table read by tests (registry and catalog); the format, resolution and `orca.yaml` parser translated into `branchyard-cli` and `branchyard-setup`; the worktree helpers into `branchyard-workspace`; the precheck and run-target resolution into `branchyard-server`'s [triggers](triggers.md#prechecks) |

The Scion provisioners cover Antigravity, Claude, Codex, Copilot, Gemini CLI, Grok Build, Hermes, Muse Code, and OpenCode, pinned at `d9b9e6a` (re-pinned from `54b9387` on 27 September 2026, when the translation was made, so that the vendored files are the ones the Rust follows; at this revision the Claude suite passes against its provisioner). Their local copies of `scion_harness.py` are preserved: the root and adjacent helper files are not all identical at this revision. Do not consolidate them without a compatibility test.

## Harness identity

Each source names harnesses its own way. Herdr's `grok` manifest and Scion's `grok-build` provisioner describe the same harness; Herdr's `copilot` manifest is stored as `github-copilot.toml`; the integration matrix uses display names. `branchyard_controls::harness` gives every harness one Branchyard ID and records each source's name for it.

Mappings rest on upstream evidence: Herdr's manifest aliases (`grok-build`, `muse-code`, `github-copilot`, `claude-code`) and Scion's launch commands (`grok`, `muse`, `gemini`, `copilot`). The registry's tests read the vendored files directly. They fail when an upstream update adds an unmapped manifest, provisioner or resume source, when a mapped name disappears, when the integration matrix changes, or when an upstream alias contradicts a mapping. Map new names in the same change that updates the upstream.

A registry entry records naming only. It is not a support claim.

## emdash and Orca: ports, as data and as translations

emdash (Apache-2.0, `873a3e2`) and Orca (MIT, `2807332`) are desktop worktree managers; Branchyard takes what they know about harnesses and connectors, and a few conveniences, without taking either application ([roadmap](roadmap.md), the Ports row). Superset (Elastic License 2.0) is replicated from documented behaviour only; nothing of it is vendored. Agent icons (`src/shared/agent-icons/*`, `icon.ts`) are third-party logos and are not vendored.

- **Data read by tests.** `branchyard_controls::harness` maps every emdash plugin `id` and every key of Orca's `TUI_AGENT_CONFIG` to one Branchyard ID, and its tests fail when upstream adds one that is not mapped, when a mapping names one that is gone, when Orca's `TuiAgent` union and its table disagree, or when the two registries name different executables for a mapped harness (Rovo Dev is the one recorded exception, with its reason). `src/tsdata.rs` reads the TypeScript object literals as JSON, evaluating nothing: an identifier, call, spread or expression stays a marker. The same tests generate `catalog/harnesses.toml` (install commands with emdash's npm and Homebrew helpers expanded, CLI login, API-key variables, models, model and resume flags, executables) and `catalog/connectors.toml` (emdash's MCP catalog reduced to kind, URL or package, authentication and credential names), and fail when the checked-in files differ; `BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog` regenerates them for review. A catalog entry is knowledge, not support.
- **Translations.** The diff-comment format (`by review`), the review-thread resolution mutation (`by pr --watch`), Orca's `orca.yaml` parser and emdash's configuration schema (`by init project`), and Orca's automation precheck and run-target resolution (`by trigger`'s prechecks) are translated into Rust, each with a header naming its origin and changes, and a test that the vendored source still has the shape the port follows. `patches/ports.json` records every derived file with the blob IDs of the sources it follows; `tools/verify_derivatives.py` fails when a header is incomplete or a pinned source changes.

To update: re-pin `vendor/emdash` or `vendor/orca` (all files at one revision), run `cargo test -p branchyard-controls`, map any new agent in `HARNESSES`, regenerate the catalogs and review their diff, read the upstream diff of each source `patches/ports.json` lists, port what applies, and record the new blobs.

## Scion: reuse environment projection

The reusable unit is its provisioner bundle: a manifest, staged inputs, a harness-specific translator, and output configuration. Branchyard keeps that boundary in Rust: [`branchyard-provision`](../crates/branchyard-provision/src/lib.rs) plans each harness's native files and variables without I/O, and the engine applies the plan to the branch's private home before launch, on every provider ([provisioning](provisioning.md)). It was translated rather than run as Python so that a harness image needs no interpreter, the planner is testable as data, and secrets never pass through staged files.

The translation is a derivative, not a vendored copy. Each derived file names its origin and changes in its header; `patches/scion-provision.json` records the blob ID of every upstream file each one follows, and `tools/verify_derivatives.py` fails when a re-pin changes one. To update: re-pin `vendor/scion` (all files at one revision), run the upstream suites, read the upstream diff of each flagged source, port what applies (with its tests), and record the new blobs.

The Branchyard adapter constructs trusted inputs from a registered profile. It rejects path escapes and links, keeps MCP servers on the driver's session channel where one exists (so no scope is translated), writes no hook commands, and takes credentials only from secrets the task names. Keep any per-run secret-bearing outputs in private ephemeral storage; exclude them from shared image layers and generic artifact capture.

The upstream Claude and Codex launch configurations include permission-bypass arguments, and Scion's seed files and Hermes provisioner turn approvals off. Those are Scion's execution assumptions, not defaults for Branchyard: none is ported, and a test checks that no provisioning plan contains one. The server creates an explicit launch plan for the chosen protocol and policy. A required approval capability that the driver cannot enforce causes admission to fail.

Some upstream MCP mappings demote project scope to global scope. Branchyard must reject that translation unless the selected profile deliberately uses a private per-run home with equivalent authority. It must never silently broaden access across sessions.

## Herdr: reuse the official-agent-source registry only

`crates/branchyard-controls/src/resume.rs` began as a fuller extraction: the upstream resume recipes and their tests, with serialization dependencies removed, an agent-name string substituted for the application-specific enum, session references validated again before returning arguments, and flag-shaped session IDs additionally rejected. Nothing outside `branchyard-controls` ever called that argument builder, so it was removed rather than kept as an unused derivative (a quick-wins review found no caller and, checking every profile against `docs/compatibility.md`, no sound place to add one — see below). What remains is `is_official_agent_source`, the registry of agent/source pairs Herdr's recipes recognize as official; `branchyard_controls::harness`'s `every_herdr_resume_source_is_registered` test validates the harness catalog's `herdr_resume` mappings against it, so a mapping can never claim upstream support Herdr does not have. Its patch is checked in and covers the whole diff from upstream, including what was cut.

**Why no fallback wiring.** A recipe would help only where Branchyard has no protocol-level resume: that is every ACP profile whose agent does not advertise `session/resume` or `session/load` (`docs/compatibility.md`'s `if advertised` rows), plus `aider`, which has no driver at all. Aider has no Herdr recipe. For the ACP rows, several agents *do* have a matching Herdr recipe (`opencode --session <id>`, `cursor-agent --resume <id>`, `hermes --resume <id>`, `qwen --resume <id>`, `kimi --session <id>`, `copilot --resume=<id>`) — but resume there is not a separate launch mode: it is negotiated inside the same ACP connection the driver already speaks (`session/resume`/`session/load` over JSON-RPC), and `crates/branchyard-harness/src/acp.rs`'s `Driver` only ever knows how to drive that protocol. Substituting a Herdr recipe's argv would launch the agent in its native, non-ACP CLI mode instead — a different protocol the `Acp` driver cannot read or write — so it would not resume the ACP session; it would silently start a process the driver could not talk to. Every profile with a real native driver (Claude Code, Codex, Antigravity, Pi, Amp) already builds its own resume argv straight from the protocol handshake (e.g. Claude Code's driver appends `--resume <id>` itself), which is the same recipe Herdr records, arrived at independently and exercised end to end by that driver's own tests — using the Herdr copy there would be redundant, not additive. There is accordingly no profile today where wiring the recipe module in is sound; if a future profile has no protocol-level resume and a matching native CLI resume flag with no other way to reach it, restore the builder and its tests from `patches/herdr-resume.patch`'s pre-cut history rather than reinventing it.

These recipes, where restored, generate argument vectors. They do not spawn processes and do not constitute a persistent protocol driver. A caller must pass arguments directly to the qualified executable, without shell concatenation, and bind the referenced session to the correct tenant, harness, workspace, and version. An absolute session path alone proves neither authorization nor confinement. Prefer artifact IDs that the sandbox resolves within its private session store.

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
3. Reapply adaptations outside `vendor/`; regenerate a human-readable patch against the new source. Re-apply or drop each local patch listed in `vendor.patches.json`, and set its `upstream_commit` to the new pin.
4. Run source-integrity checks, upstream unit suites, and Branchyard driver contract tests. Record failures and disable affected profiles; do not mask them as supported.
5. Build a new immutable harness image and pass sandbox conformance before promotion. Retain the previous image and compatibility record for rollback.

The lock file records what was inspected, not whether an upstream version is safe to execute in every environment. Harness support is attached to a complete versioned profile and qualification result.
