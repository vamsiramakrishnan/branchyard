---
name: pptx
description: "Create, inspect, update and validate editable PowerPoint decks, including native charts and rendered layout checks."
---
# PowerPoint presentations

Use the toolkit command in the task's capabilities index. In a plugin installation, resolve the sibling `artifact-tools/scripts/run.py` from this skill directory. Run `doctor` first when runtime availability is unknown. The toolkit emits JSON with separate structural, rendered and calculated checks. Missing required libraries, fonts or renderers are unmet prerequisites; never turn an unavailable check into a pass. See the shared `artifact-tools` skill for runtime and publication details.

Use `pptx create SPEC --out FILE`, `pptx read FILE`, `pptx update FILE SPEC --out NEW_FILE` and `pptx check FILE`. Use the installed section/deck schemas for editable text, images, tables and charts. Preserve the requested template, sources and narrative; a file that opens is not a completed presentation review.

Chart data needs explicit units and display formats. Use native editable charts when requested. Choose available fonts, constrain text and shape bounds, reset footer margins/paragraph spacing, and inspect actual rendered slides for overflow and missing glyphs. Required rendering must fail as unavailable if the renderer is missing.

For parallel drafting, load `parallel-deck`: each worker owns a section spec and assets, and one renderer assembles the presentation. Avoid combining binary PPTX files through unverified XML copying, which can lose theme, media and relationship references.
