"""Validate independent section files and assemble them through one renderer."""

from __future__ import annotations

import json
import math
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .common import ArtifactError, Unavailable, checked_file, library, load_json


@dataclass(frozen=True)
class SlideInput:
    """One validated slide plus section-local immutable image paths."""

    key: str
    spec: dict[str, Any]
    assets: dict[str, Path]


@dataclass(frozen=True)
class DeckInput:
    """A complete ordered deck, ready for a single writer."""

    id: str
    design: dict[str, Any]
    slides: list[SlideInput]
    template: Path | None


def validate_schema(value: dict[str, Any], name: str) -> None:
    """Apply the packaged, versioned schema and name the offending field."""
    schema = json.loads((Path(__file__).parent / "schemas" / (name + "-v1.json")).read_text())
    validator = library("jsonschema").Draft202012Validator(schema)
    error = next(validator.iter_errors(value), None)
    if error is not None:
        path = ".".join(str(part) for part in error.absolute_path)
        raise ArtifactError((path or name) + ": " + error.message)


def font_path(name: str) -> Path:
    """Resolve the requested font without silently accepting a font substitute."""
    try:
        result = subprocess.run(
            ["fc-match", "-f", "%{family}\n%{file}", name], capture_output=True, text=True, check=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise Unavailable("Font measurement needs fontconfig and the requested font: " + name) from error
    lines = result.stdout.splitlines()
    if len(lines) != 2 or name.casefold() not in lines[0].casefold().split(","):
        raise Unavailable("Requested font is not installed: " + name)
    return Path(lines[1])


def text_fits(text: str, width: float, height: float, points: float, font: Path, after: float = 0) -> bool:
    """Conservatively measure wrapped text, including paragraph spacing, in inches."""
    image_font = library("PIL.ImageFont").truetype(str(font), size=round(points * 4))
    available = max(0, width * 72 - 14) * 4
    lines = 0
    for paragraph in text.split("\n"):
        current = ""
        lines += 1
        for word in paragraph.split():
            if image_font.getlength(word) > available:
                return False
            combined = (current + " " + word).strip()
            if image_font.getlength(combined) > available:
                lines += 1
                current = word
            else:
                current = combined
    needed = lines * points * 1.25 + len(text.split("\n")) * after + 8
    return needed <= height * 72


def _section(path: Path, design: dict[str, Any] | None = None) -> tuple[dict[str, Any], list[SlideInput]]:
    value = load_json(path)
    validate_schema(value, "section")
    assets: dict[str, Path] = {}
    for asset in value["assets"]:
        if asset["id"] in assets:
            raise ArtifactError("Duplicate asset ID: " + asset["id"])
        assets[asset["id"]] = checked_file(path.parent, asset)
    slides = []
    seen: set[str] = set()
    font = font_path(design["font"]) if design else None
    if design and value["design_id"] != design["id"]:
        raise ArtifactError("Section design_id differs from deck: " + value["id"])
    for slide in value["slides"]:
        key = value["id"] + "/" + slide["id"]
        if key in seen:
            raise ArtifactError("Duplicate slide ID: " + key)
        seen.add(key)
        for citation in slide["citations"]:
            if "source" in citation:
                checked_file(path.parent, citation["source"])
        if design and font:
            margin = design["margin_in"]
            if not text_fits(slide["title"], design["width_in"] - 2 * margin, 0.75, design["title_font_size_pt"], font):
                raise ArtifactError("Title does not fit: " + key)
            footer = " | ".join(cite["label"] for cite in slide["citations"])
            if footer and not text_fits(
                footer, design["width_in"] - 2 * margin, 0.45, design["footer_font_size_pt"], font
            ):
                raise ArtifactError("Citation footer does not fit: " + key)
        for element in slide["elements"]:
            x, y, width, height = element["box"]
            if not all(math.isfinite(n) for n in element["box"]) or min(x, y) < 0 or min(width, height) <= 0:
                raise ArtifactError("Invalid shape geometry: " + key)
            if design and (x + width > design["width_in"] or y + height > design["height_in"]):
                raise ArtifactError("Shape extends past the slide: " + key)
            if design and (
                x < design["margin_in"]
                or y < design["margin_in"] + 0.85
                or x + width > design["width_in"] - design["margin_in"]
                or y + height > design["height_in"] - design["margin_in"] - 0.5
            ):
                raise ArtifactError("Content overlaps reserved title, footer or margins: " + key)
            kind = element["kind"]
            if kind == "image" and element["asset_id"] not in assets:
                raise ArtifactError("Unknown image asset: " + element["asset_id"])
            if kind == "image":
                with library("PIL.Image").open(assets[element["asset_id"]]) as image:
                    image.verify()
            if kind == "chart":
                if not slide["citations"]:
                    raise ArtifactError("Charts require a source citation: " + key)
                for series in element["series"]:
                    if len(series["values"]) != len(element["categories"]) or not all(
                        math.isfinite(n) for n in series["values"]
                    ):
                        raise ArtifactError("Chart categories and finite values must agree: " + key)
                if element["chart_type"] == "pie" and len(element["series"]) != 1:
                    raise ArtifactError("A pie chart requires exactly one series: " + key)
                if element["chart_type"] == "pie" and (
                    min(element["series"][0]["values"]) < 0 or sum(element["series"][0]["values"]) <= 0
                ):
                    raise ArtifactError("Pie values must be nonnegative with a positive total: " + key)
                if design and font:
                    if min(width, height) < 2.5 or not text_fits(
                        element["title"] + " (" + element["unit"] + ")", width, 0.7, 16, font
                    ):
                        raise ArtifactError("Chart frame or title is too small: " + key)
                    label_width = width / len(element["categories"]) if element["chart_type"] != "bar" else width / 3
                    if any(not text_fits(label, label_width, 0.7, 12, font) for label in element["categories"]):
                        raise ArtifactError("Chart category labels do not fit: " + key)
            if kind == "table":
                if any(len(row) != len(element["headers"]) for row in element["rows"]):
                    raise ArtifactError("Table row width differs from its headers: " + key)
                if design and font:
                    cells = [element["headers"], *element["rows"]]
                    for row in cells:
                        if any(
                            not text_fits(
                                str(cell), width / len(row), height / len(cells), element.get("font_size_pt", 16), font
                            )
                            for cell in row
                        ):
                            raise ArtifactError("Table text does not fit its cell: " + key)
            if (
                kind == "text"
                and design
                and font
                and not text_fits(
                    element["text"],
                    width,
                    height,
                    element.get("font_size_pt", 22),
                    font,
                    element.get("space_after_pt", 6),
                )
            ):
                raise ArtifactError("Body text does not fit: " + key)
        slides.append(SlideInput(key, slide, assets))
    return value, slides


def validate_section(path: Path, deck: Path | None = None) -> dict[str, Any]:
    """Validate a worker's independently owned section and asset digests."""
    design = None
    if deck:
        spec = load_json(deck)
        validate_schema(spec, "deck")
        design = spec["design"]
    value, slides = _section(Path(path), design)
    return {
        "status": "passed",
        "section_id": value["id"],
        "slides": [slide.key for slide in slides],
        "geometry": "passed" if design else "unavailable",
        "rendered": "unavailable",
    }


def load_deck(path: Path) -> DeckInput:
    """Require the exact slide inventory, compatible design and verified inputs."""
    value = load_json(path)
    validate_schema(value, "deck")
    design = value["design"]
    if design["margin_in"] * 2 >= min(design["width_in"], design["height_in"]):
        raise ArtifactError("Deck margins leave no usable page")
    slides: dict[str, SlideInput] = {}
    section_ids: set[str] = set()
    for ref in value["sections"]:
        if ref["id"] in section_ids:
            raise ArtifactError("Duplicate section ID: " + ref["id"])
        section_ids.add(ref["id"])
        section, loaded = _section(checked_file(path.parent, ref), design)
        if section["id"] != ref["id"]:
            raise ArtifactError("Section reference ID differs from file: " + ref["id"])
        slides.update({slide.key: slide for slide in loaded})
    if set(value["slide_order"]) != set(slides):
        missing = set(slides) - set(value["slide_order"])
        extra = set(value["slide_order"]) - set(slides)
        raise ArtifactError(
            "Slide inventory differs; omitted " + str(sorted(missing)) + "; unknown " + str(sorted(extra))
        )
    template = checked_file(path.parent, value["template"]) if "template" in value else None
    return DeckInput(value["id"], design, [slides[key] for key in value["slide_order"]], template)


def assemble(path: Path, out: Path) -> dict[str, Any]:
    """Validate all sections before atomically writing the editable presentation."""
    from .pptx import render_deck

    return render_deck(load_deck(Path(path)), Path(out))
