# Optional artifact toolkit

`branchyard_artifacts` creates and checks real Office and PDF files. Each format
has its own module and request format. The package shares file checks, atomic
writes and the versioned section/deck contracts. It does not run a model or
replace a harness's planning or native subagents.

The [knowledge and design guide](knowledge-skills.md) covers source snapshots,
table extraction, explicit data cleaning, unit-aware calculation, citation
integrity and visual review, with progressively loaded design guidance.

## Installed skills and runtime

<!-- fact:document.skills -->

| Skill | Purpose | Requires |
|---|---|---|
| `artifact-tools` | Find and run knowledge artifact tools, check dependencies, prepare the runtime and capture output files. | none |
| `docx` | Create, inspect, update and validate editable Word DOCX documents, preserving source files and checking layout when needed. | `artifact-tools` |
| `xlsx` | Create, inspect, update and validate Excel workbooks, formulas, calculated values and native pivot tables. | `artifact-tools` |
| `pdf` | Create, extract, overlay and validate PDFs, with rendered checks and explicit limits on document editing. | `artifact-tools` |
| `pptx` | Create, inspect, update and validate editable PowerPoint decks, including native charts and rendered layout checks. | `artifact-tools` |
| `google-docs` | Create, read and update native Google Docs through an authorized connector, checking actual document identity and revisions. | `artifact-tools` |
| `google-sheets` | Create, inspect and update native Google Sheets, including ranges, calculations and pivots supported by the granted connector. | `artifact-tools` |
| `google-slides` | Create, inspect and update native Google Slides through a granted connector while preserving presentation identity, layouts and permissions. | `artifact-tools` |
| `parallel-deck` | Split a large presentation into independently owned section specifications and assemble checked editable slides with one renderer. | `pptx` |
| `source-capture` | Snapshot retrieved local sources with origin metadata, exact hashes and page, paragraph or cell anchors for later evidence checks. | `artifact-tools` |
| `table-extract` | Extract local PDF, DOCX and XLSX tables into reviewable records with source hashes and page, table or sheet locations. | `artifact-tools` |
| `data-clean` | Normalize local CSV data with explicit trim, null, type and duplicate policies while preserving identifiers and recording changes. | `artifact-tools` |
| `calculate` | Evaluate reproducible arithmetic with named quantities, explicit units and assumptions using Pint, recording inputs and results. | `artifact-tools` |
| `claim-verify` | Check source hashes, reproduced extraction anchors and exact quotations while keeping factual support and semantic review explicit. | `artifact-tools` |
| `inspect-layout` | Render document pages and presentation slides into inspectable previews, keeping rendering distinct from visual review. | `artifact-tools` |
| `visual-design` | Apply coherent visual hierarchy, typography, spacing and color to reports, presentations and other knowledge artifacts. | `artifact-tools` |
| `chart-design` | Choose truthful chart encodings with explicit units, uncertainty and source data, using editable charts where supported. | `artifact-tools` |
| `diagram-design` | Create editable process, relationship and system diagrams using Mermaid or established drawing tools. | `artifact-tools` |

<!-- /fact -->

`by init plugin` installs the canonical skills for the selected native harness.
The standalone skill installer resolves dependencies: installing `parallel-deck`
also installs `pptx` and `artifact-tools`, after checking all destinations for
conflicts. The toolkit payload is embedded in the CLI; no source checkout is
needed for managed tasks.

Every task, including a leaf without delegation rights, receives a small
capabilities-index notice. The root `INDEX.md` routes to topic indexes, then
individual skill folders and their references, examples and tools. Paths are
relative inside the installed tree; the prompt carries only the root pointer.
Assets are staged before a sandbox is created or its home is
transferred. Local tasks without private homes use a separate per-branch cache
under the yard state directory; Branchyard does not change the user's global
skill installation to provision a task. Each harness keeps its own planning,
subagents and internal graph.

Run the installed helper's `doctor`. When Python libraries are missing, run
`python3 <artifact-tools>/scripts/run.py setup` with the path from that index.
This explicitly creates a private virtual environment and runs pip against the
bundled hash-pinned dependency lock. Ordinary document commands never install
software. Repeating setup preserves a matching ready environment and repairs an
interrupted install. A ready receipt binds the dependency-lock digest and Python
version; partial environments are not selected.
Linked cache paths are refused. Setup does not install system packages: native rendering and pivots need the image below or an
explicit system installation of LibreOffice, poppler, fonts and python3-uno.

If setup reports that `ensurepip` is unavailable on Debian or Ubuntu, install
the `python3-venv` package for your Python version, then rerun setup. For example,
Python 3.12 needs `python3.12-venv`. Setup reports this requirement; it does not
install operating-system packages for you.

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
pins compatible transitive libraries and conditional dependencies. Regenerate
the universal lock with `uv pip compile --python-version 3.9 --universal
--generate-hashes deploy/artifacts-requirements.in
-o deploy/artifacts-requirements.lock`. Patched PDF table extraction requires
Python 3.10 or newer; it is explicitly unavailable on 3.9.
The image installs LibreOffice, its system Python UNO bridge, fonts and Poppler.
Operating-system packages still follow the Debian image's package repository.
Building this image is an explicit installation step.

The Python 3.9 and 3.12 document CI jobs also install the separate hash lock
`deploy/artifacts-test-requirements.lock`. It pins the wheel build tools and
the `tomli` test parser for Python before 3.11. The jobs require those build
tools before running the installed-wheel test; a missing tool cannot turn
that test into a passing skip. Regenerate this lock with `pip-compile
--allow-unsafe --generate-hashes --no-emit-index-url --no-emit-trusted-host
--output-file deploy/artifacts-test-requirements.lock
deploy/artifacts-test-requirements.in` under Python 3.9 so its conditional
parser is included. These tools add no dependencies to the base SDK.

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

## Bounded document previews

```sh
python3 -m branchyard_artifacts preview --file outputs/data.xlsx --out outputs/preview --view auto --page 1 --sheet Data --offset 0 --limit 100
python3 -m branchyard_artifacts preview --file outputs/brief.pptx --out outputs/slide-preview --view pages --page 1
```

`preview` inspects one local file and prints one JSON object without modifying
the source. `--view auto` selects `table` for CSV and XLSX, `pages` for DOCX,
PPTX, PDF and raster images, and `text` for UTF-8 `.txt`, `.md` and `.json`
files. XLSX also supports `--view pages` for paginated print previews.

Table previews (`kind: "table"`) return `columns`, `rows`, `sheets`, `sheet`,
`offset`, `next_offset` and `warnings`. CSV files are read with the standard
library `csv` parser; strings, leading zeros (`"00012"`), inert markup and
formula-looking prefixes (`=`, `+`, `-`, `@`) stay literal strings and are
never evaluated. XLSX table previews use `openpyxl` in read-only mode with
`data_only=False` and `keep_links=False`: worksheet formulas are returned as
stored formula strings rather than fabricated or stale cached values, and a
warning notes that formulas were not evaluated. Rows, columns and per-cell text
are bounded, with explicit truncation warnings and offset pagination.

Poppler limits raster dimensions before allocating an image, including for PDFs
with unusually large pages.
Page previews (`kind: "pages"`) convert Office files to PDF in a fresh
per-call LibreOffice user profile with macros disabled (`MacroSecurityLevel=3`)
and external link updates disabled, then rasterize only the requested 1-based
`--page` with Poppler (`pdftoppm -f PAGE -l PAGE`) and verify/decode the single
PNG with Pillow inside `OUTPUT_DIR`. Raster images are decoded and re-encoded to
a bounded PNG (`pages: 1`); SVG and HTML inputs are refused. Macro-enabled
Office formats (`.xlsm`, `.docm`, `.pptm`, `.xlsb` or archives containing
`vbaProject.bin`), symbolic links, path traversal and archives exceeding entry
or expanded-byte bounds are rejected. Missing optional Python libraries or
native converters exit 3 (`status: "unavailable"`) with installation guidance.
`branchyard_artifacts.preview` enforces file, archive, dimension and subprocess
timeout bounds, while the calling server or runtime provides OS-level process
and network isolation.

## Independent sections and one deck assembler

Charts can cite a web source with `{ "label": "Source", "url": "https://…" }`
or local evidence with `{ "label": "Captured report", "source": {
"path": "evidence/report/source.pdf", "sha256": "<SHA-256>", "locator": "page:1"
} }`. Use exactly one form. Local paths are relative to the section file and
must stay beneath it; validation checks the actual file and digest. Speaker
notes retain the path, digest and locator. Citation identity does not itself
establish that a claim follows from the source. Bar and column axes include zero;
line charts retain their usual automatic scale.

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

The [parallel deck rig](../examples/rigs/parallel-deck.toml) provides section
seats and a lead/assembler. Run it from a configured repository with the
artifact runtime and AGY. This recipe names `GEMINI_API_KEY` for
private-home provisioning: register that secret on the server, or make it
available to the local process. The key is used by AGY; no Gemini CLI runs.
AGY OAuth qualification is separate from this recipe.

```sh
by rig check examples/rigs/parallel-deck.toml --json
by rig run examples/rigs/parallel-deck.toml "Build a 60-slide deck from the supplied brief" --allow-unapproved-tools
```

Copy its guidance folder with the recipe when using another project. Seat
contracts capture section specifications and final files; the harness chooses
how to plan and use its native agents. Reported dollar budgets depend on the
harness's usage reporting. Prepare the brief and sources before a run.

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

Status, 2026-10-29: hermetic local document tests inspect actual Office/PDF bytes,
preservation updates, bounded CSV/XLSX/image/text previews, synthetic converter
page selection, archive/symlink/macro rejections, atomic failures and
deterministic assembly of 60 slides from independently owned section files.
Native local fixtures exercised LibreOffice 24.2.7 UNO pivot/cache records and
computed total, Office conversion plus every-page PNG decoding, and bounded
single-page DOCX/XLSX/PPTX/PDF preview rendering with Poppler 24.02.0. The
Docker image also built on GCE and passed the real-file and installed-wheel
suites with networking disabled; an actual source-free AGY task loaded its
staged skills and required doctor checks. See the
[dated record](atomic-knowledge-validation-2026-10-29.md).
Not done: cross-platform Office application qualification or live Google
Workspace operations.
