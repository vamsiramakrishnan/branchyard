---
name: table-extract
description: "Extract local PDF, DOCX and XLSX tables into reviewable records with source hashes and page, table or sheet locations."
---
# Extract tables

Run `knowledge table-extract input.pdf --out outputs/tables.json` through artifact-tools. PDF extraction uses pdfplumber; Word and workbook inputs use existing format libraries. PDF table extraction requires Python 3.10 or newer with the pinned runtime. DOCX/XLSX extraction remains available on Python 3.9.

Review rows alongside their source page or worksheet. Preserve source hashes, page/table identifiers, bounding boxes where available and original cell text. Detection can split merged cells or lose multi-page headers. Distinguish a text-only page with no tables from an image-only scan needing OCR.

Use the mature library directly when task-specific table settings are needed, and retain the settings and tool version. Check headers, totals and units before analysis. Successful extraction does not establish completeness or semantic correctness.
