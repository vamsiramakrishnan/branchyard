"""Explicit native rendering and qualified UNO pivot operations."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path
from typing import Any

from .common import ArtifactError, Unavailable, library, load_json, output_file, report, request
from .doctor import uno_python


def render(path: Path, out: Path) -> dict[str, Any]:
    """Render every page into a new preview directory, exposing conversion failures."""
    raster = shutil.which("pdftoppm")
    office = shutil.which("libreoffice") or shutil.which("soffice")
    if not raster or (path.suffix.lower() != ".pdf" and not office):
        raise Unavailable("Rendering needs pdftoppm and, for Office files, LibreOffice")
    if out.exists():
        raise ArtifactError("Preview directory already exists; choose a new --preview-dir")
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".render-", dir=out.parent) as temporary:
        work = Path(temporary)
        if path.suffix.lower() == ".pdf":
            pdf = work / "preview.pdf"
            shutil.copyfile(path, pdf)
        else:
            profile = (work / "profile").as_uri()
            subprocess.run(
                [
                    str(office),
                    "-env:UserInstallation=" + profile,
                    "--headless",
                    "--convert-to",
                    "pdf",
                    "--outdir",
                    str(work),
                    str(path.resolve()),
                ],
                capture_output=True,
                check=True,
                timeout=90,
            )
            pdf = work / (path.stem + ".pdf")
            if not pdf.is_file():
                raise ArtifactError("LibreOffice completed without producing the requested PDF")
        reader = library("pypdf").PdfReader(str(pdf), strict=True)
        count = len(reader.pages)
        if not count:
            raise ArtifactError("Renderer produced no pages")
        subprocess.run(
            [raster, "-r", "96", "-png", str(pdf), str(work / "page")], capture_output=True, check=True, timeout=90
        )
        images = sorted(work.glob("page-*.png"))
        if len(images) != count:
            raise ArtifactError("Raster page inventory differs from rendered PDF")
        for image in images:
            with library("PIL.Image").open(image) as pixels:
                pixels.verify()
        profile_dir = work / "profile"
        if profile_dir.exists():
            shutil.rmtree(profile_dir)
        os.replace(work, out)
    return {
        "status": "passed",
        "rendered": "passed",
        "pages": count,
        "preview_dir": str(out),
        "visual_review": "not_performed",
    }


def create_pivot(path: Path, spec: Path, out: Path) -> dict[str, Any]:
    """Generate and independently reopen native pivot table/cache parts via LibreOffice UNO."""
    from .xlsx import MEDIA, check

    value = load_json(spec)
    request(
        value,
        {"name", "source_sheet", "source_range", "target_sheet", "target_cell", "rows", "columns", "data"},
        {"name", "source_sheet", "source_range", "target_sheet", "target_cell", "rows", "columns", "data"},
    )
    office = shutil.which("libreoffice") or shutil.which("soffice")
    python = uno_python()
    if not office or not python:
        raise Unavailable(
            "Native pivot creation needs LibreOffice Calc and python3-uno; no static summary was substituted"
        )
    if not isinstance(value["data"], list) or not value["data"]:
        raise ArtifactError("Pivot data must name at least one field")
    for field in value["data"]:
        if set(field) != {"field", "function"} or field["function"] not in {"SUM", "COUNT", "AVERAGE", "MIN", "MAX"}:
            raise ArtifactError("Pivot data fields need field and SUM/COUNT/AVERAGE/MIN/MAX function")
    with output_file(out) as staged, tempfile.TemporaryDirectory(prefix="branchyard-pivot-") as temporary:
        payload = Path(temporary) / "request.json"
        payload.write_text(
            json.dumps(
                {
                    "input": str(path.resolve()),
                    "output": str(staged.resolve()),
                    "office": office,
                    "work": temporary,
                    "spec": value,
                }
            )
        )
        subprocess.run(
            [python, str(Path(__file__).with_name("uno_backend.py")), str(payload)],
            capture_output=True,
            check=True,
            timeout=90,
        )
        with zipfile.ZipFile(staged) as archive:
            parts = archive.namelist()
            if (
                not any(name.startswith("xl/pivotTables/pivotTable") for name in parts)
                or not any(name.startswith("xl/pivotCache/pivotCacheDefinition") for name in parts)
                or not any(name.startswith("xl/pivotCache/pivotCacheRecords") for name in parts)
            ):
                raise ArtifactError("UNO output has no native pivot table/cache; refusing unqualified output")
        evidence = check(staged)
        if evidence["native_pivots"] < 1:
            raise ArtifactError("Independent workbook reader found no native pivot")
    return report(
        out,
        MEDIA,
        native_pivots=evidence["native_pivots"],
        structural="passed",
        calculated="passed",
        rendered="unavailable",
        backend="libreoffice-uno",
    )
