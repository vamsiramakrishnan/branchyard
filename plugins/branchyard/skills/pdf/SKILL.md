---
name: pdf
description: "Create, extract, overlay and validate PDFs, with rendered checks and explicit limits on document editing."
---
# PDF documents

Use the toolkit command in the task's capabilities index. In a plugin installation, resolve the sibling `artifact-tools/scripts/run.py` from this skill directory. Run `doctor` first when runtime availability is unknown. The toolkit emits JSON with separate structural, rendered and calculated checks. Missing required libraries, fonts or renderers are unmet prerequisites; never turn an unavailable check into a pass. See the shared `artifact-tools` skill for runtime and publication details.

Use `pdf create SPEC --out FILE`, `pdf read FILE`, `pdf update FILE SPEC --out NEW_FILE` and `pdf check FILE`. Consult the installed PDF example for supported literal text and page operations. Use ReportLab and pypdf through the toolkit.

Read page count and extracted text before changing content or metadata. Preserve the source and check page ordering, rotations and content in the result. For page selection or merging, use the installed pypdf library directly; the toolkit CLI currently covers creation, reading, overlays and checks. Render pages when appearance matters. Text extraction does not establish OCR quality, and an overlay does not redact underlying content.

For paragraph reflow or unsupported PDF edits, prefer editing an available source document and exporting a new PDF. Report missing source/OCR/redaction capabilities accurately instead of silently degrading the requested operation.
