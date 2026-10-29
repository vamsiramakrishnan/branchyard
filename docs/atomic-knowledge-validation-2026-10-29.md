# Atomic document work and native identity validation

Written 29 October 2026.

These checks use development builds of Branchyard and Anvil, not a claim that
the public Branchyard 0.1.1 release contains the new features. The implementation
is on `feat/atomic-knowledge-work` and [Anvil PR 79](https://github.com/vamsiramakrishnan/anvil/pull/79).
The case inputs are illustrative, not a forecast about a real organization.

## Artifact runtime and source-free tasks

Hermetic remote runtime evidence: the optional Docker image built on the GCE
worker and passed its complete doctor requirements with networking disabled.
Its real-file suite passed 16 tests and its installed-wheel suite passed three.
Those tests inspect DOCX, XLSX, PDF and PPTX bytes, preserve originals on updates,
verify a native pivot and cache records, and assemble a deterministic 60-slide
fixture. The runtime used Python 3.11, LibreOffice 7.4.7.2 and the pinned Python
libraries in the committed dependency lock. Not done: cross-platform Office
application qualification or live Google Workspace editing.

Live harness evidence: AGY 1.4.9 ran through native Branchyard SSH on the existing
GCE worker. It read its task's embedded capabilities index and relevant skills,
ran the installed helper with required format/render/pivot/font capabilities,
and returned the accepted native JSON `{"status":"ready"}`. The task ended
`NoChanges`, without a Branchyard checkout or delegation grant. Its API key
was read from an existing private server-side credential file. No credential
was copied into the repository or evidence. Not done: AGY OAuth qualification.

The controller's record is `/tmp/branchyard-atomic-20261028/live/`, including
`image-build.log`, `image-doctor.json`, `image-tests.log`,
`image-wheel-tests.log`, `capabilities-events.log` and
`capabilities-result.json`. The isolated remote directory is
`by-atomic-20261029` under the validation account's home. The original VM and
its existing services were preserved.

## Live GKE WIF and protected MCP

Live identity evidence: the existing GKE Autopilot cluster `cc-sandbox` ran
dedicated namespace-local workloads mapped to new allow and deny IAM service
accounts. Official Google ADC used the real GKE metadata service; no ADC file,
service-account key or `gcloud` token fallback was configured in the workloads.

The source-free compiled Anvil gateway called a TLS MCP qualification fixture
through the official MCP SDK. That fixture used Google's real Storage API to
authenticate the workload bearer and enforce IAM on a dedicated test bucket.
The allowed workload read `probe.json`; the denied workload received native
GCS HTTP 403 and executed no captured tool. Branchyard grant and confirmation
denials made no upstream requests. The captured native tool name survived
its public rename. This is a live GCS-IAM-backed MCP fixture, not a claim about
IAM-protected MCP hosting or a third-party connector's production service.

A development Branchyard server also verified real metadata ID tokens. It
accepted the configured exact Google issuer, service-account numeric subject
and audience, rejected a wrong audience and an unmapped subject, and retained
the configured read-only scope when `/metrics` required admin. Not done: live
human OAuth consent for Google Docs, Sheets or Slides. Protected MCP OAuth
discovery, PKCE, refresh and account isolation have actual-SDK hermetic tests.

Redacted records are under `/tmp/branchyard-atomic-20261028/live-wif/`:
`gateway-allow.json`, `gateway-deny.json`, `admission-allow.json`,
`admission-deny.json`, `runtime-receipt.json` and `resources.json`.
The namespace, bucket and both new service accounts were deleted and absence
verified. One immediate service-account describe briefly returned stale data;
subsequent describe and list checks confirmed removal. Temporary validation
TLS private files and kubeconfigs were removed. Existing cluster settings,
workloads and project IAM policies were preserved.

Live end-to-end extension: a real Google metadata ID token admitted a task to
Branchyard over HTTP. The engine packaged the connector, issued the turn token
and invoked the generated TypeScript SDK through its normal effect proxy and
supervised Anvil gateway. Google's native bucket IAM allowed the read. All 14
in-turn assertions passed, including JWKS verification, derived caller identity,
tenant and account namespace, audit attribution, and a read-grant mutation
refusal that made no upstream call. An unmapped Google caller received 401.
The task used the existing fake ACP shell driver, so this was a live identity
test without a model call. The committed [record and rerun procedure](https://github.com/vamsiramakrishnan/anvil/blob/feat/branchyard-identity-mcp/docs/qualification/2026-10-29-native-trust-chain.md)
include binary hashes and cleanup receipts. The extension's temporary namespace,
bucket and service accounts were removed and absence verified. Not done: a
model-driven identity turn, production MCP hosting, or human Workspace consent.

## Copied-provider artifact capture

Live copied-provider evidence: a trusted SSH recipe ran AGY 1.4.9 with
`gemini-3.8-flash-low` on the existing GCE host. One fresh turn wrote a declared
nested JSON output and unrelated files, all ignored by Git. The task ended
`NoChanges`, with no Git candidate, and its native required result was accepted.
Ordinary artifact retrieval returned exactly the declared 68 bytes; their
SHA-256 was `66485b25bb42036b28a657a3057b9e8e12795f2ad9319f57e1956fe32c89b595`.
The producer-bound receipt contained one artifact. No ignored output was
restored to the controller's worktree. The remote job, task home and launcher
were removed, and the original services and credential metadata were unchanged.
The [redacted verification receipt](evidence/atomic-knowledge-2026-10-29/copied-capture-verification.json)
records the asserted boundaries.
Records are in `/tmp/branchyard-atomic-20261028/live-copied-ssh/`, including
`RECORD.md`, `verification.json`, `capture-receipt.json` and cleanup observations.
Not done: copied-provider Office rendering, live crash recovery or isolation.

The qualification's job-owned launcher read the original remote credential
file in place and supplied it only to the AGY child's environment. This avoided
the existing recipe environment transport, which serializes environment values
into SSH command arguments. General recipe secret transport was not qualified
or changed. Native SSH server mode resolves secrets on the server and is the
preferred route for credentialed tasks.

## Parallel deck and review

Live knowledge-work evidence: AGY 1.4.9 ran on the existing GCE worker through
native Branchyard SSH. The parallel-deck rig spawned three native harness
workers for economics, delivery and governance. The initial Pro conversations
hit provider HTTP 429 quota and 503 demand errors. Explicit operator
reincarnations with `gemini-3.8-flash-low` completed three accepted 20-slide
sections, captured as immutable producer artifacts. A separate assembler
received those artifacts through ordinary sharing and produced a 60-slide
PPTX with ten editable charts and a 60-page PDF. Not done: an unassisted
single-command completion of the initial rig or measured model dollar cost.

The first assembly exceeded its ten-minute controller budget after rendering.
An explicit fresh continuation captured the deliverables. Independent review
checked all ten chart series against the illustrative inputs and found correct
values, then requested three clearer unit labels. It also found repeated
delivery filler and governance claims broader than the qualification evidence.
An explicit editorial pass replaced that filler and qualified those claims.
AGY applied the reviewed sources, reassembled and rendered the deck, returned
the required native JSON and captured seven new deliverables on submitted
turn four. The intervening third turn had a soft-denied tool command and an
empty native result, so it published no accepted deliverable. Schema validation
and deterministic assembly alone do not establish useful or accurate content.
The [capture inventory](evidence/atomic-knowledge-2026-10-29/deck-capture.json)
records exact producer references, byte counts and content digests.

Replayed artifact evidence: the downloaded final capture passed all 442
[independent checks](evidence/atomic-knowledge-2026-10-29/deck-verification.json).
They compare the seven captured files and exact reviewed source bytes, all
sixty slides and PDF pages, ten chart series and embedded Excel workbooks,
financial arithmetic, and two fresh deterministic assemblies against the
captured PowerPoint bytes. Visual review inspected all-page contact sheets and
thirteen full-size pages; the controller also opened final pages 21 and 45.
No obvious clipping, overlap or missing chart was found. The result uses a
sparse report layout. Not done: interactive Microsoft Office roundtripping or
an independently verified business forecast.

The case estimates 552 hours of monthly capacity value at full adoption, not
cash savings. Its illustrative twelve-month net value is USD 295,180 after
ramp, operating costs and initial investment. Neither number estimates this
validation's actual model spend. The local export preserves accepted originals,
source specifications, capture references and independent verification under
`/home/user/branchyard-atomic-demo-20261029/`. The controller's native event and
result records are under `/tmp/branchyard-atomic-20261028/live/`.

## Setup friction and changes

| Observed friction | Result |
|---|---|
| A skill could name tools that were absent from an installed task | The CLI embeds the canonical toolkit and stages it before placement; a source-free AGY task exercised it |
| A task without delegation received no useful document skill | Staging runs independently of delegation; an actual non-delegating task loaded the index |
| Installing one skill could omit its shared tools | The installer resolves dependencies and checks every destination before writing |
| Native installation summaries omitted directory payloads | Summaries count distinct skill roots; existing recursive installation is retained |
| Optional Python and native Office dependencies were unclear | Doctor distinguishes requirements, explicit setup uses a hashed lock, and the runtime image includes native rendering and pivots |
| Interrupted setup or linked cache paths could select unsuitable runtime files | Setup uses a structured readiness receipt and refuses linked cache boundaries; regression tests cover both |
| Theme defaults caused slide footer and layout problems in the earlier demo | The shared renderer sets explicit font, geometry and paragraph spacing and preserves editable chart data |
| High GCP OS Login UIDs exceed Docker's accepted user range | This validation used a private UID/ACL wrapper around the Office container; running an artifact image still needs an appropriate host ownership configuration |
| A container adapter dropped Python heredoc input | The lab adapter now forwards stdin with Docker's `-i`; a real heredoc probe and subsequent assembly exercised it |
| A model-only continuation replaced credential provisioning | Operator sends now use a typed session patch for model and effort edits; regression tests check retained secret sources, MCP configuration and instructions |
| Failed remote JSON sends returned shell success | Waiting JSON sends now return a failing exit status for failed tasks while retaining the same single JSON envelope |
| A capabilities event listed every helper file | Human output now groups document assets under their capabilities index; structured file inventories remain available |
| A remote continuation defaulted to asking for tool permission where no terminal could answer | The test explicitly used `--yes --allow-unapproved-tools` on the owned task; policy remains explicit rather than silently allowing tools |
| Model quota errors persisted in an older conversation after changing the model | Explicit fresh reincarnations completed on the available model; the original failed results were retained rather than accepted |
| A valid section could still contain repeated filler or unsupported claims | Independent content review checks usefulness and evidence in addition to schema, chart and render checks |
| Remote `--detach` differs from local mode | Remote work already outlives the following client; the CLI explains the distinction. The validation used separate following processes |
| IAM deletion visibility is briefly eventually consistent | Cleanup checks successful deletion and subsequent absence rather than treating an immediate stale describe as a leaked resource |

The [toolkit guide](artifact-toolkit.md) describes installation and the parallel
deck recipe. The [identity guide](identity.md) separates server callers,
workload identity and human connector accounts. The [capture guide](artifact-capture.md)
describes bounded, producer-bound file publication.

Live continuation evidence: after upgrading only the owned job's Branchyard
binaries, a model-only `by send` on the successful Flash probe retained the
stored API-key source, provisioned `GEMINI_API_KEY` and returned the accepted
native `{"status":"ready"}` result with `NoChanges`. No credential flags were
supplied on the continuation. The [redacted record](evidence/atomic-knowledge-2026-10-29/session-patch.json)
includes binary hashes. The original listeners on ports 8787 and 5432 remained
available. Not done: AGY OAuth or changing a fixed native conversation to a
different model.

Hermetic setup evidence: ten bootstrap tests passed after a regression first
reproduced repeated setup deleting a ready environment. Matching readiness
receipts now skip deletion and installation while retaining the setup lock;
incomplete or mismatched environments still repair. Not done: concurrent live
installation against an external package index.
