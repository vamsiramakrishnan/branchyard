# Third-party sources

Retrieved 2026-09-16; Agent Substrate retrieved 2026-09-26; Scion re-pinned 2026-09-27. Files under `vendor/` are upstream copies pinned in `vendor.lock.json`, which records each path, commit, Git blob ID, SHA-256 digest, and license as fetched. A file may carry a local patch only when `vendor.patches.json` lists it with its reason and upstream commit; none does today, so every file is byte-for-byte upstream. No upstream NOTICE file was found at the selected repository roots. Existing copyright notices remain in their source files.

| Directory | Upstream | Revision | License |
|---|---|---|---|
| `vendor/scion` | GoogleCloudPlatform/scion | `d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338` | Apache-2.0; license retained |
| `vendor/herdr` | herdrdev/herdr | `5f3763dda88e0fefbce14afab7cc2c7232a5e2a8` | Apache-2.0; license retained |
| `vendor/openrig` | mvschwarz/openrig | `cc75efdd17fb967bde7cff6c5805791986af78d8` | Apache-2.0; license retained |
| `vendor/substrate` | agent-substrate/substrate | `1d7ca8ced056192a1801d6565251adcaab3eb0c9` | Apache-2.0; license retained |
| `vendor/warp-agpl` | warpdotdev/warp | `2f0db5c5edd8134f0e858aebbc0ecd0db2f91d38` | Selected application sources: AGPL-3.0; both upstream license texts retained |

`crates/branchyard-controls/src/resume.rs` is a modified derivative of Herdr's `src/agent_resume.rs`, under Apache-2.0, kept down to `is_official_agent_source`: the registry of agent/source pairs the harness catalog's tests validate `herdr_resume` mappings against. Herdr's CLI resume-recipe builder was ported for evaluation but never wired to a caller outside this module — no Branchyard profile needed it, since every driver with protocol-level resume builds its own launch argv, and ACP resume runs over the ACP protocol itself, never a harness's native CLI — so it was removed as dead code (see [vendoring](docs/vendoring.md#herdr-reuse-the-official-agent-source-registry-only)). Its header identifies the origin and modification; `patches/herdr-resume.patch` records the diff. Herdr's original license accompanies the extraction.

`crates/branchyard-provision` is a modified derivative of Scion's harness provisioners, under Apache-2.0: `harnesses/scion_harness.py`, the `provision.py` of `claude`, `codex`, `gemini-cli`, `opencode`, `copilot`, `hermes` and `antigravity`, model aliases from their `config.yaml`, the provisioning contract of `harnesses/authoring-guide.md`, and test cases from the vendored `*_test.py` files, all at the pinned revision. They were translated from Python to Rust, not copied. Each derived file's header names its origin, the revision, the files it follows, the license and what was changed. `patches/scion-provision.json` records the Git blob ID of every source file each derivative follows, and `tools/verify_derivatives.py` fails when one changes; a line patch between two languages would not be reviewable, so none is kept. Scion's copyright notice (Copyright 2026 Google LLC) is preserved in those headers and its license accompanies the vendored sources.

`crates/branchyard-substrate` generates Rust types and a gRPC client from the unmodified `vendor/substrate/pkg/proto/ateapipb/ateapi.proto` at build time. No Substrate Go source is copied or translated. The proto's copyright header is preserved in the vendored file and in the generated output.

The Substrate adapter's and the bridge's Cargo dependencies (Tonic, Prost, rustls with `ring`, `webpki-roots`, which bundles Mozilla's root certificates, and their transitive crates; `rcgen` for tests only) are resolved from `Cargo.lock` and are not vendored. `protoc-bin-vendored` supplies a prebuilt `protoc` used only at build time. Review their licenses when producing a distributable image.

The optional `postgres` feature of `branchyard`, `branchyard-server` and `branchyard-cli` depends on `postgres` and `tokio-postgres` (MIT OR Apache-2.0) and their transitive crates from crates.io, resolved from `Cargo.lock` and not vendored.

`branchyard` depends directly on `blake3` (CC0-1.0 OR Apache-2.0) from crates.io for artifact content-addressing (`docs/storage.md`); it was already a transitive dependency of `branchyard-microsandbox`'s image tooling, so this adds no new entry to `Cargo.lock`'s dependency graph beyond the direct edge.

The command-line parsers of `by`, `branchyard-server`, `branchyard-bridge`, `branchyard-herdr`, `branchyard-mcp` and `branchyard-qualify` use `clap` with its derive macros (MIT OR Apache-2.0); `by` also uses `clap_complete` and `clap_mangen` (MIT OR Apache-2.0) for `by completions` and `by man`, and `shlex` (MIT OR Apache-2.0, already in `Cargo.lock`) to split `--check` and `--command`. They and their transitive crates (`anstream`, `anstyle`, `clap_lex`, `roff` and others) are resolved from `Cargo.lock` from crates.io and are not vendored.

`crates/branchyard-harness` depends on `agent-client-protocol-schema` (Apache-2.0) and `serde_json` (MIT OR Apache-2.0) from crates.io. Its test fixtures are redacted transcripts recorded from Claude Code 2.1.283, codex-cli 0.157.1 and claude-agent-acp 0.81.2; they contain protocol frames and harness output, not harness source. Frame shapes also follow the Agent SDK's published TypeScript types and Codex's generated JSON Schema, neither of which is vendored.

Warp licenses its UI framework crates under MIT and the rest of its repository under AGPL v3. The selected harness helpers belong to the latter category. The retained MIT license text does not grant an MIT license to these helpers. See [Warp's licensing statement](https://github.com/warpdotdev/warp/tree/2f0db5c5edd8134f0e858aebbc0ecd0db2f91d38#licensing).

Warp files are a source reference collection, not a complete independently buildable library. The Cargo workspace has no path dependency on them. Incorporating them into a service is a separate licensing and implementation decision; a directory or process boundary alone should not be treated as a licensing exemption.

Harness executables, model services, third-party ACP adapter packages, and Microsandbox are not distributed in this starter. Their licenses, authentication requirements, and server deployment terms must be tracked when producing runtime images. No affiliation or endorsement by an upstream project is implied.
