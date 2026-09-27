# Third-party sources

Retrieved 2026-09-16; Agent Substrate retrieved 2026-09-26. Files under `vendor/` are byte-for-byte upstream copies. `vendor.lock.json` records each path, commit, Git blob ID, SHA-256 digest, and license. No upstream NOTICE file was found at the selected repository roots. Existing copyright notices remain in their source files.

| Directory | Upstream | Revision | License |
|---|---|---|---|
| `vendor/scion` | GoogleCloudPlatform/scion | `54b9387549ea2f673376c94b5d992010ebe9100a` | Apache-2.0; license retained |
| `vendor/herdr` | herdrdev/herdr | `5f3763dda88e0fefbce14afab7cc2c7232a5e2a8` | Apache-2.0; license retained |
| `vendor/openrig` | mvschwarz/openrig | `cc75efdd17fb967bde7cff6c5805791986af78d8` | Apache-2.0; license retained |
| `vendor/substrate` | agent-substrate/substrate | `1d7ca8ced056192a1801d6565251adcaab3eb0c9` | Apache-2.0; license retained |
| `vendor/warp-agpl` | warpdotdev/warp | `2f0db5c5edd8134f0e858aebbc0ecd0db2f91d38` | Selected application sources: AGPL-3.0; both upstream license texts retained |

`crates/branchyard-controls/src/resume.rs` is a modified derivative of Herdr's `src/agent_resume.rs`, under Apache-2.0. Its header identifies the origin and modifications; `patches/herdr-resume.patch` records the changes. Herdr's original license accompanies the extraction.

`crates/branchyard-substrate` generates Rust types and a gRPC client from the unmodified `vendor/substrate/pkg/proto/ateapipb/ateapi.proto` at build time. No Substrate Go source is copied or translated. The proto's copyright header is preserved in the vendored file and in the generated output.

The Substrate adapter's and the bridge's Cargo dependencies (Tonic, Prost, rustls with `ring`, `webpki-roots`, which bundles Mozilla's root certificates, and their transitive crates; `rcgen` for tests only) are resolved from `Cargo.lock` and are not vendored. `protoc-bin-vendored` supplies a prebuilt `protoc` used only at build time. Review their licenses when producing a distributable image.

`crates/branchyard-harness` depends on `agent-client-protocol-schema` (Apache-2.0) and `serde_json` (MIT OR Apache-2.0) from crates.io. Its test fixtures are redacted transcripts recorded from Claude Code 2.1.283, codex-cli 0.157.1 and claude-agent-acp 0.81.2; they contain protocol frames and harness output, not harness source. Frame shapes also follow the Agent SDK's published TypeScript types and Codex's generated JSON Schema, neither of which is vendored.

Warp licenses its UI framework crates under MIT and the rest of its repository under AGPL v3. The selected harness helpers belong to the latter category. The retained MIT license text does not grant an MIT license to these helpers. See [Warp's licensing statement](https://github.com/warpdotdev/warp/tree/2f0db5c5edd8134f0e858aebbc0ecd0db2f91d38#licensing).

Warp files are a source reference collection, not a complete independently buildable library. The Cargo workspace has no path dependency on them. Incorporating them into a service is a separate licensing and implementation decision; a directory or process boundary alone should not be treated as a licensing exemption.

Harness executables, model services, third-party ACP adapter packages, and Microsandbox are not distributed in this starter. Their licenses, authentication requirements, and server deployment terms must be tracked when producing runtime images. No affiliation or endorsement by an upstream project is implied.
