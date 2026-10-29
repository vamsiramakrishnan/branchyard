"""Editable PowerPoint output, inspection and targeted preservation updates."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .common import ArtifactError, canonical_zip, check_report, library, load_json, output_file, report, request
from .deck import DeckInput

MEDIA = "application/vnd.openxmlformats-officedocument.presentationml.presentation"


def _text(slide: Any, text: str, box: list[float], design: dict[str, Any], size: float, after: float = 0) -> Any:
    util = library("pptx.util")
    shape = slide.shapes.add_textbox(*(util.Inches(n) for n in box))
    shape.text_frame.word_wrap = True
    for index, line in enumerate(text.split("\n")):
        paragraph = shape.text_frame.paragraphs[0] if index == 0 else shape.text_frame.add_paragraph()
        paragraph.text = line
        paragraph.font.name = design["font"]
        paragraph.font.size = util.Pt(size)
        paragraph.font.color.rgb = library("pptx.dml.color").RGBColor.from_string(design["foreground"])
        paragraph.space_before = util.Pt(0)
        paragraph.space_after = util.Pt(after)
        paragraph.line_spacing = 1.1
    return shape


def _chart_data(spec: dict[str, Any]) -> Any:
    data = library("pptx.chart.data").CategoryChartData(number_format=spec["number_format"])
    data.categories = spec["categories"]
    for series in spec["series"]:
        data.add_series(series["name"], series["values"], spec["number_format"])
    return data


def render_deck(deck: DeckInput, out: Path) -> dict[str, Any]:
    """Render validated sections once, keeping charts and their source workbooks editable."""
    pptx = library("pptx")
    util = library("pptx.util")
    presentation = pptx.Presentation(str(deck.template)) if deck.template else pptx.Presentation()
    design = deck.design
    width, height = design["width_in"], design["height_in"]
    if deck.template:
        if len(presentation.slides):
            raise ArtifactError("A deck template must have no slides; use pptx update for an existing presentation")
        if (
            abs(presentation.slide_width / 914400 - width) > 0.01
            or abs(presentation.slide_height / 914400 - height) > 0.01
        ):
            raise ArtifactError("Template dimensions differ from the deck design")
    presentation.slide_width, presentation.slide_height = util.Inches(width), util.Inches(height)
    layout = next(
        (
            item
            for item in presentation.slide_layouts
            if all(int(placeholder.placeholder_format.type) in {13, 15, 16} for placeholder in item.placeholders)
        ),
        None,
    )
    if layout is None:
        raise ArtifactError("Template has no blank layout")
    chart_types = library("pptx.enum.chart").XL_CHART_TYPE
    types = {
        "column": chart_types.COLUMN_CLUSTERED,
        "bar": chart_types.BAR_CLUSTERED,
        "line": chart_types.LINE,
        "pie": chart_types.PIE,
    }
    for index, item in enumerate(deck.slides):
        spec = item.spec
        slide = presentation.slides.add_slide(layout)
        slide.background.fill.solid()
        slide.background.fill.fore_color.rgb = library("pptx.dml.color").RGBColor.from_string(design["background"])
        margin = design["margin_in"]
        _text(slide, spec["title"], [margin, margin, width - 2 * margin, 0.75], design, design["title_font_size_pt"])
        for element in spec["elements"]:
            box = element["box"]
            position = [util.Inches(n) for n in box]
            if element["kind"] == "text":
                _text(
                    slide,
                    element["text"],
                    box,
                    design,
                    element.get("font_size_pt", 22),
                    element.get("space_after_pt", 6),
                )
            elif element["kind"] == "image":
                with library("PIL.Image").open(item.assets[element["asset_id"]]) as image:
                    scale = min(box[2] / image.width, box[3] / image.height)
                    image_width, image_height = image.width * scale, image.height * scale
                slide.shapes.add_picture(
                    str(item.assets[element["asset_id"]]),
                    util.Inches(box[0] + (box[2] - image_width) / 2),
                    util.Inches(box[1] + (box[3] - image_height) / 2),
                    util.Inches(image_width),
                    util.Inches(image_height),
                )
            elif element["kind"] == "table":
                rows = [element["headers"], *element["rows"]]
                table = slide.shapes.add_table(len(rows), len(rows[0]), *position).table
                for row_index, row in enumerate(rows):
                    for column, value in enumerate(row):
                        cell = table.cell(row_index, column)
                        cell.text = "" if value is None else str(value)
                        paragraph = cell.text_frame.paragraphs[0]
                        paragraph.font.name = design["font"]
                        paragraph.font.size = util.Pt(element.get("font_size_pt", 16))
                        paragraph.font.color.rgb = library("pptx.dml.color").RGBColor.from_string(design["foreground"])
                        paragraph.space_before, paragraph.space_after = util.Pt(0), util.Pt(0)
                        if row_index == 0:
                            cell.fill.solid()
                            cell.fill.fore_color.rgb = library("pptx.dml.color").RGBColor.from_string(design["accent"])
                            paragraph.font.color.rgb = library("pptx.dml.color").RGBColor(255, 255, 255)
                            paragraph.font.bold = True
            elif element["kind"] == "chart":
                chart = slide.shapes.add_chart(types[element["chart_type"]], *position, _chart_data(element)).chart
                chart.has_title = True
                chart.chart_title.text_frame.text = element["title"] + " (" + element["unit"] + ")"
                for paragraph in chart.chart_title.text_frame.paragraphs:
                    paragraph.font.name, paragraph.font.size = design["font"], util.Pt(16)
                chart.has_legend = len(element["series"]) > 1 or element["chart_type"] == "pie"
                if chart.has_legend:
                    chart.legend.font.name, chart.legend.font.size = design["font"], util.Pt(11)
                for series_index, series in enumerate(chart.series):
                    color = library("pptx.dml.color").RGBColor.from_string(design["accent"])
                    if element["chart_type"] == "line":
                        series.format.line.color.rgb = color
                        series.format.line.color.brightness = min(series_index * 0.2, 0.6)
                    elif element["chart_type"] != "pie":
                        series.format.fill.solid()
                        series.format.fill.fore_color.rgb = color
                        series.format.fill.fore_color.brightness = min(series_index * 0.2, 0.6)
                if element["chart_type"] != "pie":
                    if element["chart_type"] in {"bar", "column"}:
                        values = [value for series in element["series"] for value in series["values"]]
                        chart.value_axis.minimum_scale = min(0, min(values))
                        if min(values) < 0 and max(values) <= 0:
                            chart.value_axis.maximum_scale = 0
                    chart.value_axis.tick_labels.number_format = element["number_format"]
                    chart.value_axis.has_title = True
                    chart.value_axis.axis_title.text_frame.text = element["unit"]
                    for axis in [chart.value_axis, chart.category_axis]:
                        axis.tick_labels.font.name, axis.tick_labels.font.size = design["font"], util.Pt(12)
                    for paragraph in chart.value_axis.axis_title.text_frame.paragraphs:
                        paragraph.font.name, paragraph.font.size = design["font"], util.Pt(12)
                else:
                    chart.plots[0].has_data_labels = True
                    labels = chart.plots[0].data_labels
                    labels.show_category_name, labels.show_value = True, True
                    labels.number_format = element["number_format"]
        footer = " | ".join(cite["label"] for cite in spec["citations"])
        _text(
            slide,
            str(index + 1) + ("  " + footer if footer else ""),
            [margin, height - margin - 0.45, width - 2 * margin, 0.45],
            design,
            design["footer_font_size_pt"],
        )
        citations = []
        for citation in spec["citations"]:
            if "url" in citation:
                citations.append(citation["label"] + ": " + citation["url"])
            else:
                source = citation["source"]
                citations.append(
                    citation["label"]
                    + ": "
                    + source["path"]
                    + "; locator: "
                    + source["locator"]
                    + "; SHA-256: "
                    + source["sha256"]
                )
        slide.notes_slide.notes_text_frame.text = item.key + "\n" + "\n".join(citations)
    with output_file(out) as staged:
        presentation.save(str(staged))
        canonical_zip(staged)
        check(staged)
    return report(
        out,
        MEDIA,
        slides=len(deck.slides),
        slide_order=[slide.key for slide in deck.slides],
        structural="passed",
        rendered="unavailable",
    )


def create(spec: Path, out: Path) -> dict[str, Any]:
    """Create a deck from the versioned deck manifest."""
    from .deck import assemble

    return assemble(spec, out)


def read(path: Path) -> dict[str, Any]:
    """Inspect actual slide shapes, editable chart values and speaker notes."""
    presentation = library("pptx").Presentation(str(path))
    slides = []
    for slide in presentation.slides:
        shapes = []
        for shape in slide.shapes:
            entry: dict[str, Any] = {
                "name": shape.name,
                "box": [n / 914400 for n in (shape.left, shape.top, shape.width, shape.height)],
            }
            if shape.has_text_frame:
                entry["text"] = shape.text
            if shape.has_table:
                entry["table"] = [[cell.text for cell in row.cells] for row in shape.table.rows]
            if shape.has_chart:
                entry["chart"] = {
                    "series": [{"name": series.name, "values": list(series.values)} for series in shape.chart.series]
                }
            shapes.append(entry)
        slides.append(
            {"shapes": shapes, "notes": slide.notes_slide.notes_text_frame.text if slide.has_notes_slide else ""}
        )
    return {
        "slides": slides,
        "width_in": presentation.slide_width / 914400,
        "height_in": presentation.slide_height / 914400,
    }


def check(path: Path) -> dict[str, Any]:
    """Reopen the presentation and reject out-of-slide shapes."""
    value = read(path)
    for index, slide in enumerate(value["slides"]):
        for shape in slide["shapes"]:
            x, y, width, height = shape["box"]
            if (
                min(x, y) < 0
                or min(width, height) <= 0
                or x + width > value["width_in"] + 0.001
                or (y + height > value["height_in"] + 0.001)
            ):
                raise ArtifactError("Shape exceeds slide bounds on slide " + str(index + 1))
    return check_report(
        slides=len(value["slides"]),
        editable_charts=sum("chart" in shape for slide in value["slides"] for shape in slide["shapes"]),
    )


def update(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Replace selected shape text, preserving other slides, charts and embedded media."""
    edits = load_json(spec)
    request(edits, {"replacements"}, {"replacements"})
    presentation = library("pptx").Presentation(str(path))
    for edit in edits["replacements"]:
        if set(edit) != {"slide", "shape", "text"} or not isinstance(edit["text"], str):
            raise ArtifactError("Each replacement needs slide, shape and text")
        if not isinstance(edit["slide"], int) or not 0 <= edit["slide"] < len(presentation.slides):
            raise ArtifactError("Replacement slide index is out of range")
        slide = presentation.slides[edit["slide"]]
        if not isinstance(edit["shape"], int) or not 0 <= edit["shape"] < len(slide.shapes):
            raise ArtifactError("Replacement shape index is out of range")
        shape = slide.shapes[edit["shape"]]
        if not shape.has_text_frame:
            raise ArtifactError("Replacement shape has no text frame")
        paragraph = shape.text_frame.paragraphs[0]
        if paragraph.runs:
            paragraph.runs[0].text = edit["text"]
            for run in paragraph.runs[1:]:
                run.text = ""
        else:
            paragraph.text = edit["text"]
        for remaining in list(shape.text_frame.paragraphs)[1:]:
            remaining.text = ""
    with output_file(out) as staged:
        presentation.save(str(staged))
        check(staged)
    return report(out, MEDIA, structural="passed", rendered="unavailable")
