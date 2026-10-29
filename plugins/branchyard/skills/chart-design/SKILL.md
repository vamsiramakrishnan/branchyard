---
name: chart-design
description: "Choose truthful chart encodings with explicit units, uncertainty and source data, using editable charts where supported."
---
# Design a chart

Start with the comparison the reader needs and inspect the source data. Read [chart guidance](references/charts.md) for encoding checks. Bars suit category comparisons, lines ordered change, and tables exact lookup.

Keep units, denominators, time periods and series definitions explicit. Show missing values and uncertainty when relevant. Distinguish scenarios, forecasts and observations. Preserve source data and transformation code.

Cite local evidence with a section-relative source path, SHA-256 and locator; use a URL for a web source. Never invent a URL for a local file. Bar and column axes include zero in the shared renderer.

Use the installed section/deck example for editable PowerPoint column, bar, line or pie charts and embedded workbook data. For other chart types, use an established plotting library and retain its script; label static images accurately.

Reopen actual chart series and compare values with the source. Render at the intended reading size and check labels, legends, contrast and number formatting. Supply an accessible data table when needed.
