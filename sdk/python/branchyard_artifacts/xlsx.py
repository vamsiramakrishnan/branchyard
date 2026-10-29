"""Excel cells, formulas, tables and native editable charts."""

from __future__ import annotations

import datetime
import warnings
from pathlib import Path
from typing import Any

from .common import ArtifactError, canonical_zip, check_report, finite, library, load_json, output_file, report, request

MEDIA = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"


def _load(path: Path, data_only: bool = False) -> Any:
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always", UserWarning)
        workbook = library("openpyxl").load_workbook(str(path), data_only=data_only)
    if caught:
        raise ArtifactError(
            "Workbook needs unsupported preservation features: " + "; ".join(str(w.message) for w in caught)
        )
    return workbook


def _cells(sheet: Any, spec: dict[str, Any]) -> None:
    if set(spec) - {"name", "cells", "number_formats", "freeze_panes", "tables"}:
        raise ArtifactError("Unknown sheet field")
    coordinates = library("openpyxl.utils.cell")
    for address, value in spec.get("cells", {}).items():
        try:
            row, column = coordinates.coordinate_to_tuple(address)
        except ValueError as error:
            raise ArtifactError("Invalid cell address: " + address) from error
        if not 1 <= row <= 1048576 or not 1 <= column <= 16384:
            raise ArtifactError("Cell address exceeds Excel limits: " + address)
        if not isinstance(value, (str, int, float, bool, type(None))):
            raise ArtifactError("Cell values must be strings, numbers, booleans or null")
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            finite(value, address)
        sheet[address] = value
    for address, number_format in spec.get("number_formats", {}).items():
        if not isinstance(number_format, str) or not number_format:
            raise ArtifactError("Number format must be a nonempty string")
        sheet[address].number_format = number_format
    if "freeze_panes" in spec:
        sheet.freeze_panes = spec["freeze_panes"]
    for table in spec.get("tables", []):
        if set(table) != {"name", "range"}:
            raise ArtifactError("A table needs name and range")
        sheet.add_table(library("openpyxl.worksheet.table").Table(displayName=table["name"], ref=table["range"]))


def _charts(workbook: Any, specs: list[dict[str, Any]]) -> None:
    charts = library("openpyxl.chart")
    for spec in specs:
        if set(spec) != {"type", "sheet", "title", "unit", "number_format", "categories", "values", "anchor"}:
            raise ArtifactError("A chart needs type, sheet, title, unit, number_format, categories, values and anchor")
        if spec["type"] not in {"column", "line"} or not all(spec[key] for key in ["title", "unit", "number_format"]):
            raise ArtifactError("Charts need column/line type and explicit title, units and number format")
        sheet = workbook[spec["sheet"]]
        chart = charts.BarChart() if spec["type"] == "column" else charts.LineChart()
        chart.title, chart.y_axis.title = spec["title"], spec["unit"]
        chart.y_axis.numFmt = spec["number_format"]
        chart.add_data(charts.Reference(sheet, range_string=sheet.title + "!" + spec["values"]), titles_from_data=True)
        chart.set_categories(charts.Reference(sheet, range_string=sheet.title + "!" + spec["categories"]))
        sheet.add_chart(chart, spec["anchor"])


def create(spec: Path, out: Path) -> dict[str, Any]:
    """Create an editable workbook; formulas are preserved and not evaluated."""
    value = load_json(spec)
    request(value, {"sheets", "charts"}, {"sheets"})
    if not value["sheets"]:
        raise ArtifactError("At least one sheet is required")
    workbook = library("openpyxl").Workbook()
    workbook.remove(workbook.active)
    names: set[str] = set()
    for sheet in value["sheets"]:
        name = sheet["name"]
        if name.casefold() in names:
            raise ArtifactError("Duplicate sheet name: " + name)
        names.add(name.casefold())
        _cells(workbook.create_sheet(name), sheet)
    _charts(workbook, value.get("charts", []))
    with output_file(out) as staged:
        workbook.save(str(staged))
        canonical_zip(staged)
        check(staged)
    return report(out, MEDIA, structural="passed", calculated="unavailable", rendered="unavailable")


def read(path: Path) -> dict[str, Any]:
    """Read formulas and cached values separately, without inventing cached results."""
    workbook, cached = _load(path), _load(path, data_only=True)
    sheets = []
    for sheet in workbook:
        cells = {}
        for row in sheet:
            for cell in row:
                if cell.value is not None:
                    value = (
                        cell.value.isoformat() if isinstance(cell.value, (datetime.date, datetime.time)) else cell.value
                    )
                    cells[cell.coordinate] = {
                        "value": value,
                        "number_format": cell.number_format,
                        "cached_value": cached[sheet.title][cell.coordinate].value if cell.data_type == "f" else None,
                    }
        sheets.append(
            {
                "name": sheet.title,
                "cells": cells,
                "tables": list(sheet.tables),
                "charts": len(sheet._charts),
                "pivots": len(sheet._pivots),
            }
        )
    return {"sheets": sheets, "calculated": "unavailable"}


def check(path: Path) -> dict[str, Any]:
    """Reopen cells/charts/pivots and report formula evaluation separately."""
    workbook = _load(path)
    formulas = sum(cell.data_type == "f" for sheet in workbook for row in sheet for cell in row)
    return check_report(
        sheets=len(workbook.sheetnames),
        formulas=formulas,
        calculated="unavailable",
        editable_charts=sum(len(sheet._charts) for sheet in workbook),
        native_pivots=sum(len(sheet._pivots) for sheet in workbook),
    )


def update(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Update named cells and formats while retaining sheets, tables, formulas and supported charts."""
    value = load_json(spec)
    request(value, {"sheets", "charts"}, {"sheets"})
    workbook = _load(path)
    for sheet in value["sheets"]:
        if sheet["name"] not in workbook.sheetnames:
            raise ArtifactError("Update sheet does not exist: " + sheet["name"])
        _cells(workbook[sheet["name"]], sheet)
    _charts(workbook, value.get("charts", []))
    with output_file(out) as staged:
        workbook.save(str(staged))
        check(staged)
    return report(out, MEDIA, structural="passed", calculated="unavailable", rendered="unavailable")


def pivot(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Create a native Calc/Excel pivot only through an available UNO backend."""
    from .native import create_pivot

    return create_pivot(path, spec, out)
