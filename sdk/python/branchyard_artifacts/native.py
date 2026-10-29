"""Explicit native rendering and qualified UNO pivot operations."""

from __future__ import annotations

import io
import json
import math
import os
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path
from typing import Any
from xml.sax.saxutils import escape

from .common import ArtifactError, Unavailable, library, load_json, output_file, report, request
from .doctor import uno_python

MAX_RENDER_PAGES = 500
MAX_RENDER_DIMENSION = 2048
MAX_RENDER_IMAGE_BYTES = 16 * 1024 * 1024
MAX_RENDER_PIXELS = 16_777_216
RENDER_TIMEOUT_SECONDS = 60


def write_hardened_profile(profile_dir: Path) -> str:
    """Create a fresh per-call LibreOffice user profile with macros and link updates disabled."""
    user_dir = profile_dir / "user"
    user_dir.mkdir(parents=True, exist_ok=True)
    xcu = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<oor:items xmlns:oor="http://openoffice.org/2001/registry" '
        'xmlns:xs="http://www.w3.org/2001/XMLSchema" '
        'xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">\n'
        '  <item oor:path="/org.openoffice.Office.Common/Security/Scripting">'
        '<prop oor:name="MacroSecurityLevel" oor:op="fuse"><value>3</value></prop></item>\n'
        '  <item oor:path="/org.openoffice.Office.Common/Security/Scripting">'
        '<prop oor:name="BlockUntrustedRefererLinks" oor:op="fuse"><value>true</value></prop></item>\n'
        '  <item oor:path="/org.openoffice.Office.Calc/Content/Update">'
        '<prop oor:name="Link" oor:op="fuse"><value>2</value></prop></item>\n'
        '  <item oor:path="/org.openoffice.Office.Writer/Content/Update">'
        '<prop oor:name="Link" oor:op="fuse"><value>0</value></prop></item>\n'
        "</oor:items>\n"
    )
    (user_dir / "registrymodifications.xcu").write_text(xcu, encoding="utf-8")
    return profile_dir.as_uri()


def render_page(
    path: Path,
    out_image: Path,
    *,
    page: int = 1,
    max_pages: int = MAX_RENDER_PAGES,
    max_dimension: int = MAX_RENDER_DIMENSION,
    max_output_bytes: int = MAX_RENDER_IMAGE_BYTES,
    timeout: int = RENDER_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    """Render and verify a single bounded page without rasterizing the entire document."""
    if isinstance(page, bool) or not isinstance(page, int) or page < 1:
        raise ArtifactError("page must be a positive 1-based integer")
    if page > max_pages:
        raise ArtifactError("Requested page " + str(page) + " exceeds the " + str(max_pages) + "-page limit")
    suffix = path.suffix.lower()
    raster = shutil.which("pdftoppm")
    office = shutil.which("libreoffice") or shutil.which("soffice")
    if not raster or (suffix != ".pdf" and not office):
        raise Unavailable(
            "Page preview requires pdftoppm (Poppler) and, for Office documents, LibreOffice; "
            "install poppler-utils and libreoffice or use the branchyard-artifacts runtime image"
        )
    pypdf = library("pypdf")
    pil_image = library("PIL.Image")
    warnings: list[str] = []
    with tempfile.TemporaryDirectory(prefix="branchyard-page-render-") as temporary:
        work = Path(temporary)
        input_dir = work / "input"
        input_dir.mkdir(parents=True, exist_ok=True)
        staged_input = input_dir / path.name
        shutil.copyfile(path, staged_input)
        if suffix == ".pdf":
            pdf = staged_input
        else:
            profile = write_hardened_profile(work / "profile")
            try:
                subprocess.run(
                    [
                        str(office),
                        "-env:UserInstallation=" + profile,
                        "--headless",
                        "--nolockcheck",
                        "--nodefault",
                        "--nofirststartwizard",
                        "--norestore",
                        "--convert-to",
                        "pdf",
                        "--outdir",
                        str(work),
                        str(staged_input),
                    ],
                    capture_output=True,
                    check=True,
                    timeout=timeout,
                )
            except subprocess.TimeoutExpired as error:
                raise ArtifactError("LibreOffice conversion timed out") from error
            except (OSError, subprocess.SubprocessError) as error:
                raise ArtifactError("LibreOffice failed to convert " + escape(path.name) + " to PDF") from error
            pdf = work / (path.stem + ".pdf")
            if not pdf.is_file() or pdf.stat().st_size == 0:
                raise ArtifactError("LibreOffice completed without producing the requested PDF")
        try:
            reader = pypdf.PdfReader(str(pdf), strict=True)
        except Exception as error:
            if isinstance(error, (ArtifactError, Unavailable)):
                raise
            raise ArtifactError("Unable to read PDF page structure: " + str(error)) from error
        if reader.is_encrypted:
            raise ArtifactError("Encrypted PDF requires an explicit decryption workflow")
        count = len(reader.pages)
        if count < 1:
            raise ArtifactError("Renderer produced no pages")
        if count > max_pages:
            raise ArtifactError(
                "Document page count (" + str(count) + ") exceeds the " + str(max_pages) + "-page limit"
            )
        if page > count:
            raise ArtifactError("Requested page " + str(page) + " exceeds document page count (" + str(count) + ")")
        target_page = reader.pages[page - 1]
        width_pt = float(target_page.mediabox.width)
        height_pt = float(target_page.mediabox.height)
        if not math.isfinite(width_pt) or not math.isfinite(height_pt) or width_pt <= 0 or height_pt <= 0:
            raise ArtifactError("PDF page has invalid dimensions")
        dpi = min(96, max(18, int(max_dimension * 72.0 / max(width_pt, height_pt))))
        try:
            subprocess.run(
                [
                    str(raster),
                    "-f",
                    str(page),
                    "-l",
                    str(page),
                    "-r",
                    str(dpi),
                    "-png",
                    str(pdf),
                    str(work / "page"),
                ],
                capture_output=True,
                check=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as error:
            raise ArtifactError("Poppler page rasterization timed out") from error
        except (OSError, subprocess.SubprocessError) as error:
            raise ArtifactError("Poppler failed to rasterize page " + str(page)) from error
        images = sorted(work.glob("page*.png"))
        if len(images) != 1:
            raise ArtifactError("Raster page output differs from the requested single page")
        raw_png = images[0].read_bytes()
        if not raw_png or len(raw_png) > max_output_bytes:
            raise ArtifactError("Rendered page image exceeds the output byte limit")
        try:
            with pil_image.open(io.BytesIO(raw_png)) as verified:
                if verified.width <= 0 or verified.height <= 0 or verified.width * verified.height > MAX_RENDER_PIXELS:
                    raise ArtifactError("Rendered page dimensions exceed preview pixel bounds")
                verified.verify()
            with pil_image.open(io.BytesIO(raw_png)) as decoded:
                decoded.load()
                if decoded.width > max_dimension or decoded.height > max_dimension:
                    decoded.thumbnail((max_dimension, max_dimension))
                    warnings.append("Scaled rendered page within " + str(max_dimension) + "px dimension bound")
                    buffer = io.BytesIO()
                    decoded.save(buffer, format="PNG")
                    raw_png = buffer.getvalue()
                width, height = decoded.width, decoded.height
        except Exception as error:
            if isinstance(error, (ArtifactError, Unavailable)):
                raise
            raise ArtifactError("Rendered PNG failed image verification") from error
        if not raw_png or len(raw_png) > max_output_bytes:
            raise ArtifactError("Rendered page image exceeds the output byte limit")
        out_image.parent.mkdir(parents=True, exist_ok=True)
        with output_file(out_image) as staged:
            staged.write_bytes(raw_png)
    return {
        "pages": count,
        "width": width,
        "height": height,
        "warnings": warnings,
    }


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
            profile = write_hardened_profile(work / "profile")
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
