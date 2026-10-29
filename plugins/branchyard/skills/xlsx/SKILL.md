---
name: xlsx
description: "Create, inspect, update and validate Excel workbooks, formulas, calculated values and native pivot tables."
---
# Excel workbooks

Use the toolkit command in the task's capabilities index. In a plugin installation, resolve the sibling `artifact-tools/scripts/run.py` from this skill directory. Run `doctor` first when runtime availability is unknown. The toolkit emits JSON with separate structural, rendered and calculated checks. Missing required libraries, fonts or renderers are unmet prerequisites; never turn an unavailable check into a pass. See the shared `artifact-tools` skill for runtime and publication details.

Use `xlsx create SPEC --out FILE`, `xlsx read FILE`, `xlsx update FILE SPEC --out NEW_FILE` and `xlsx check FILE`. The installed workbook and pivot examples describe supported ranges, cell types and pivot inputs. Keep user data as literal cells unless a formula is explicitly requested.

Preserve the input workbook and unrelated sheets when updating. Verify formulas and cached numeric values separately. Writing a formula does not calculate it. Require a qualified calculation runtime before reporting recalculated results. Check financial units, percentages, totals and source-row coverage independently of displayed formatting.

`xlsx pivot FILE SPEC --out NEW_FILE` requires LibreOffice UNO. Verify a real pivot table and cache by reopening the XLSX. Report unavailable when that backend is absent; a static aggregation is a separate output. Preserve native pivots, charts and unsupported workbook features only where the selected library's round trip is qualified.
