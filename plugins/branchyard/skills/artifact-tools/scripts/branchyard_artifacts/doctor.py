"""Report optional runtime capabilities without installing dependencies."""

from __future__ import annotations

import importlib.metadata
import importlib.util
import shutil
import subprocess
from typing import Any


def uno_python() -> str | None:
    """Find a Python interpreter that can load the system LibreOffice UNO bridge."""
    for program in ["/usr/bin/python3", shutil.which("python3")]:
        if not program:
            continue
        try:
            result = subprocess.run([program, "-c", "import uno"], capture_output=True, timeout=10)
        except (OSError, subprocess.SubprocessError):
            continue
        if result.returncode == 0:
            return program
    return None


def doctor(required: list[str] | None = None) -> dict[str, Any]:
    """Separate installed libraries from native rendering and pivot capabilities."""
    modules = {
        "docx": ("docx", "python-docx"),
        "xlsx": ("openpyxl", "openpyxl"),
        "pdf": ("pypdf", "pypdf"),
        "pptx": ("pptx", "python-pptx"),
        "pdf-create": ("reportlab", "reportlab"),
        "schema": ("jsonschema", "jsonschema"),
        "images": ("PIL", "Pillow"),
        "pdf-tables": ("pdfplumber", "pdfplumber"),
        "data-clean": ("pandas", "pandas"),
        "calculate": ("pint", "Pint"),
    }
    capabilities: dict[str, Any] = {}
    for name, (module, distribution) in modules.items():
        available = importlib.util.find_spec(module) is not None
        info: dict[str, Any] = {"status": "passed" if available else "unavailable"}
        if available:
            info["version"] = importlib.metadata.version(distribution)
        capabilities[name] = info
    office, raster = shutil.which("libreoffice") or shutil.which("soffice"), shutil.which("pdftoppm")
    capabilities["render"] = {
        "status": "passed" if office and raster else "unavailable",
        "libreoffice": office,
        "pdftoppm": raster,
    }
    capabilities["pivot"] = {
        "status": "passed" if office and uno_python() else "unavailable",
        "qualification": "requires fixture check; installation alone does not qualify native pivot output",
    }
    capabilities["fonts"] = {"status": "passed" if shutil.which("fc-match") else "unavailable"}
    required = required or []
    missing = [name for name in required if name not in capabilities or capabilities[name]["status"] != "passed"]
    return {
        "status": "unavailable" if missing else "passed",
        "capabilities": capabilities,
        "missing": missing,
        "next": "Install the artifacts extra and native runtime explicitly; see docs/artifact-toolkit.md",
    }
