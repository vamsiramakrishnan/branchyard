---
name: parallel-deck
description: "Split a large presentation into independently owned section specifications and assemble checked editable slides with one renderer."
---
# Parallel presentation work

Keep planning, native subagents and internal DAGs with the lead's chosen harness. Branchyard supplies independent workspaces, scoped grants, result contracts, immutable artifacts and explicit handoffs.

Freeze a deck design and section/slide inventory using the installed deck and section schemas. Assign each worker distinct section IDs and an asset directory. Workers emit their own section JSON and digest-addressed assets, check with `deck section-check SECTION --deck DECK`, and return a compact structured result. They do not edit the shared presentation.

Let the lead select accepted versions and explicitly share/handoff their artifacts. One assembler runs `deck assemble DECK --out FILE`. Assembly must reject missing, duplicate, extra or reordered inventory, incompatible designs, stale asset hashes, unsafe paths, missing chart units/citations, and invalid geometry. The schema controls layout and sources, not the worker's native reasoning process.

The renderer applies shared fonts, theme, numbering and editable chart formats once. Require the checks the deliverable needs and inspect rendered previews. Check factual consistency and narrative across sections independently of structural validity. Preserve versions for revisions, and rerender only from selected immutable inputs. Native Google Slides assembly uses its authorized connector's supported operations and remains separate from local PPTX output.
