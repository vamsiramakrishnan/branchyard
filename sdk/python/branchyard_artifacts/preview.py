"""Bounded local previews for tables, documents, presentations, PDFs, images and text."""

from __future__ import annotations

import csv
import datetime
import io
import json
import os
import stat
import zipfile
from decimal import Decimal
from pathlib import Path
from typing import Any

from .common import ArtifactError, finite, library, output_file
from .native import (
    MAX_RENDER_DIMENSION,
    MAX_RENDER_IMAGE_BYTES,
    MAX_RENDER_PAGES,
    MAX_RENDER_PIXELS,
    RENDER_TIMEOUT_SECONDS,
    render_page,
)

MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_OUTPUT_IMAGE_BYTES = MAX_RENDER_IMAGE_BYTES
MAX_ARCHIVE_ENTRIES = 10_000
MAX_ARCHIVE_EXPANDED_BYTES = 128 * 1024 * 1024
MAX_PAGES = MAX_RENDER_PAGES
MAX_IMAGE_DIMENSION = 4096
MAX_PREVIEW_DIMENSION = MAX_RENDER_DIMENSION
MAX_IMAGE_PIXELS = MAX_RENDER_PIXELS
MAX_COLUMNS = 100
MAX_LIMIT = 500
MAX_SCAN_ROWS = 100_000
MAX_CELL_CHARS = 1_000
MAX_TEXT_BYTES = 256 * 1024
MAX_TEXT_CHARS = 65_536
PROCESS_TIMEOUT_SECONDS = RENDER_TIMEOUT_SECONDS

MACRO_SUFFIXES = {
    ".docm",
    ".dotm",
    ".xlsm",
    ".xltm",
    ".xlam",
    ".xlsb",
    ".pptm",
    ".potm",
    ".ppam",
    ".ppsm",
    ".sldm",
}
ACTIVE_MARKUP_SUFFIXES = {".svg", ".svgz", ".html", ".htm", ".xhtml", ".xml", ".mhtml"}
IMAGE_FORMATS = {
    ".png": "png",
    ".jpg": "jpeg",
    ".jpeg": "jpeg",
    ".webp": "webp",
    ".gif": "gif",
    ".bmp": "bmp",
    ".tif": "tiff",
    ".tiff": "tiff",
}
TEXT_FORMATS = {
    ".txt": "txt",
    ".md": "markdown",
    ".markdown": "markdown",
    ".json": "json",
}
REQUIRED_OFFICE_PARTS = {
    "docx": "word/document.xml",
    "xlsx": "xl/workbook.xml",
    "pptx": "ppt/presentation.xml",
}


def _warn_once(warnings: list[str], message: str) -> None:
    if message not in warnings:
        warnings.append(message)


def _check_no_symlinks(path: Path, label: str) -> None:
    """Refuse symbolic links on the target path or any ancestor component."""
    raw = Path(path)
    if raw.is_symlink():
        raise ArtifactError(label + " must not be a symbolic link")
    absolute = Path(os.path.abspath(raw))
    for candidate in (absolute, *absolute.parents):
        if candidate.is_symlink():
            raise ArtifactError(label + " must not traverse a symbolic link")


def _validate_paths(source: Path, out: Path) -> tuple[Path, Path]:
    """Validate input and output paths before reading or creating files."""
    source, out = Path(source), Path(out)
    _check_no_symlinks(source, "Preview input")
    _check_no_symlinks(out, "Preview output directory")
    source_abs = Path(os.path.abspath(source))
    out_abs = Path(os.path.abspath(out))
    if out_abs == source_abs or (out.exists() and source.exists() and os.path.samefile(out, source)):
        raise ArtifactError("Preview output directory must not overwrite the source file")
    if out.exists() and not out.is_dir():
        raise ArtifactError("Preview output destination must be a directory")
    return source, out


def _read_regular(path: Path, limit: int = MAX_INPUT_BYTES) -> bytes:
    """Read a bounded regular file without following a final symlink."""
    _check_no_symlinks(path, "Preview input")
    flags = os.O_RDONLY | os.O_NONBLOCK | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ArtifactError("Preview input must be an existing regular file") from error
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise ArtifactError("Preview input must be a regular file")
        if metadata.st_size == 0:
            raise ArtifactError("Preview input is empty")
        if metadata.st_size > limit:
            raise ArtifactError("Preview input exceeds the byte limit")
        payload = stream.read(limit + 1)
    if not payload:
        raise ArtifactError("Preview input is empty")
    if len(payload) > limit:
        raise ArtifactError("Preview input exceeds the byte limit")
    return payload


def _check_office_archive(payload: bytes, fmt: str) -> None:
    """Validate Office ZIP entry count, expanded size, structure and absence of macros."""
    try:
        archive_context = zipfile.ZipFile(io.BytesIO(payload))
    except (zipfile.BadZipFile, OSError, ValueError, EOFError) as error:
        raise ArtifactError("Malformed Office archive") from error
    with archive_context as archive:
        entries = archive.infolist()
        if not entries or len(entries) > MAX_ARCHIVE_ENTRIES:
            raise ArtifactError("Office archive entry count exceeds preview bounds")
        expanded = 0
        names: set[str] = set()
        for info in entries:
            name = info.filename
            parts = Path(name).parts
            if not name or "\x00" in name or name.startswith(("/", "\\")) or ".." in parts:
                raise ArtifactError("Office archive contains an unsafe member path")
            if stat.S_ISLNK(info.external_attr >> 16):
                raise ArtifactError("Office archive contains a symbolic link member")
            if info.flag_bits & 0x1:
                raise ArtifactError("Encrypted Office archive is not supported")
            if info.file_size < 0 or info.file_size > MAX_ARCHIVE_EXPANDED_BYTES:
                raise ArtifactError("Office archive member exceeds expanded byte limit")
            expanded += info.file_size
            if expanded > MAX_ARCHIVE_EXPANDED_BYTES:
                raise ArtifactError("Expanded Office archive exceeds byte limit")
            lowered = name.lower()
            if (
                lowered.endswith(("vbaproject.bin", "vbadata.xml"))
                or "/macros/" in lowered
                or "macrosheets/" in lowered
            ):
                raise ArtifactError("Macro-enabled Office archive is refused")
            names.add(name)
        required_part = REQUIRED_OFFICE_PARTS.get(fmt)
        if "[Content_Types].xml" not in names or (required_part and required_part not in names):
            raise ArtifactError("Malformed " + fmt.upper() + " package: missing required document parts")
        try:
            content_types = archive.read("[Content_Types].xml").decode("utf-8", errors="replace").lower()
        except Exception as error:
            raise ArtifactError("Malformed Office [Content_Types].xml part") from error
        if "vbaproject" in content_types or "macroenabled" in content_types:
            raise ArtifactError("Macro-enabled Office content type is refused")


def _truncate_text(text: str, warnings: list[str]) -> str:
    if len(text) > MAX_CELL_CHARS:
        _warn_once(warnings, "Truncated cell text to " + str(MAX_CELL_CHARS) + " characters")
        return text[:MAX_CELL_CHARS]
    return text


def _preview_csv(path: Path, offset: int, limit: int) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if b"\x00" in payload:
        raise ArtifactError("CSV input contains NUL bytes")
    try:
        decoded = payload.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ArtifactError("CSV input must be valid UTF-8") from error
    warnings: list[str] = []
    reader = csv.reader(io.StringIO(decoded, newline=""), strict=True)
    try:
        raw_header = next(reader, None)
    except csv.Error as error:
        raise ArtifactError("Malformed CSV header: " + str(error)) from error
    if raw_header is None or not raw_header or all(cell == "" for cell in raw_header):
        raise ArtifactError("CSV input has no header row")
    if len(raw_header) > MAX_COLUMNS:
        raw_header = raw_header[:MAX_COLUMNS]
        _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
    columns = [
        _truncate_text(label, warnings) if label != "" else "column_" + str(index + 1)
        for index, label in enumerate(raw_header)
    ]
    rows: list[list[Any]] = []
    next_offset: int | None = None
    row_index = 0
    try:
        for raw_row in reader:
            if row_index >= MAX_SCAN_ROWS:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
                break
            if len(raw_row) > MAX_COLUMNS:
                raw_row = raw_row[:MAX_COLUMNS]
                _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
            if len(raw_row) > len(columns):
                for extra in range(len(columns), len(raw_row)):
                    columns.append("column_" + str(extra + 1))
            if row_index < offset:
                row_index += 1
                continue
            if len(rows) < limit:
                padded = [*raw_row, *([""] * max(0, len(columns) - len(raw_row)))]
                rows.append([_truncate_text(cell, warnings) for cell in padded[: len(columns)]])
                row_index += 1
                continue
            if offset + len(rows) < MAX_SCAN_ROWS:
                next_offset = offset + len(rows)
            else:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
            break
    except csv.Error as error:
        raise ArtifactError("Malformed CSV record: " + str(error)) from error
    for row in rows:
        if len(row) < len(columns):
            row.extend([""] * (len(columns) - len(row)))
    return {
        "status": "passed",
        "kind": "table",
        "format": "csv",
        "columns": columns,
        "rows": rows,
        "sheets": [],
        "sheet": None,
        "offset": offset,
        "next_offset": next_offset,
        "warnings": warnings,
    }


def _xlsx_cell(value: Any, warnings: list[str]) -> Any:
    if value is None or isinstance(value, bool):
        return value
    if isinstance(value, int):
        finite(value, "Worksheet cell")
        return value
    if isinstance(value, float):
        return finite(value, "Worksheet cell")
    if isinstance(value, Decimal):
        return finite(float(value), "Worksheet cell")
    if isinstance(value, (datetime.date, datetime.time, datetime.datetime)):
        return value.isoformat()
    formula_text = getattr(value, "text", None)
    if isinstance(formula_text, str):
        _warn_once(warnings, "Workbook formulas are shown as literal formula text and were not evaluated")
        return _truncate_text(formula_text, warnings)
    if isinstance(value, str):
        if value.startswith("="):
            _warn_once(warnings, "Workbook formulas are shown as literal formula text and were not evaluated")
        return _truncate_text(value, warnings)
    return _truncate_text(str(value), warnings)


def _trimmed_xlsx_row(raw_row: tuple[Any, ...], warnings: list[str]) -> list[Any]:
    cells = list(raw_row)
    if len(cells) > MAX_COLUMNS:
        cells = cells[:MAX_COLUMNS]
        _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
    while cells and cells[-1] is None:
        cells.pop()
    return [_xlsx_cell(cell, warnings) for cell in cells]


def _preview_xlsx_table(path: Path, sheet: str | None, offset: int, limit: int) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    _check_office_archive(payload, "xlsx")
    openpyxl = library("openpyxl")
    column_letter = library("openpyxl.utils.cell").get_column_letter
    warnings: list[str] = []
    try:
        workbook = openpyxl.load_workbook(
            io.BytesIO(payload),
            read_only=True,
            data_only=False,
            keep_links=False,
        )
    except Exception as error:
        if isinstance(error, ArtifactError):
            raise
        raise ArtifactError("Unable to open XLSX workbook: " + str(error)) from error
    try:
        sheets = list(workbook.sheetnames)
        if not sheets:
            raise ArtifactError("Workbook contains no worksheets")
        selected = sheets[0] if sheet is None else sheet
        if selected not in sheets:
            raise ArtifactError("Worksheet does not exist: " + str(selected))
        worksheet = workbook[selected]
        if (worksheet.max_column or 0) > MAX_COLUMNS:
            _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
        row_iter = worksheet.iter_rows(max_col=MAX_COLUMNS + 1, values_only=True)
        first_row = next(row_iter, None)
        if first_row is None:
            return {
                "status": "passed",
                "kind": "table",
                "format": "xlsx",
                "columns": [],
                "rows": [],
                "sheets": sheets,
                "sheet": selected,
                "offset": offset,
                "next_offset": None,
                "warnings": warnings,
            }
        header_cells = _trimmed_xlsx_row(first_row, warnings)
        columns = [
            _truncate_text(str(cell), warnings)
            if cell is not None and str(cell) != ""
            else str(column_letter(index + 1))
            for index, cell in enumerate(header_cells)
        ]
        rows: list[list[Any]] = []
        next_offset: int | None = None
        row_index = 0
        for raw_row in row_iter:
            if row_index >= MAX_SCAN_ROWS:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
                break
            converted = _trimmed_xlsx_row(raw_row, warnings)
            if len(converted) > len(columns):
                for extra in range(len(columns), len(converted)):
                    columns.append(str(column_letter(extra + 1)))
            if row_index < offset:
                row_index += 1
                continue
            if len(rows) < limit:
                padded = [*converted, *([None] * max(0, len(columns) - len(converted)))]
                rows.append(padded[: len(columns)])
                row_index += 1
                continue
            if offset + len(rows) < MAX_SCAN_ROWS:
                next_offset = offset + len(rows)
            else:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
            break
        for row in rows:
            if len(row) < len(columns):
                row.extend([None] * (len(columns) - len(row)))
    finally:
        workbook.close()
    return {
        "status": "passed",
        "kind": "table",
        "format": "xlsx",
        "columns": columns,
        "rows": rows,
        "sheets": sheets,
        "sheet": selected,
        "offset": offset,
        "next_offset": next_offset,
        "warnings": warnings,
    }


def _preview_document_page(path: Path, out: Path, fmt: str, page: int) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if fmt in REQUIRED_OFFICE_PARTS:
        _check_office_archive(payload, fmt)
    elif fmt == "pdf" and not payload.lstrip().startswith(b"%PDF-"):
        raise ArtifactError("Malformed PDF input: missing %PDF- header")
    image_name = f"page-{page}.png"
    target = out / image_name
    if Path(os.path.abspath(target)) == Path(os.path.abspath(path)):
        raise ArtifactError("Preview output image must not overwrite the source file")
    rendered = render_page(
        path,
        target,
        page=page,
        max_pages=MAX_PAGES,
        max_dimension=MAX_PREVIEW_DIMENSION,
        max_output_bytes=MAX_OUTPUT_IMAGE_BYTES,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    return {
        "status": "passed",
        "kind": "pages",
        "format": fmt,
        "page": page,
        "pages": rendered["pages"],
        "image": image_name,
        "width": rendered["width"],
        "height": rendered["height"],
        "warnings": rendered["warnings"],
    }


def _preview_image(path: Path, out: Path, fmt: str, page: int) -> dict[str, Any]:
    if page != 1:
        raise ArtifactError("Image preview has 1 page; requested page " + str(page) + " is out of range")
    payload = _read_regular(path, MAX_INPUT_BYTES)
    stripped = payload[:256].lstrip().lower()
    if stripped.startswith((b"<svg", b"<!doctype html", b"<html", b"<?xml")):
        raise ArtifactError("Active markup disguised as a raster image is refused")
    pil_image = library("PIL.Image")
    warnings: list[str] = []
    image_name = "page-1.png"
    target = out / image_name
    if Path(os.path.abspath(target)) == Path(os.path.abspath(path)):
        raise ArtifactError("Preview output image must not overwrite the source file")
    try:
        with pil_image.open(io.BytesIO(payload)) as probe:
            if (
                probe.width <= 0
                or probe.height <= 0
                or probe.width > MAX_IMAGE_DIMENSION
                or probe.height > MAX_IMAGE_DIMENSION
                or probe.width * probe.height > MAX_IMAGE_PIXELS
            ):
                raise ArtifactError("Image dimensions exceed preview bounds")
            probe.verify()
        with pil_image.open(io.BytesIO(payload)) as decoded:
            decoded.load()
            if getattr(decoded, "is_animated", False) and getattr(decoded, "n_frames", 1) > 1:
                decoded.seek(0)
                _warn_once(warnings, "Animated image preview renders frame 1 only")
            mode = "RGBA" if "A" in decoded.getbands() or "transparency" in decoded.info else "RGB"
            converted = decoded.convert(mode)
            if converted.width > MAX_PREVIEW_DIMENSION or converted.height > MAX_PREVIEW_DIMENSION:
                converted.thumbnail((MAX_PREVIEW_DIMENSION, MAX_PREVIEW_DIMENSION))
                _warn_once(warnings, "Scaled image within " + str(MAX_PREVIEW_DIMENSION) + "px dimension bound")
            width, height = converted.width, converted.height
            buffer = io.BytesIO()
            converted.save(buffer, format="PNG")
            png_bytes = buffer.getvalue()
        with pil_image.open(io.BytesIO(png_bytes)) as check_img:
            check_img.verify()
    except Exception as error:
        if isinstance(error, ArtifactError):
            raise
        raise ArtifactError("Invalid or unsupported raster image: " + str(error)) from error
    if not png_bytes or len(png_bytes) > MAX_OUTPUT_IMAGE_BYTES:
        raise ArtifactError("Converted PNG exceeds the output byte limit")
    out.mkdir(parents=True, exist_ok=True)
    with output_file(target) as staged:
        staged.write_bytes(png_bytes)
    return {
        "status": "passed",
        "kind": "pages",
        "format": fmt,
        "page": 1,
        "pages": 1,
        "image": image_name,
        "width": width,
        "height": height,
        "warnings": warnings,
    }


def _preview_text(path: Path, fmt: str) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if b"\x00" in payload:
        raise ArtifactError("Text preview input contains NUL bytes")
    warnings: list[str] = []
    truncated = False
    raw_slice = payload
    if len(payload) > MAX_TEXT_BYTES:
        raw_slice = payload[:MAX_TEXT_BYTES]
        truncated = True
        _warn_once(warnings, "Truncated text preview to " + str(MAX_TEXT_BYTES) + " bytes")
    try:
        text = raw_slice.decode("utf-8-sig") if not truncated else raw_slice.decode("utf-8-sig", errors="ignore")
        if truncated:
            payload[: min(len(payload), 4096)].decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ArtifactError("Text preview input must be valid UTF-8") from error
    if fmt == "json" and not truncated:
        try:
            json.loads(text)
        except ValueError as error:
            raise ArtifactError("Malformed JSON input: " + str(error)) from error
    if len(text) > MAX_TEXT_CHARS:
        text = text[:MAX_TEXT_CHARS]
        truncated = True
        _warn_once(warnings, "Truncated text preview to " + str(MAX_TEXT_CHARS) + " characters")
    return {
        "status": "passed",
        "kind": "text",
        "format": fmt,
        "text": text,
        "truncated": truncated,
        "warnings": warnings,
    }


def preview(
    path: Path,
    out: Path,
    *,
    view: str = "auto",
    page: int = 1,
    sheet: str | None = None,
    offset: int = 0,
    limit: int = 100,
) -> dict[str, Any]:
    """Generate a bounded table, page or text preview without modifying the source file."""
    source, out_dir = _validate_paths(path, out)
    if view not in {"auto", "table", "pages", "text"}:
        raise ArtifactError("view must be auto, table or pages")
    if isinstance(page, bool) or not isinstance(page, int) or page < 1:
        raise ArtifactError("page must be a positive 1-based integer")
    if isinstance(offset, bool) or not isinstance(offset, int) or offset < 0 or offset >= MAX_SCAN_ROWS:
        raise ArtifactError("offset must be a non-negative integer within the row scan limit")
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= MAX_LIMIT:
        raise ArtifactError("limit must be an integer between 1 and " + str(MAX_LIMIT))
    if sheet is not None and (not isinstance(sheet, str) or not sheet.strip()):
        raise ArtifactError("sheet must be a nonempty worksheet name")

    suffix = source.suffix.lower()
    if suffix in MACRO_SUFFIXES:
        raise ArtifactError("Macro-enabled Office formats are refused for preview: " + suffix)
    if suffix in ACTIVE_MARKUP_SUFFIXES:
        raise ArtifactError("Active or executable markup formats (SVG/HTML/XML) are refused: " + suffix)

    if suffix == ".csv":
        if view not in {"auto", "table"}:
            raise ArtifactError("CSV files support table preview only")
        if sheet is not None:
            raise ArtifactError("CSV preview does not support --sheet")
        if page != 1:
            raise ArtifactError("Table preview uses --offset and --limit rather than --page")
        return _preview_csv(source, offset, limit)

    if suffix == ".xlsx":
        resolved_view = "table" if view == "auto" else view
        if resolved_view == "table":
            if page != 1:
                raise ArtifactError("Table preview uses --offset and --limit rather than --page")
            return _preview_xlsx_table(source, sheet, offset, limit)
        if resolved_view == "pages":
            if sheet is not None:
                raise ArtifactError("XLSX page preview does not support --sheet; use --view table for sheet selection")
            if offset != 0:
                raise ArtifactError("Page preview uses --page rather than --offset")
            return _preview_document_page(source, out_dir, "xlsx", page)
        raise ArtifactError("XLSX files support table or pages preview")

    if suffix in {".docx", ".pptx", ".pdf"}:
        fmt = suffix.lstrip(".")
        if view not in {"auto", "pages"}:
            raise ArtifactError(fmt.upper() + " files support pages preview only")
        if sheet is not None:
            raise ArtifactError(fmt.upper() + " preview does not support --sheet")
        if offset != 0:
            raise ArtifactError("Page preview uses --page rather than --offset")
        return _preview_document_page(source, out_dir, fmt, page)

    if suffix in IMAGE_FORMATS:
        fmt = IMAGE_FORMATS[suffix]
        if view not in {"auto", "pages"}:
            raise ArtifactError("Image files support pages preview only")
        if sheet is not None:
            raise ArtifactError("Image preview does not support --sheet")
        if offset != 0:
            raise ArtifactError("Image preview does not support --offset")
        return _preview_image(source, out_dir, fmt, page)

    if suffix in TEXT_FORMATS:
        fmt = TEXT_FORMATS[suffix]
        if view not in {"auto", "text"}:
            raise ArtifactError(fmt.upper() + " files support text preview only")
        if sheet is not None or page != 1 or offset != 0:
            raise ArtifactError("Text preview does not support --page, --sheet or --offset")
        return _preview_text(source, fmt)

    raise ArtifactError("Unsupported preview file format: " + (suffix or source.name))
