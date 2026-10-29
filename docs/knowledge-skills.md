# Knowledge and design skills

Branchyard's knowledge skills turn retrieved sources into reviewable evidence,
tables, calculations and deliverables. They use the same artifact toolkit,
installation and capture path as the document skills. Harnesses keep their native
planning and delegation; each worker owns its output directory.

## Discover only what is needed

Start with the installed `skills/INDEX.md`. It links to small topic indexes,
which link to individual skill folders. A folder's `INDEX.md` routes to its
`SKILL.md`, relevant references and request examples. Read the files needed for
the current task; do not concatenate the library into a prompt.

Managed tasks receive a pointer to their guest-visible index. The staged tree
contains the executable runtime and references without requiring a checkout.
Native harnesses retain their existing frontmatter-based discovery alongside
these indexes. Root indexes have a 4 KiB budget and detail indexes an 8 KiB
budget, enforced by the generator. The canonical catalog and dependency list
are generated into the [toolkit reference](artifact-toolkit.md).

## Prepare the runtime

Use the installed artifact-tools launcher, whose actual path is in its index:

```sh
python3 /path/to/artifact-tools/scripts/run.py setup
python3 /path/to/artifact-tools/scripts/run.py doctor --require data-clean,calculate,pdf-tables,render
```

Setup is explicit and installs the hash-pinned runtime. Ordinary operations do
not download dependencies or call models. An SDK installation can use
`pip install './sdk/python[artifacts]'` and `python3 -m branchyard_artifacts`.
Native Office previews also require LibreOffice, Poppler and the documented
[artifact runtime](artifact-toolkit.md#installed-skills-and-runtime).

The SDK and non-PDF-table knowledge operations support Python 3.9. PDF table
extraction requires Python 3.10 or newer: older pdfminer releases have known
unsafe CMap deserialization, and the patched dependency dropped Python 3.9.
The lock excludes that dependency on 3.9 and the command reports unavailable;
it does not install an older vulnerable parser.

## Build an evidence-backed result

Retrieve a source through the harness's search tools or a granted connector.
Save the bytes locally and copy the source-capture request example from the
installed skill index. Fill in its actual filename, canonical source URI,
title and timezone-qualified retrieval timestamp. Keep credentials and signed
download parameters out of the recorded URI.

```sh
python3 -m branchyard_artifacts knowledge source-capture capture.json --out evidence/report
python3 -m branchyard_artifacts knowledge table-extract report.pdf --out outputs/tables.json
```

Capture publishes a new directory containing original bytes, extraction
segments and an index. Keep the returned index SHA-256 when sharing the bundle.
The origin metadata is supplied, so `retrieval_verified` remains false. Text
extraction preserves locators but does not run OCR or evaluate workbook formulas.
Extraction scope and warnings are recorded. PDF table detection records page
geometry; XLSX extraction reads native named tables, rather than guessing a used
range. No detected tables and suspected image-only scans have distinct reports.

Prepare the installed data-clean and calculate request examples:

```sh
python3 -m branchyard_artifacts knowledge data-clean clean.json --out outputs/cleaned
python3 -m branchyard_artifacts knowledge calculate calculation.json --out outputs/calculation.json
```

Cleaning uses pandas with explicit trim, null, type and duplicate policies.
Identifiers such as `00012` and literal `NA` survive default loading. Integer
conversion rejects fractional values and signed-64-bit overflow. The `number`
type uses binary floating point; use strings, exact integers or ordinary Decimal
code when decimal precision is required. CSV formula-like values are retained
as data. The new output directory commits `data.csv` and its receipt together,
with source/output hashes, policies and change counts.

Calculation uses Pint's parser and unit conversions. The convenience expression
supports named variables, numeric literals, arithmetic and parentheses; use
native Pint/Python for powers, functions or larger models and retain the script.
Currency calculations require an explicit basis and supply no exchange rates.
Dimension mismatch, non-finite values and division by zero fail the operation.
Released labor capacity does not itself establish cash savings.

Prepare a claim-verify request using the captured index path and its returned
hash. Evidence entries name source ID, exact locator and quotation:

```sh
python3 -m branchyard_artifacts knowledge claim-verify claims.json --out outputs/claims.json
```

The checker verifies source and extraction hashes and reproduces extraction
before matching quotes. Modified receipts cannot silently invent quotations.
Extraction must reproduce with the installed pinned library version. An intact
quotation establishes citation integrity; semantic support, conflicting evidence
and causal interpretation still require an attributed review.

## Design and inspect

Use visual-design to establish typography, spacing and a shared layout before
parallel drafting. Chart-design guides encoding, units, source data and editable
charts. Diagram-design uses editable Mermaid source or an existing drawing tool;
an absent optional renderer is reported explicitly. References and examples load
only when needed.

```sh
python3 -m branchyard_artifacts knowledge inspect-layout outputs/report.pptx --out outputs/layout
```

This reuses the existing native renderer and emits every page's preview. Inspect
the actual images before recording visual findings. Successful rendering keeps
`visual_review: not_performed`; quotation checks keep
`semantic_review: not_performed`. Use a new preview directory after a revision.
Publish checked files with the existing [capture contract](artifact-capture.md).

## Reused libraries and upstream skills

The implementation uses python-docx, openpyxl, python-pptx, pypdf, pdfplumber,
pandas, Pint and the existing LibreOffice/Poppler path. It adds no formula parser,
OCR engine, skill installer, authentication layer or harness planner.

The upstream review on 2026-10-29 covered these established catalogs:

| Source | Use in Branchyard |
|---|---|
| [OpenAI skills](https://github.com/openai/skills) | Pinned Apache-2.0 PDF review reference |
| [K-Dense scientific-agent-skills](https://github.com/K-Dense-AI/scientific-agent-skills) | Adapted MIT data-quality, units, evidence-appraisal and chart guidance |
| [Anthropic skills](https://github.com/anthropics/skills) | Adapted Apache-2.0 canvas composition guidance; restrictive document skills were excluded |
| [Vercel skills](https://github.com/vercel-labs/skills) | Existing discovery/install CLI for broader ecosystem exploration; no replacement registry |
| [Composio awesome-claude-skills](https://github.com/ComposioHQ/awesome-claude-skills) | Discovery catalog reviewed; no bundled material or connector credentials reused |

The installed `upstream.lock.json` records exact upstream commits and file
digests, local modifications, installed digests and license paths. Copies retain
their applicable licenses. `tools/artifact_assets.py` verifies installed bytes
and generates indexes and embeds; it performs no network request. Refresh an
upstream reference deliberately and review its new license and behavior before
updating the record.

## Friction found during implementation

The dated [verification record](evidence/knowledge-skills-2026-10-29/verification.json)
records real local file/CLI checks, the independent forward workflow and the
corrected chart/citation behavior. Its fixtures are illustrative, not empirical
financial findings.

| Developer experience | Result |
|---|---|
| Loading every skill to find one capability | A root index routes through topics; only the selected skill and reference need loading |
| Request examples buried in a shared runtime | Skill-folder indexes link directly to their relevant installed examples |
| Assuming a repository-wide license covers document skills | Per-file review, pinned provenance and installed license copies; restrictive material excluded |
| Keeping Python 3.9 by selecting an older PDF parser | Patched PDF extraction is explicit on Python 3.10+; other supported operations retain 3.9 |
| Silent fractional-to-integer conversion in pandas | Exact integer validation rejects truncation and overflow before publishing output |
| Local evidence could not satisfy a web-only slide citation | Section-relative file, SHA-256 and locator citations are supported and checked |
| Automatic bar axes exaggerated a small difference | Bar and column axes include zero; reopened-chart and rendered checks verify the fix |
| Missing optional diagram renderer | Keep editable source and report the missing established renderer; no implicit installation |

The native Office runtime remains an explicit prerequisite. Use doctor before
starting work or provision the artifact image. A missing Google Workspace
account connection still needs the existing upstream consent flow.

Hermetic verification uses synthetic sources, actual pinned libraries and local
CLI/native-render invocations in `tests/test_knowledge_evidence.py`,
`tests/test_knowledge_analysis.py` and `tests/test_knowledge_cli.py`. The artifact
CI matrix requires the relevant libraries before running these tests, along with
installed-wheel and source-free skill checks. Not done: live model qualification
of these newly added skills, OCR, automatic factual entailment, or Google
Workspace consent through these new workflows.
