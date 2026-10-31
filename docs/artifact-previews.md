# Document previews

The workspace console can derive a table or page image from an immutable
artifact. Downloading the original remains a separate action. A preview does
not change its source, accept a deliverable or establish visual approval.

Status, 29 October 2026: hermetic cache, converter and HTTP fixtures pass with
fake namespace commands, including current membership, original preservation,
authenticated page images and revocation during conversion. The explicit
multi-sheet XLSX fixture also passes with the artifact-runtime Python. Live
converter CLI checks used Bubblewrap 0.9.0, LibreOffice 24.2.7.2 and Poppler
24.09.0 on synthetic CSV, XLSX, DOCX and PPTX documents. Network access was
blocked and the originals stayed unchanged. The [dated converter record](evidence/console-document-converters-2026-10-29.json)
contains the cases and original digests. Not done: complete console browser
acceptance, model turns or qualification on other remote platforms.

Status, 29 October 2026: live native converters and authenticated HTTP passed
on a GCE Ubuntu 24.04.4 server with Docker 29.1.3 and a pinned artifact image.
CSV and XLSX tables, XLSX print preview, DOCX pages and PPTX slides passed.
Original download digests were unchanged, unauthenticated metadata and image
requests returned 401, and the temporary server and containers stopped cleanly.
The [GCE renderer record](evidence/console-document-gce-2026-10-29.json) contains
the image identity, observed resource limits, versions and source digests.
Not done: browser acceptance, GKE renderer qualification, model turns or
production load testing.

Status, 30 October 2026: hermetic. A table preview warned "Truncated columns to
100" for every XLSX sheet, however narrow, because read-only rows were padded to
101 cells before the check. `tests/test_artifact_previews.py` now covers a
three-column workbook, a styled empty cell and a value in column 150, a value in
column 101, a stored dimension that understates the data, and CSV rows padded
with empty fields. With Python 3.12.3 and openpyxl 3.1.5 it failed 5 tests on
the module before the change and passed 19, with 1 skipped, after it. Not done:
other spreadsheet writers, and a live renderer on GCE or GKE.

## Open a preview

An authorized client requests:

```text
GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview
GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview/assets/{image}
GET /v1/workspaces/{workspace}/sources/{source}/preview
GET /v1/workspaces/{workspace}/sources/{source}/preview/assets/{image}
GET /v1/workspaces/{workspace}/artifacts/{artifact}/preview
GET /v1/workspaces/{workspace}/artifacts/{artifact}/preview/assets/{image}
```

Both requests need the original artifact's current read grant. A cached image
has no independent sharing capability. Workspace aliases apply workspace
membership in addition to the original grant. Do not put bearer tokens in image
URLs; fetch bytes with the authenticated client and display a Blob URL.

The server checks the exact credential and current membership before loading
the original and again after conversion. A permission revoked during conversion
blocks the response. Preview JSON and page images carry `private, no-store` and
`nosniff`. Legacy routes and workspace aliases share the same conversion slots
and derivative cache.

Workspace detail includes `current_subject`, `current_role` and permitted actions
under `capabilities`. It also reports `max_upload_bytes` and `max_source_bytes`
so an upload form can show the server's limits before sending a document.

Query options apply to both requests:

- `view=auto`, `table` or `pages`. Auto selects tables for CSV/XLSX and pages
  for documents and raster images.
- `page=1` selects a document page or presentation slide, starting at one.
- `sheet=NAME` selects an XLSX sheet.
- `offset=0&limit=100` selects table rows. The maximum limit is 500 and maximum
  offset is 100000. Table cells remain inert, including formula-looking text.

The table response includes columns, rows, selected sheet, next offset and
warnings. The page response includes a PNG basename, dimensions, selected page,
total page count and warnings. Use the same options when requesting its image.
A table shows at most 100 columns. The warning "Truncated columns to 100"
means a cell beyond column 100 holds a value. Empty cells there do not count:
trailing empty CSV fields, or workbook cells that carry only a style. An XLSX
sheet is read row by row at each row's own width, so a stored dimension that
understates the used range hides nothing.
CSV and XLSX values are shown without executing formulas. XLSX print preview,
DOCX pages and PPTX slides use the native rendering helper; PDF and raster
images use the same bounded page response.

Table JSON has a 2 MiB encoded response budget. A wide table can return fewer
rows than `limit`; continue at `next_offset`. The response includes a warning,
and cell strings remain unchanged. Plain text and Markdown use an inert text
response. JSON-only modes need no image output directory from the helper.

Missing tools produce `preview_unavailable` with setup guidance. Busy converters
produce `preview_busy` with a retry interval. Conversion timeouts produce
`preview_timeout`. These failures preserve the original download. They never
enable an unsandboxed fallback or install software.

## Prepare the rendering server

Use a Linux server with Bubblewrap, Python and the explicit
[artifact runtime](artifact-toolkit.md#installed-skills-and-runtime). Rendering
Office documents needs LibreOffice, Poppler and fonts. Select executables before
starting the server:

```sh
export BRANCHYARD_PREVIEW_BWRAP=/usr/bin/bwrap
export BRANCHYARD_PREVIEW_PYTHON=/srv/branchyard/artifacts/bin/python
```

The default backend is Bubblewrap. Its defaults are `bwrap` and `python3` on the server's system path. Python may be
a dedicated virtual environment. The service binds its reported installation
and base installation read-only; it refuses a prefix that exposes `/`, a home
directory, `/tmp` or `/etc`. It does not bind the operator's home or credentials.
Other operating systems receive an unavailable response with Linux renderer
guidance.

The converter gets a fresh user, PID, IPC, UTS and network namespace, private
temporary directory and home, a cleared environment, read-only source and helper,
and one writable output directory. Minimal system executables/libraries, fonts,
loader configuration, `/etc/libreoffice` when installed, and the selected Python
installation are available.
The helper disables Office macros and external updates; OS network isolation
holds even if document content attempts an external fetch.

## Container rendering on hosts with restricted user namespaces

On some Ubuntu hosts, AppArmor permits an unconfined application to create a
user namespace but denies capabilities inside it. Bubblewrap can then fail at
`loopback: Failed RTM_NEWADDR: Operation not permitted`. Do not remove network
isolation or disable the host's AppArmor policy to make a document render.
The [Ubuntu release notes](https://documentation.ubuntu.com/release-notes/24.04/)
describe this capability restriction. Bubblewrap's
[loopback setup](https://github.com/containers/bubblewrap/blob/v0.9.0/network.c)
performs the denied route-netlink operation.

An operator can explicitly select Docker before starting the rendering server:

```sh
export BRANCHYARD_PREVIEW_BACKEND=docker
export BRANCHYARD_PREVIEW_DOCKER=/usr/bin/docker
export BRANCHYARD_PREVIEW_DOCKER_IMAGE="$(docker image inspect --format '{{.Id}}' branchyard-artifacts)"
export BRANCHYARD_PREVIEW_DOCKER_PYTHON=/usr/local/bin/python3
```

Use your preprovisioned image name in the inspect command. It returns the local
image's full lowercase SHA-256 content ID. Provision the [artifact runtime image](artifact-toolkit.md) beforehand.
Tags, registry URLs and images declaring writable volumes are refused. Each
request verifies and invokes the immutable local image ID. It never pulls an
image. The selected Python must have the native artifact dependencies;
LibreOffice, Poppler, fonts and GNU `timeout` must be installed in the image.
Container rendering sets `SAL_USE_VCLPLUGIN=svp` for headless LibreOffice.

This backend reaches only the local Unix daemon at `/var/run/docker.sock`.
Its Docker client uses an empty private configuration and a cleared environment.
It inherits no registry credentials, Docker context, remote daemon or SSH agent.
Daemon access is an operator privilege. Request data cannot select the image,
executable, daemon, mounts or command.

The container has no network, a read-only image, no Linux capabilities,
`no-new-privileges`, non-root UID/GID 65532, no healthcheck and no persistent
container logs. Source bytes and the embedded native helper are sent through
stdin into private tmpfs. No host paths, home directories, credentials, sockets
or writable host directories are mounted. `/tmp` is limited to 256 MiB and
`/output` to 64 MiB; both are `noexec,nosuid,nodev`. The container is limited to
768 MiB memory without swap, one CPU, 128 processes and 128 open files. Docker's
[resource limits](https://docs.docker.com/engine/containers/resource_constraints/)
and [tmpfs limits](https://docs.docker.com/engine/storage/tmpfs/) enforce those
bounds.

An in-container watchdog kills conversion at 40 seconds independently of the
HTTP server. The attach operation has a 45-second deadline. Cleanup targets only
the exact container ID returned by this request's successful creation, with a
10-second process deadline. Containers auto-remove on exit. The server keeps
source staging, conversion admission and cleanup alive after an HTTP client
disconnects. A server crash loses the response; the container watchdog still
bounds running work. A crash between creation and start can leave a stopped
container bearing the `io.branchyard.purpose=document-preview` label.

Only bounded JSON metadata and a bounded PNG cross stdout. The standard JSON and
base64 libraries carry the PNG, and the existing image/path validators run before
cache publication. The 2 MiB metadata and 16 MiB decoded PNG limits remain
separate from the larger transport envelope. Unavailable Docker, a missing image
or failed isolation produces an unavailable preview. It does not change backends.

## Bounds and cache behavior

The server accepts at most 64 MiB of original bytes. Request admission bounds
original copying and conversion together to two requests. Blocking readers keep
their admission even if the client disconnects; malformed image names are
rejected before the original is copied. The converter has a 45-second wall deadline, 35-second
CPU limit, 2 GiB address-space limit, 32 MiB per-file limit and bounded output
pipes. Timeout and normal exit tear down its process group. A disconnected request
retains its conversion slot until the bounded conversion and cleanup complete.
The format helper also checks archives, page counts, dimensions and table sizes.

Successful derivatives are published by atomic directory rename. Their key binds
the authenticated tenant, source identity and bytes, format, options, embedded
renderer payload, package version and Python runtime, or the Docker image content
ID and its selected interpreter. Failed or unavailable
conversions are not cached. Cache entries are private, expire after one day,
and are evicted before exceeding the entry/byte budget. Each rendering server
owns its derivative cache; the cache is not a source of artifact authority.

Servers sharing a cache root use one short cross-process file lock for lookup,
eviction and publication. Conversion runs outside that lock. Image bytes are
read while pinned by the lock, so eviction cannot invalidate an in-flight image
response. A missing or incomplete entry is derived again from the original.

Server adapters call `PreviewService::admit()` or `admit_asset(image)` before
loading original bytes, then use that permit's `preview` or `asset` method.
`authorized_input` requires the permit and keeps its materialization guard in
the blocking reader. Other blocking source loaders must retain
`permit.materialization_guard()` in their closure too. The ordinary service
methods remain available for inputs that are already materialized.
