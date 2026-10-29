"""PDF text generation, extraction and preservation overlays."""

from __future__ import annotations

import io
from functools import partial
from pathlib import Path
from typing import Any
from xml.sax.saxutils import escape

from .common import ArtifactError, check_report, finite, library, load_json, output_file, report, request

MEDIA = "application/pdf"


def create(spec: Path, out: Path) -> dict[str, Any]:
    """Create a paginated PDF with literal text and fixed metadata."""
    value = load_json(spec)
    request(value, {"title", "pages", "page_size_pt", "margins_pt"}, {"pages"})
    if not isinstance(value["pages"], list) or not value["pages"]:
        raise ArtifactError("PDF needs at least one page")
    size = value.get("page_size_pt", [612, 792])
    margins = value.get("margins_pt", [54, 54, 54, 54])
    if (
        len(size) != 2
        or len(margins) != 4
        or any(finite(n, "page measure", positive=True) <= 0 for n in [*size, *margins])
    ):
        raise ArtifactError("PDF page_size_pt needs two numbers and margins_pt four")
    if margins[0] + margins[2] >= size[0] or margins[1] + margins[3] >= size[1]:
        raise ArtifactError("PDF margins leave no usable page")
    flow = library("reportlab.platypus")
    styles = library("reportlab.lib.styles").getSampleStyleSheet()
    story = []
    for index, page in enumerate(value["pages"]):
        if set(page) - {"title", "paragraphs"}:
            raise ArtifactError("PDF page supports title and paragraphs")
        if index:
            story.append(flow.PageBreak())
        if page.get("title"):
            story.append(flow.Paragraph(escape(page["title"]), styles["Title"]))
        for text in page.get("paragraphs", []):
            if not isinstance(text, str):
                raise ArtifactError("PDF paragraphs must be strings")
            story.append(flow.Paragraph(escape(text).replace("\n", "<br/>"), styles["BodyText"]))
            story.append(flow.Spacer(1, 8))
        if not page.get("title") and not page.get("paragraphs"):
            story.append(flow.Spacer(1, 1))
    with output_file(out) as staged:
        document = flow.SimpleDocTemplate(
            str(staged),
            pagesize=size,
            leftMargin=margins[0],
            bottomMargin=margins[1],
            rightMargin=margins[2],
            topMargin=margins[3],
            title=value.get("title", ""),
        )
        document.build(story, canvasmaker=partial(library("reportlab.pdfgen.canvas").Canvas, invariant=1))
        check(staged)
    return report(out, MEDIA, structural="passed", rendered="unavailable")


def read(path: Path) -> dict[str, Any]:
    """Extract text and actual page dimensions with pypdf."""
    reader = library("pypdf").PdfReader(str(path), strict=True)
    if reader.is_encrypted:
        raise ArtifactError("Encrypted PDF needs an explicit decryption workflow")
    return {
        "pages": [
            {
                "text": page.extract_text(),
                "width_pt": float(page.mediabox.width),
                "height_pt": float(page.mediabox.height),
            }
            for page in reader.pages
        ],
        "metadata": {str(k): str(v) for k, v in (reader.metadata or {}).items()},
    }


def check(path: Path) -> dict[str, Any]:
    """Parse every page and report structural validity separately from raster inspection."""
    value = read(path)
    if not value["pages"] or any(page["width_pt"] <= 0 or page["height_pt"] <= 0 for page in value["pages"]):
        raise ArtifactError("PDF has no usable pages")
    return check_report(pages=len(value["pages"]))


def update(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Overlay literal text and update metadata, retaining all source pages."""
    value = load_json(spec)
    request(value, {"overlays", "metadata"})
    reader = library("pypdf").PdfReader(str(path), strict=True)
    if reader.is_encrypted:
        raise ArtifactError("Encrypted PDF needs explicit decryption")
    writer = library("pypdf").PdfWriter(clone_from=reader)
    for overlay in value.get("overlays", []):
        if set(overlay) != {"page", "text", "x_pt", "y_pt", "font_size_pt"}:
            raise ArtifactError("An overlay needs page, text, x_pt, y_pt and font_size_pt")
        index = overlay["page"]
        if not isinstance(index, int) or not 0 <= index < len(writer.pages) or not isinstance(overlay["text"], str):
            raise ArtifactError("Overlay needs an existing zero-based page and string text")
        page = writer.pages[index]
        x, y, points = (
            finite(overlay[key], key, positive=key == "font_size_pt") for key in ["x_pt", "y_pt", "font_size_pt"]
        )
        width = library("reportlab.pdfbase.pdfmetrics").stringWidth(overlay["text"], "Helvetica", points)
        if min(x, y) < 0 or x + width > float(page.mediabox.width) or y + points > float(page.mediabox.height):
            raise ArtifactError("PDF overlay extends past page bounds")
        buffer = io.BytesIO()
        canvas = library("reportlab.pdfgen.canvas").Canvas(
            buffer, pagesize=(page.mediabox.width, page.mediabox.height), invariant=1
        )
        canvas.setFont("Helvetica", points)
        canvas.drawString(x, y, overlay["text"])
        canvas.save()
        page.merge_page(library("pypdf").PdfReader(buffer).pages[0])
    metadata = value.get("metadata", {})
    if set(metadata) - {"title", "author", "subject", "keywords"} or any(
        not isinstance(v, str) for v in metadata.values()
    ):
        raise ArtifactError("PDF metadata supports string title, author, subject and keywords")
    if metadata:
        writer.add_metadata({"/" + key.title(): text for key, text in metadata.items()})
    with output_file(out) as staged:
        with staged.open("wb") as target:
            writer.write(target)
        check(staged)
    return report(out, MEDIA, structural="passed", rendered="unavailable")
