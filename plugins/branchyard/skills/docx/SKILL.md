---
name: docx
description: "Create, inspect, update and validate editable Word DOCX documents, preserving source files and checking layout when needed."
---
# Word documents

Use the toolkit command in the task's capabilities index. In a plugin installation, resolve the sibling `artifact-tools/scripts/run.py` from this skill directory. Run `doctor` first when runtime availability is unknown. The toolkit emits JSON with separate structural, rendered and calculated checks. Missing required libraries, fonts or renderers are unmet prerequisites; never turn an unavailable check into a pass. See the shared `artifact-tools` skill for runtime and publication details.

Use `docx create SPEC --out FILE`, `docx read FILE`, `docx update FILE SPEC --out NEW_FILE` and `docx check FILE`. Read the installed DOCX example for supported paragraphs, headings, runs, tables and metadata. Use python-docx through the toolkit; do not build ZIP/XML documents by hand.

For an existing document, inspect its structure before an update. Use a separate output and preserve unrelated content and styles within the supported update scope. Report unsupported tracked changes, embedded objects or round-trip fidelity requirements before attempting a lossy edit. Do not flatten a formatted document into extracted text and claim the original formatting survived.

Check that requested edits appear and unrelated content remains. Use required rendering and inspect the generated PDF for page breaks, table headings, clipping and typography. A structural pass alone does not establish pagination or factual accuracy.
