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
| Remote `--detach` differs from local mode | Remote work already outlives the following client; the CLI explains the distinction. The validation used separate following processes |
| IAM deletion visibility is briefly eventually consistent | Cleanup checks successful deletion and subsequent absence rather than treating an immediate stale describe as a leaked resource |

The [toolkit guide](artifact-toolkit.md) describes installation and the parallel
deck recipe. The [identity guide](identity.md) separates server callers,
workload identity and human connector accounts. The [capture guide](artifact-capture.md)
describes bounded, producer-bound file publication.
