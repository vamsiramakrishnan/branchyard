"""Extract actual PDF/Office tables with source identity and explicit review limits."""

from __future__ import annotations

import datetime
import hashlib
import importlib.metadata
import io
import json
import stat
import sys
import zipfile
from pathlib import Path
from typing import Any

from .common import ArtifactError, Unavailable, finite, library, output_file

MAX_BYTES = 64 * 1024 * 1024
MAX_PAGES = 200
MAX_TABLES = 1000
MAX_CELLS = 1_000_000
MAX_OUTPUT = 32 * 1024 * 1024


def _source(path: Path) -> bytes:
    """Read bounded regular source bytes once so hashes describe the parsed input."""
    path = Path(path)
    if path.is_symlink() or not stat.S_ISREG(path.stat().st_mode):
        raise ArtifactError("Table input must be a regular file, not a symlink")
    with path.open("rb") as source:
        data = source.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ArtifactError("Table input exceeds 64 MiB")
    return data


def _office(data: bytes) -> io.BytesIO:
    """Bound the expanded Office package before handing it to its established reader."""
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        if len(archive.infolist()) > 10_000 or sum(item.file_size for item in archive.infolist()) > 128 * 1024 * 1024:
            raise ArtifactError("Expanded Office package exceeds the table extraction limit")
    return io.BytesIO(data)


def _cell(value: Any) -> Any:
    """Keep library values JSON-compatible without inventing calculated cell caches."""
    if isinstance(value, (datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        finite(value, "Table cell")
    if value is not None and not isinstance(value, (str, int, float, bool)):
        raise ArtifactError("Unsupported table cell value")
    return value


def extract(path: Path, out: Path) -> dict[str, Any]:
    """Extract bounded native tables; distinguish no detections from suspected scanned pages."""
    path, out = Path(path), Path(out)
    if out.resolve() == path.resolve() or (out.exists() and out.samefile(path)):
        raise ArtifactError("Table output must preserve the source file")
    data = _source(path)
    tables: list[dict[str, Any]] = []
    warnings: list[str] = []
    scanned: list[int] = []
    cells = 0

    def append(locator: dict[str, Any], rows: list[list[Any]], **details: Any) -> None:
        nonlocal cells
        cells += sum(len(row) for row in rows)
        if len(tables) >= MAX_TABLES or cells > MAX_CELLS:
            raise ArtifactError("Detected tables exceed 1000 tables or one million cells")
        tables.append({"locator": locator, "rows": rows, **details})

    extension = path.suffix.lower()
    if extension == ".pdf":
        if sys.version_info < (3, 10):
            raise Unavailable("PDF table extraction requires Python 3.10 or newer; DOCX and XLSX remain available")
        backend = "pdfplumber"
        with library(backend).open(io.BytesIO(data)) as document:
            pages = len(document.pages)
            if pages > MAX_PAGES:
                raise ArtifactError("PDF exceeds 200 pages")
            for number, page in enumerate(document.pages, 1):
                if not page.chars and page.images:
                    scanned.append(number)
                for index, table in enumerate(page.find_tables(), 1):
                    append(
                        {"page": number, "table": index},
                        table.extract(),
                        bbox=[finite(value, "Table bounding box") for value in table.bbox],
                        bbox_unit="PDF point",
                    )
        if scanned:
            warnings.append("Image-only pages may require OCR; table extraction did not perform OCR")
    elif extension == ".docx":
        backend = "python-docx"
        document = library("docx").Document(_office(data))
        pages = None
        for index, table in enumerate(document.tables, 1):
            append({"table": index}, [[cell.text for cell in row.cells] for row in table.rows])
    elif extension == ".xlsx":
        backend = "openpyxl"
        workbook = library(backend).load_workbook(_office(data), data_only=False, read_only=False)
        pages = None
        try:
            for sheet in workbook:
                for name, table in sheet.tables.items():
                    # TableList.items() returns ranges; indexing retrieves the actual Table.
                    del table
                    ref = sheet.tables[name].ref
                    bounds = library("openpyxl.utils.cell").range_boundaries(ref)
                    if (bounds[2] - bounds[0] + 1) * (bounds[3] - bounds[1] + 1) + cells > MAX_CELLS:
                        raise ArtifactError("Native table exceeds one million cells")
                    rows = [
                        [_cell(cell.value) for cell in row]
                        for row in sheet.iter_rows(
                            min_col=bounds[0], min_row=bounds[1], max_col=bounds[2], max_row=bounds[3]
                        )
                    ]
                    append({"sheet": sheet.title, "table": name, "range": ref}, rows)
        finally:
            workbook.close()
        warnings.append("Only native named Excel tables were extracted; formulas were preserved and not evaluated")
    else:
        raise ArtifactError("Table extraction supports PDF, DOCX and native named XLSX tables")
    state = (
        "partial_ocr_required"
        if scanned and tables
        else "ocr_required"
        if scanned
        else "detected"
        if tables
        else "no_tables_detected"
    )
    value = {
        "schema_version": 1,
        "status": "unavailable" if scanned and not tables else "passed",
        "source": {"name": path.name, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)},
        "extractor": {"name": backend, "version": importlib.metadata.version(backend)},
        "extraction_status": state,
        "extraction_review": "not_performed",
        "pages_attempted": pages,
        "suspected_scan_pages": scanned,
        "table_count": len(tables),
        "tables": tables,
        "warnings": warnings,
    }
    encoded = json.dumps(value, ensure_ascii=False, allow_nan=False, indent=2).encode("utf-8") + b"\n"
    if len(encoded) > MAX_OUTPUT:
        raise ArtifactError("Table extraction report exceeds 32 MiB")
    with output_file(out) as staged:
        staged.write_bytes(encoded)
    return {**value, "path": str(out), "sha256": hashlib.sha256(encoded).hexdigest()}
