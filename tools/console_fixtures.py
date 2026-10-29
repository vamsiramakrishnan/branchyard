#!/usr/bin/env python3
"""Generate deterministic knowledge-work fixture bundles for console checks.

Produces one CSV, one XLSX workbook, one DOCX report and one PPTX deck under a
new output directory, plus a ``manifest.json`` describing each file (media
type, byte size, SHA-256 and expected content) for independent render checks.
Office files are built through the existing ``branchyard_artifacts`` create
APIs; only the CSV uses the stdlib :mod:`csv` module. No OOXML is hand-written.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import sys
import tempfile
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "sdk" / "python"))

from branchyard_artifacts.common import digest, output_directory  # noqa: E402
from branchyard_artifacts.docx import create as create_docx  # noqa: E402
from branchyard_artifacts.pptx import create as create_pptx  # noqa: E402
from branchyard_artifacts.xlsx import create as create_xlsx  # noqa: E402

CSV_NAME = "ticket_backlog.csv"
XLSX_NAME = "backlog_metrics.xlsx"
DOCX_NAME = "backlog_report.docx"
PPTX_NAME = "backlog_briefing.pptx"

# Ticket rows: identifiers keep leading zeros (strings), one field looks like a
# formula and one like HTML; both are inert data.
TICKETS: list[dict[str, str]] = [
    {
        "ticket_id": "00451",
        "summary": "Dashboard load regression",
        "severity": "high",
        "notes": '=SUM(A1:A2) is not a formula, just reported text',
    },
    {
        "ticket_id": "00452",
        "summary": 'Export dialog drops quotes, "fields"',
        "severity": "medium",
        "notes": "<script>alert('reported')</script> seen in the log copy",
    },
    {
        "ticket_id": "00453",
        "summary": "Search latency spike, region eu-west",
        "severity": "low",
        "notes": "Reported by two customers; escalation not required",
    },
]

CSV_FIELDS = ["ticket_id", "summary", "severity", "notes"]


def _write_json(path: Path, value: dict[str, Any]) -> Path:
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")
    return path


def _build_csv(out: Path) -> None:
    with out.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=CSV_FIELDS, quoting=csv.QUOTE_MINIMAL)
        writer.writeheader()
        writer.writerows(TICKETS)


def _xlsx_spec(staging: Path) -> Path:
    """Two named sheets; a live SUM formula on Summary and a column chart."""
    return _write_json(
        staging / "xlsx.json",
        {
            "schema_version": 1,
            "sheets": [
                {
                    "name": "Backlog",
                    "cells": {
                        "A1": "ticket_id",
                        "B1": "severity",
                        "A2": TICKETS[0]["ticket_id"],
                        "B2": TICKETS[0]["severity"],
                        "A3": TICKETS[1]["ticket_id"],
                        "B3": TICKETS[1]["severity"],
                        "A4": TICKETS[2]["ticket_id"],
                        "B4": TICKETS[2]["severity"],
                    },
                },
                {
                    "name": "Summary",
                    "cells": {
                        "A1": "Quarter",
                        "B1": "Tickets closed",
                        "A2": "Q1",
                        "B2": 12,
                        "A3": "Q2",
                        "B3": 19,
                        "A4": "Total",
                        "B4": "=SUM(B2:B3)",
                    },
                    "number_formats": {"B2": "0", "B3": "0", "B4": "0"},
                },
            ],
            "charts": [
                {
                    "type": "column",
                    "sheet": "Summary",
                    "title": "Tickets closed",
                    "unit": "tickets",
                    "number_format": "0",
                    "categories": "A2:A3",
                    "values": "B1:B3",
                    "anchor": "D2",
                }
            ],
        },
    )


def _docx_spec(staging: Path) -> Path:
    paragraphs = [
        {"text": "Backlog review", "style": "Heading 1"},
        {"text": "Three tickets remain open; one is high severity and blocks the console release.", "style": "Normal"},
        {"text": "Findings", "style": "Heading 2"},
        {
            "text": 'Ticket 00451 carries the reported text "=SUM(A1:A2)"; it is data, not a formula.',
            "style": "Normal",
        },
        {"text": "Source: branchyard console fixture generator, illustrative data only.", "style": "Normal"},
    ]
    return _write_json(
        staging / "docx.json",
        {
            "schema_version": 1,
            "title": "Backlog review",
            "paragraphs": paragraphs,
            "footer": "Console fixtures - illustrative data",
        },
    )


def _pptx_specs(staging: Path) -> Path:
    """A three-slide deck through the deck/section manifest path."""
    design = {
        "id": "brief-v1",
        "width_in": 13.333333,
        "height_in": 7.5,
        "font": "DejaVu Sans",
        "title_font_size_pt": 28,
        "footer_font_size_pt": 10,
        "margin_in": 0.5,
        "background": "FFFFFF",
        "foreground": "17202A",
        "accent": "2563EB",
    }
    sections = [
        {
            "schema_version": 1,
            "kind": "section",
            "id": "summary",
            "design_id": "brief-v1",
            "assets": [],
            "slides": [
                {
                    "id": "overview",
                    "title": "Backlog overview",
                    "elements": [
                        {
                            "kind": "text",
                            "box": [0.9, 1.6, 11.0, 2.0],
                            "text": "Three open tickets. One high-severity regression blocks the console release.",
                            "font_size_pt": 22,
                            "space_after_pt": 6,
                        }
                    ],
                    "citations": [{"label": "Illustrative fixture", "url": "https://example.com/fixture"}],
                }
            ],
        },
        {
            "schema_version": 1,
            "kind": "section",
            "id": "metrics",
            "design_id": "brief-v1",
            "assets": [],
            "slides": [
                {
                    "id": "closures",
                    "title": "Tickets closed by quarter",
                    "elements": [
                        {
                            "kind": "chart",
                            "box": [0.7, 1.5, 7.0, 4.8],
                            "chart_type": "column",
                            "title": "Tickets closed",
                            "unit": "tickets",
                            "number_format": "0",
                            "categories": ["Q1", "Q2"],
                            "series": [{"name": "Closed", "values": [12, 19]}],
                        },
                        {
                            "kind": "text",
                            "box": [8.2, 1.5, 4.3, 2.0],
                            "text": "Native editable chart; values are illustrative.",
                            "font_size_pt": 20,
                            "space_after_pt": 6,
                        },
                    ],
                    "citations": [{"label": "Illustrative fixture", "url": "https://example.com/fixture"}],
                }
            ],
        },
        {
            "schema_version": 1,
            "kind": "section",
            "id": "next",
            "design_id": "brief-v1",
            "assets": [],
            "slides": [
                {
                    "id": "actions",
                    "title": "Next actions",
                    "elements": [
                        {
                            "kind": "text",
                            "box": [0.9, 1.6, 11.0, 2.5],
                            "text": "Fix 00451, retest the export dialog, then clear the search latency report.",
                            "font_size_pt": 22,
                            "space_after_pt": 6,
                        }
                    ],
                    "citations": [{"label": "Illustrative fixture", "url": "https://example.com/fixture"}],
                }
            ],
        },
    ]
    section_refs = []
    for index, section in enumerate(sections):
        path = staging / f"section-{index}.json"
        _write_json(path, section)
        section_refs.append({"id": section["id"], "path": path.name, "sha256": digest(path)})
    return _write_json(
        staging / "deck.json",
        {
            "schema_version": 1,
            "kind": "deck",
            "id": "backlog-briefing",
            "design": design,
            "sections": section_refs,
            "slide_order": [
                f"{ref['id']}/{slide_id}"
                for ref, slide_id in zip(section_refs, ["overview", "closures", "actions"], strict=True)
            ],
        },
    )


def _entry(out: Path, name: str, media_type: str, expected: dict[str, Any]) -> dict[str, Any]:
    path = out / name
    data = path.read_bytes()
    return {
        "filename": name,
        "media_type": media_type,
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "expected": expected,
    }


def generate(out: Path) -> dict[str, Any]:
    """Generate the fixture bundle at ``out`` and return the JSON-safe manifest."""
    out = Path(out)
    with tempfile.TemporaryDirectory(prefix="console-fixtures-") as temporary, output_directory(out) as staged:
        specs = Path(temporary)
        _build_csv(staged / CSV_NAME)
        create_xlsx(_xlsx_spec(specs), staged / XLSX_NAME)
        create_docx(_docx_spec(specs), staged / DOCX_NAME)
        create_pptx(_pptx_specs(specs), staged / PPTX_NAME)
        manifest = {
            "generator": "tools/console_fixtures.py",
            "files": [
                _entry(
                    staged,
                    CSV_NAME,
                    "text/csv",
                    {
                        "rows": len(TICKETS) + 1,
                        "columns": CSV_FIELDS,
                        "key_text": [TICKETS[0]["ticket_id"], TICKETS[1]["notes"]],
                        "note": "Leading-zero identifiers and formula/HTML-looking text are inert data",
                    },
                ),
                _entry(
                    staged,
                    XLSX_NAME,
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    {
                        "sheets": ["Backlog", "Summary"],
                        "charts": 1,
                        "formulas": ["=SUM(B2:B3)"],
                        "key_text": ["Tickets closed"],
                    },
                ),
                _entry(
                    staged,
                    DOCX_NAME,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                    {
                        "paragraphs": 5,
                        "key_text": ["Backlog review", "Source: branchyard console fixture generator"],
                    },
                ),
                _entry(
                    staged,
                    PPTX_NAME,
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    {
                        "slides": 3,
                        "charts": 1,
                        "key_text": ["Backlog overview", "Tickets closed by quarter", "Next actions"],
                    },
                ),
            ],
        }
        (staged / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


def main(argv: list[str] | None = None) -> int:
    """Print the manifest JSON only; never binary bytes."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path, help="New output directory for the fixture bundle")
    args = parser.parse_args(argv)
    print(json.dumps(generate(args.out), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
