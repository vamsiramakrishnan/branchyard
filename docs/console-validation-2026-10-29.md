# Console validation and setup notes, 29 October 2026

This record covers the 0.3.0 release candidate on `feat/workspace-console`.
The desktop UI was merged at `51fb6af4`, the workspace UI at `4e0c74bb`,
and the permission and preview integration at `321b01a5`. The workspace merge
had one conflict in `docs/README.md`; the resolution kept both the preview
component guide and the deterministic fixture guide. Publication is pending.

Hermetic verification on the integrated tree passed the companion HTTP suite
with nine tests, the desktop browser test, the workspace browser test, and the
server library suite with 252 passed and one existing ignored test. The
companion library filter passed 19 tests. Browser previews use explicit fake
metadata and PNG responses. Fake agents produce the captured files; original
bytes, downloads, session continuation and acceptance use the real server.
Not done in these results: live models, native document conversion, remote
provider execution, Safari, Firefox, a real phone or a screen reader.

A separate live GCE run on 29 October used Docker 29.1.3, Python 3.11.14,
LibreOffice 7.4.7.2 and Poppler 22.12.0. The recorded server identifies itself
as 0.2.1; this is evidence for the converter patch, not the final 0.3.0 binary.
CSV tables, XLSX tables and pages, DOCX pages and PPTX pages returned authenticated
HTTP 200 responses. Original SHA-256 values were unchanged, unauthenticated
requests were refused, the temporary server stopped, and no preview containers
remained running. See the [converter record](evidence/console-document-gce-2026-10-29.json).
Not done in that record: a model turn, a browser workflow, GKE conversion or
production load testing.

The integrated PostgreSQL suite passed all 22 tests against a private Unix-socket database. Schema freshness, shipped skill checks, Rustdoc with warnings denied and Clippy also passed. The missing-documentation gate caught one new command field; adding its comment restored the existing limit without raising it. Final live model acceptance and publication remain in progress.
The current GKE check has exercised the native Control API, an actor, a
byte-preserving file round trip and AGY 1.4.9's version command using the 0.3.0
CLI. That model-free smoke check does not qualify a model workflow. Live model
acceptance is recorded separately when it completes. Not done: final release
publication and an Agent Compute Engine preview run, which is excluded because
preview access is unavailable.

## Friction found during setup

| What failed | Cause | Resolution or remaining work |
|---|---|---|
| Fast checks could not find ruff | The system Python did not have CI's tools | Use a private environment with `ruff==0.15.8` and `mypy==2.4.0`. |
| The workspace browser could not create its Office fixtures | Its Python lacked the artifact libraries | Install the pinned artifact requirements in the environment used to run the test. A skipped browser run is not a pass. |
| An original UI test expected every workspace preview to return 404 | The UI branch predates the mounted preview routes | Test successful table and page previews explicitly, and retain a separate older-server 404 case. |
| A viewer was offered writer actions | Token scope checks omitted workspace membership | Read `current_role` and `capabilities.can_run` before offering uploads, sessions, continuation or acceptance. The server remains authoritative. |
| AGY start requests could not acknowledge tool-approval limitations | Permission policy and harness approval support are separate contracts | Add a separate, unchecked acknowledgment. The operator must permit it too; reviewers remain constrained. |
| A session form stayed disabled after initialization completed | Readiness was captured only when the form opened | Recompute the Start button's state when workspace data changes. |
| The GCE Python had no pip, ensurepip or venv support | Minimal host Python packaging | Bootstrap an owned virtual environment with uv and install the hash-pinned artifact requirements there. |
| Bubblewrap failed while configuring loopback | The host's AppArmor user-namespace policy restricted capabilities | Keep host policy unchanged. Explicitly configure the bounded Docker renderer; never fall back to unsandboxed conversion. |
| LibreOffice exited without a PDF in the container | The `gen` backend attempted to open an X display | Use the headless `svp` backend. |
| Parallel Rust tests intermittently reported no Git author | A fixture changed global Git environment variables while other tests ran | Remove the fixture mutation and run verification with an explicit sterile Git environment. |
| A pre-push check read fixture commits from the integration repository | Git exports repository and index context into hooks; nested checks inherited it | Clear every Git-local environment variable before checks start. A regression proves another scratch repository stays separate. The refused push's fixture commit was retained under a backup reference, and the branch and index were restored to the saved integration commit without rewriting working files. |
| A schema test could not validate an operation response | A generic JSON root concealed the actual operation shape | Use the typed flat operation response and namespaced workspace schema roots. |
| Bucket creation rejected `--labels` | The installed gcloud required a separate label update | Create, retain the exact creation receipt, then update labels. Refuse to adopt an ambiguous pre-existing bucket. |
| The evaluation database allocated 500 GiB | The pinned upstream database manifest has a separate PVC | Use the supported smaller evaluation override, record PVC/PV identities and verify disk reclamation during cleanup. |
| A Ready Substrate worker had no scheduling capacity after replacing the empty database | Upstream reports worker capacity once at process startup | Replace only the owned worker Pod after verifying its UID and empty assignment state; confirm capacity registration before retrying. |
| Concurrent session admission returned an internal revision conflict | Workspace and tenant-quota compare-and-set updates competed | Retry rolled-back admission conflicts with the same request key after revalidating access, state and quota. The synchronized regression now passes on the integrated source. |
| An AGY follow-up lost its original tool permissions | Console continuation rebuilt the default-deny policy instead of retaining the session policy | Persist and reapply the validated session policy while preserving current operator and caller checks. Command-requiring allow, deny, legacy-session and reviewer regressions now pass. The stale-result guard refused acceptance after the incomplete turn. |

## Remaining product gaps

The console's new-session form supports model selection and explicit tool
acknowledgment. Remote provider options currently use the typed API, Python
client or manager MCP; a local executable list is not proof that a remote
worker, template or model login is ready. Operator-approved execution presets
would give the browser a useful remote selector without asking users to enter
server credential paths.

Source names currently use an ASCII header representation. Preserve arbitrary
Unicode titles through an explicit encoded metadata contract before claiming
full filename support. Source preview routes exist; the Sources panel currently
offers downloads. Active application previews need a separate origin and are
not served as trusted console HTML.

Use the [workspace manager](workspace-management.md),
[runtime guide](workspace-runtime.md), [preview guide](artifact-previews.md) and
[GKE guide](substrate-gke.md) for the supported controls. The earlier
[remote validation record](remote-validation-2026-10-28.md) covers its own
Microsandbox, recipe and model runs; it does not replace validation of this
release candidate.
