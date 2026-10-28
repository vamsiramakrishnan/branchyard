# Optional artifact toolkit

`branchyard_artifacts` creates and checks real Office and PDF files. Each format
has its own module and request format. The package shares file checks, atomic
writes and the versioned section/deck contracts. It does not run a model or
replace a harness's planning or native subagents.

Install the optional libraries explicitly from a checkout:

```sh
python3 -m pip install './sdk/python[artifacts]'
python3 -m branchyard_artifacts doctor --require docx,xlsx,pdf,pptx,schema,images
```

The base SDK still has no dependencies. Importing `branchyard`,
`branchyard_client`, or `branchyard_artifacts`, and reading CLI help does not
import the optional libraries. Rendering never installs software or fetches an
asset. Every input asset is a local, digest-checked file.

For the explicit native runtime, build from the repository root:

```sh
docker build -f deploy/Dockerfile.artifacts -t branchyard-artifacts .
docker run --rm -v "$PWD:/work" branchyard-artifacts doctor --require render,pivot,fonts
```

The Python runtime dependencies use a checked-in hash lock at
`deploy/artifacts-requirements.lock`. `deploy/artifacts-requirements.in` also
pins transitive libraries compatible with Python 3.9. Regenerate the lock with
`pip-compile --generate-hashes --no-emit-index-url --no-emit-trusted-host
--output-file deploy/artifacts-requirements.lock deploy/artifacts-requirements.in`.
The image installs LibreOffice, its system Python UNO bridge, fonts and Poppler.
Operating-system packages still follow the Debian image's package repository.
Building this image is an explicit installation step.

## Create, inspect and update

Commands print JSON. A successful operation exits 0, a failed request or check
exits 1, and an unavailable requirement exits 3. Structural checks,
recalculation and rendering are separate evidence. A workbook formula is never
described as calculated merely because its formula text was saved.

From a checkout with the optional package installed:

```sh
mkdir -p outputs
python3 -m branchyard_artifacts docx create sdk/python/branchyard_artifacts/examples/docx.json --out outputs/brief.docx
python3 -m branchyard_artifacts xlsx create sdk/python/branchyard_artifacts/examples/xlsx.json --out outputs/data.xlsx
python3 -m branchyard_artifacts pdf create sdk/python/branchyard_artifacts/examples/pdf.json --out outputs/brief.pdf
python3 -m branchyard_artifacts pptx create sdk/python/branchyard_artifacts/examples/deck.json --out outputs/brief.pptx
python3 -m branchyard_artifacts pptx read outputs/brief.pptx
python3 -m branchyard_artifacts pptx check outputs/brief.pptx --require-render --preview-dir outputs/brief-preview
```

The same `create`, `read`, `update INPUT SPEC --out OUTPUT` and `check` pattern
applies to each format. `pptx create` reads a deck manifest. The other create
examples are small format-specific requests. Every request declares
`schema_version: 1`; unknown top-level fields are refused. No request can
name an executable or dependencies.

DOCX requests add paragraphs with explicit before/after spacing, tables,
headers, footers and margins. A template reference names a path and SHA-256.
`docx update` accepts `paragraphs`, an object mapping zero-based paragraph
indices to replacement text, and `append_paragraphs`, using the same paragraph
objects as create. It keeps other tables, headers, footers, media and styles.

XLSX requests name sheets, cell addresses, number formats, tables and optional
native charts. The chart request declares its unit and number format.
`xlsx update` changes cells and formats on existing sheets and can add charts;
other sheets, supported charts and formulas remain. The library refuses loading
warnings about unsupported workbook features rather than claiming it preserved
them. Formula caches are reported separately and can be absent.

PDF requests contain pages with literal titles/paragraphs. Markup-looking text
is escaped. `pdf update` accepts text overlays with zero-based page, text,
`x_pt`, `y_pt` and `font_size_pt`, plus string metadata such as author. It keeps
all pages. It does not rewrite arbitrary PDF text or remove content underneath
an overlay.

`pptx update` takes `replacements: [{slide: 0, shape: 0, text: "Revised title"}]`.
It targets existing text shapes and retains other slides, charts and embedded
workbooks. Use separate output paths to keep originals. If the output names
the input, replacement happens only after writing and checking the complete
new package. Every format stages output beside its destination and replaces
the destination atomically after success. A failure leaves the previous file.

`check --require-render` needs native tools and produces a PDF plus a decoded
PNG for every rendered page in a new preview directory. It reports
`rendered: passed` only after conversion and rasterization succeed. It also
reports `visual_review: not_performed`. Open the previews to assess legibility,
alignment and unintended overlap. A structural or rasterization pass does not
substitute for that review.

## Independent sections and one deck assembler

Read the installed contracts with
`python3 -m branchyard_artifacts deck schema section` or `deck schema deck`.
The same JSON files ship in the wheel, under `branchyard_artifacts/schemas/`.
The packaged `examples/deck.json` and `examples/section.json` show the request.

A worker owns its section JSON and asset directory. It declares a section ID,
design ID, stable slide IDs, local asset references with SHA-256 and citations.
Text, tables, images and native charts have explicit inch boxes. Charts contain
editable category/series values, a title, unit and number format; their slide
must cite a source. Citation links also go into speaker notes.

The lead owns a deck manifest. It binds each section path and digest, fixes the
design and declares the exact slide order as `section-id/slide-id` entries.
The assembler rejects missing/duplicate slides, mismatched IDs/designs, modified
assets, symlinks/traversal, non-finite numbers, invalid dimensions and text that
fails conservative font measurement. Geometry uses inches; font sizes and
paragraph spacing use points. It resolves the requested font through fontconfig
and refuses an unavailable font rather than accepting a substitute.
Content stays inside the design margins and below the title region, starting
at `margin_in + 0.85` vertically. Its bottom stays above
`height_in - margin_in - 0.5`, leaving the citation footer clear. Images retain
their aspect ratio inside the declared box.

```sh
python3 -m branchyard_artifacts deck section-check worker/section.json --deck deck.json
python3 -m branchyard_artifacts deck assemble deck.json --out outputs/deck.pptx
```

A PowerPoint template must match the declared dimensions, contain no slides,
and provide a blank layout. Its masters and relationships remain in the output.
For an existing deck with slides, use targeted updates. Assembly normalizes ZIP
timestamps, document timestamps and embedded chart workbook containers, so the
same inputs produce the same PPTX bytes with the pinned libraries. Independent
workers never write the shared presentation. The lead remains free to choose
its native planning, agents and DAG.

## Native pivot tables

```sh
python3 -m branchyard_artifacts xlsx pivot outputs/data.xlsx sdk/python/branchyard_artifacts/examples/pivot.json --out outputs/pivot.xlsx
```

The pivot spec names source sheet/range, target sheet/cell, row/column fields
and data aggregation. This operation calls LibreOffice's native DataPilot API
through a dedicated system-Python UNO process. It recalculates the workbook,
exports XLSX, checks native pivot and cache parts, and independently reopens
the result. Without Calc and UNO it returns unavailable. It never substitutes
a static summary table.

## Capture outputs after a turn

```sh
python3 -m branchyard_artifacts manifest outputs/brief.docx outputs/data.xlsx outputs/deck.pptx --out outputs/artifacts.json
```

The deterministic manifest contains `schema_version: 1` and `files`, each with
a relative path, display name, media type and SHA-256. Only explicitly named
regular files beneath the manifest directory are accepted. It rejects
symlinks, duplicates, traversal and self-capture. This command does not publish,
choose a Branchyard actor or change grants. The engine's producer-bound capture
can collect the files after provider transfer. Keep native final JSON in the
caller's result schema; artifact metadata is separate.

Spec/manifest SHA-256 values validate input bytes. Branchyard's published
artifact digest is blake3 and its artifact ID names a publication. Keep those
identities distinct.

Status: hermetic local document tests inspect actual Office/PDF bytes,
preservation updates, atomic failures and deterministic assembly of 60 slides
from independently owned section files. Native local fixtures exercised
LibreOffice 24.2.7 UNO pivot/cache records and computed total, and Office
conversion plus every-page PNG decoding. Not done: Docker image build,
cross-platform Office application qualification, live Google operations or a
model-driven deck run.
