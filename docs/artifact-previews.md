# Document previews

The workspace console can derive a table or page image from an immutable
artifact. Downloading the original remains a separate action. A preview does
not change its source, accept a deliverable or establish visual approval.

Status, 29 October 2026: hermetic Rust fixtures pass, including the real embedded
CSV/text helper through a fake namespace adapter, encoded-byte pagination,
materialization admission, concurrent cache access and process teardown. The
explicit multi-sheet XLSX fixture passes with the artifact-runtime Python.
Evidence: `cargo test --locked --offline -p branchyard-server --lib preview::`
and the explicit ignored XLSX fixture. Not done: actual Bubblewrap/Office page
rendering or workspace-membership HTTP checks in this isolated slice; the parent
console integration records those checks separately.

## Open a preview

An authorized client requests:

```text
GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview
GET /v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview/assets/{image}
```

Both requests need the original artifact's current read grant. A cached image
has no independent sharing capability. Workspace aliases apply workspace
membership in addition to the original grant. Do not put bearer tokens in image
URLs; fetch bytes with the authenticated client and display a Blob URL.

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

The defaults are `bwrap` and `python3` on the server's system path. Python may be
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

## Bounds and cache behavior

The server accepts at most 64 MiB of original bytes. Request admission bounds
original copying and conversion together to two requests. Blocking readers keep
their admission even if the client disconnects; malformed image names are
rejected before the original is copied. The converter has a 45-second wall deadline, 35-second
CPU limit, 2 GiB address-space limit, 32 MiB per-file limit and bounded output
pipes. Timeout, cancellation and normal exit tear down its process group.
The format helper also checks archives, page counts, dimensions and table sizes.

Successful derivatives are published by atomic directory rename. Their key binds
the authenticated tenant, source identity and bytes, format, options, embedded
renderer payload, package version and Python runtime. Failed or unavailable
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
