---
name: inspect-layout
description: "Render document pages and presentation slides into inspectable previews, keeping rendering distinct from visual review."
---
# Inspect layout

Run `knowledge inspect-layout report.pptx --out outputs/layout` through artifact-tools. The destination must be new. Existing LibreOffice/Poppler rendering emits a PDF and a PNG for every page. Native requirements are reported explicitly; ordinary operations never install them.

Inspect every delivered page. Use a contact sheet to locate problems, then open dense or suspect regions at readable scale. Check clipping, overlaps, glyphs, chart labels, contrast and page breaks. Re-render after edits and inspect changed output.

Successful rendering records `visual_review: not_performed`. Record your reviewed page inventory and findings separately. File counts and structural checks do not prove visual quality. For PDFs read the [pinned OpenAI reference](references/upstream-pdf.md) as needed; Branchyard's runtime and existing permissions govern execution.
