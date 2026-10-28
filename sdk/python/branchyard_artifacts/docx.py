"""Word documents with explicit margins, paragraph spacing and targeted updates."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .common import (
    ArtifactError,
    canonical_zip,
    check_report,
    checked_file,
    finite,
    library,
    load_json,
    output_file,
    report,
    request,
)

MEDIA = "application/vnd.openxmlformats-officedocument.wordprocessingml.document"


def _paragraph(document: Any, spec: Any) -> None:
    if not isinstance(spec, dict) or set(spec) - {"text", "style", "space_before_pt", "space_after_pt"}:
        raise ArtifactError("A paragraph needs text and optional style/space_before_pt/space_after_pt")
    if not isinstance(spec.get("text"), str):
        raise ArtifactError("Paragraph text must be a string")
    paragraph = document.add_paragraph(spec["text"], spec.get("style"))
    points = library("docx.shared").Pt
    for key, default in [("space_before_pt", 0), ("space_after_pt", 8)]:
        value = finite(spec.get(key, default), key)
        if value < 0:
            raise ArtifactError(key + " must be nonnegative")
        setattr(paragraph.paragraph_format, key.removesuffix("_pt"), points(value))


def create(spec: Path, out: Path) -> dict[str, Any]:
    """Create a Word document or append content to a digest-bound template."""
    value = load_json(spec)
    request(value, {"title", "paragraphs", "tables", "header", "footer", "margins_in", "template"})
    template = checked_file(spec.parent, value["template"]) if "template" in value else None
    document = library("docx").Document(str(template)) if template else library("docx").Document()
    if "title" in value:
        document.add_heading(value["title"], level=0)
    for paragraph in value.get("paragraphs", []):
        _paragraph(document, paragraph)
    for spec_table in value.get("tables", []):
        if set(spec_table) != {"headers", "rows"} or not spec_table["headers"]:
            raise ArtifactError("A table needs headers and rows")
        table = document.add_table(rows=1, cols=len(spec_table["headers"]))
        for index, text in enumerate(spec_table["headers"]):
            table.rows[0].cells[index].text = str(text)
        for row in spec_table["rows"]:
            if len(row) != len(spec_table["headers"]):
                raise ArtifactError("Table row length differs from headers")
            for cell, text in zip(table.add_row().cells, row):
                cell.text = str(text)
    for section in document.sections:
        margins = value.get("margins_in", {} if template else {"top": 0.75, "bottom": 0.75, "left": 0.8, "right": 0.8})
        if set(margins) - {"top", "bottom", "left", "right"}:
            raise ArtifactError("Unknown margin name")
        for side, amount in margins.items():
            setattr(section, side + "_margin", library("docx.shared").Inches(finite(amount, side, positive=True)))
        for name in ["header", "footer"]:
            if name in value:
                paragraph = getattr(section, name).paragraphs[0]
                paragraph.text = value[name]
                paragraph.paragraph_format.space_before = library("docx.shared").Pt(0)
                paragraph.paragraph_format.space_after = library("docx.shared").Pt(0)
    with output_file(out) as staged:
        document.save(str(staged))
        canonical_zip(staged)
        check(staged)
    return report(out, MEDIA, structural="passed", rendered="unavailable")


def read(path: Path) -> dict[str, Any]:
    """Read document text, tables, paragraph spacing, headers and footers."""
    document = library("docx").Document(str(path))
    return {
        "paragraphs": [
            {
                "text": paragraph.text,
                "style": paragraph.style.name,
                "space_before_pt": paragraph.paragraph_format.space_before.pt
                if paragraph.paragraph_format.space_before is not None
                else None,
                "space_after_pt": paragraph.paragraph_format.space_after.pt
                if paragraph.paragraph_format.space_after is not None
                else None,
            }
            for paragraph in document.paragraphs
        ],
        "tables": [[[cell.text for cell in row.cells] for row in table.rows] for table in document.tables],
        "headers": ["\n".join(p.text for p in section.header.paragraphs) for section in document.sections],
        "footers": ["\n".join(p.text for p in section.footer.paragraphs) for section in document.sections],
    }


def check(path: Path) -> dict[str, Any]:
    """Verify the actual package and page margins, without claiming a rendered layout."""
    document = library("docx").Document(str(path))
    for section in document.sections:
        if section.left_margin + section.right_margin >= section.page_width or (
            section.top_margin + section.bottom_margin >= section.page_height
        ):
            raise ArtifactError("Margins leave no usable page")
    return check_report(paragraphs=len(document.paragraphs), tables=len(document.tables))


def update(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Edit selected paragraphs while retaining other text, styles, tables and media."""
    value = load_json(spec)
    request(value, {"paragraphs", "append_paragraphs"})
    document = library("docx").Document(str(path))
    for index, text in value.get("paragraphs", {}).items():
        if not index.isdigit() or not 0 <= int(index) < len(document.paragraphs) or not isinstance(text, str):
            raise ArtifactError("Paragraph replacement needs an existing zero-based index and string")
        paragraph = document.paragraphs[int(index)]
        if paragraph.runs:
            paragraph.runs[0].text = text
            for run in paragraph.runs[1:]:
                run.text = ""
        else:
            paragraph.text = text
    for paragraph in value.get("append_paragraphs", []):
        _paragraph(document, paragraph)
    with output_file(out) as staged:
        document.save(str(staged))
        check(staged)
    return report(out, MEDIA, structural="passed", rendered="unavailable")
