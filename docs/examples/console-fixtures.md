# Console fixtures

Written 2026-10-29.

`tools/console_fixtures.py` generates a small, deterministic knowledge-work
fixture bundle for independent console and renderer checks. It creates one CSV
(stdlib `csv`), one XLSX workbook, one DOCX report and one PPTX deck through the
existing `branchyard_artifacts` create APIs, under one new output directory.
No OOXML or ZIP is hand-written and no new dependency is added.

```console
$ python3 tools/console_fixtures.py --out /tmp/console-fixtures
```

The command prints only the JSON manifest; binary bytes are never printed.

## Bundle contents

| File | Format | Key fixtures |
| --- | --- | --- |
| `ticket_backlog.csv` | CSV | Leading-zero identifiers (`00451`), a quoted field with commas and embedded quotes, formula-looking (`=SUM(A1:A2)`) and HTML-looking (`<script>`) text as inert data |
| `backlog_metrics.xlsx` | XLSX | Two named sheets (`Backlog`, `Summary`), a live `=SUM(B2:B3)` formula and a native column chart |
| `backlog_report.docx` | DOCX | `Heading 1` and `Heading 2` headings, body paragraphs and a trailing source note |
| `backlog_briefing.pptx` | PPTX | Three slides, including a native editable column chart |
| `manifest.json` | JSON | Per-file media type, byte size, SHA-256 and expected content (row/sheet/slide counts, key text) |

## Behavior

- `generate(out)` is callable as a function and returns the JSON-safe manifest.
- The output directory must not already exist and may not be a symlink; the
  bundle is staged in a sibling temporary directory and renamed into place only
  after every file succeeds (`branchyard_artifacts.common.output_directory`).
- A refused destination leaves any existing directory untouched.
- Originals under `sdk/python/branchyard_artifacts/examples` are read as
  reference and never modified.

## Tests

`tests/test_console_fixtures.py` reopens every generated artifact
(`csv`, `openpyxl`, `python-docx`, `python-pptx`), checks each manifest
hash and size, verifies the three-slide inventory, the two sheets, the inert
CSV data, and the refusal of existing and symlink destinations. Tests that
need an optional library are skipped with a message when it is missing.
