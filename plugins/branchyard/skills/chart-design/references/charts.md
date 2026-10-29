# Chart decisions

Adapted from K-Dense's scientific-visualization skill. Changes: a compact guide for business and research charts using Branchyard's existing document tooling.

Preserve raw data, exclusions, transformations and missing-value codes. Select an encoding that preserves meaning: bars for categories, lines for ordered observations, scatter plots for paired quantitative values. The convenience deck renderer supports a smaller chart set; use a mature plotting library for other types.

Label units, population, time period and uncertainty. Avoid connecting missing observations or tuning axes to exaggerate differences. Start bar axes at zero unless a clearly explained alternative is essential. Use color redundantly with labels or other encodings.

An interactive hover does not replace readable labels or an accessible data table. Check the delivered static artifact at its intended size. Palette names, resolution values and an automated report do not establish accessibility.

For editable deck charts, copy and adapt the installed section.json example; keep categories, series, unit and number_format consistent with the supplied data. Reopen actual series and embedded workbook values before delivery.

Source: https://github.com/K-Dense-AI/scientific-agent-skills/blob/5f2bc0341354fd5590aef76a533ada1e1e5b4e62/skills/scientific-visualization/SKILL.md
License: [MIT](LICENSE.k-dense.txt).
