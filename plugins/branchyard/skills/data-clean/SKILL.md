---
name: data-clean
description: "Normalize local CSV data with explicit trim, null, type and duplicate policies while preserving identifiers and recording changes."
---
# Clean data

Run `knowledge data-clean clean.json --out outputs/cleaned` through artifact-tools. The new directory contains data.csv and its receipt. Retain the source. Read the request example linked from this folder's index when preparing a spec.

Declare columns to trim, explicit null tokens, column types and duplicate handling. Default loading preserves `00012` and literal `NA`. Choose record keys before deduplication. Resolve failed conversions rather than silently discarding rows, imputing values or converting identifiers to numbers.

Inspect source/output hashes, before/after counts, representative changed records and domain totals. For joins or complex transformations, use pandas directly and preserve the script, assumptions and output manifest. Read [data quality guidance](references/data-quality.md) when selecting policies.

The number type uses binary floating point. Keep identifiers and exact decimal amounts as strings, or use ordinary Decimal code when the task requires decimal precision. Formula-like CSV strings remain data; no spreadsheet escaping is applied.
