# Data quality decisions

Adapted from K-Dense's exploratory-data-analysis skill. Changes: a short CSV cleaning guide for Branchyard's supported operations; domain inspectors and environment-specific installation instructions omitted.

Preserve raw inputs. Inspect identifiers, missing-value codes, units and row keys before transformations. Load identifier columns as strings. Distinguish absence from literal strings such as NA. State any exclusions and why they are justified.

Choose conversion and duplicate policies explicitly. Ambiguous values need resolution rather than silent coercion. Check before/after row counts, missingness and domain totals. Outliers are observations to investigate; their magnitude alone does not justify deletion.

For broader analysis, use pandas directly and retain the executable transformation, source hash and output hash. An exploratory result does not establish causation or validate a forecast.

Source: https://github.com/K-Dense-AI/scientific-agent-skills/blob/5f2bc0341354fd5590aef76a533ada1e1e5b4e62/skills/exploratory-data-analysis/SKILL.md
License: [MIT](LICENSE.k-dense.txt).
