"""File boundaries and reports shared by the format-specific tools."""

from __future__ import annotations

import hashlib
import importlib
import io
import json
import math
import os
import tempfile
import zipfile
from collections.abc import Iterator, Set
from contextlib import contextmanager
from pathlib import Path
from typing import Any


class ArtifactError(ValueError):
    """A request or artifact cannot satisfy its declared contract."""

    kind = "invalid_artifact"


class Unavailable(ArtifactError):
    """A required optional library or native capability is unavailable."""

    kind = "unavailable"


def library(name: str) -> Any:
    """Import a library on demand, without installing anything."""
    try:
        return importlib.import_module(name)
    except ImportError as error:
        raise Unavailable("Missing " + name + "; install branchyard[artifacts] explicitly") from error


def load_json(path: Path) -> dict[str, Any]:
    """Read an object, refusing nonstandard JSON numeric constants."""

    def invalid_constant(value: str) -> None:
        raise ArtifactError("JSON contains a non-finite number: " + value)

    with Path(path).open(encoding="utf-8") as source:
        value = json.load(
            source, parse_constant=invalid_constant, parse_float=lambda number: finite(float(number), "JSON number")
        )
    if not isinstance(value, dict):
        raise ArtifactError("Expected a JSON object")
    return value


def request(value: dict[str, Any], allowed: set[str], required: Set[str] = frozenset()) -> None:
    """Check version and field names at a request boundary."""
    if value.get("schema_version") != 1 or isinstance(value.get("schema_version"), bool):
        raise ArtifactError("schema_version must be 1")
    extra = set(value) - allowed - {"schema_version"}
    missing = required - set(value)
    if extra or missing:
        raise ArtifactError("Unknown fields " + str(sorted(extra)) + "; missing fields " + str(sorted(missing)))


def digest(path: Path) -> str:
    """Return a file's SHA-256, used by document specs rather than artifact IDs."""
    with Path(path).open("rb") as source:
        return _digest(source)


def _digest(source: Any) -> str:
    result = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        result.update(chunk)
    return result.hexdigest()


def contained(root: Path, relative: str) -> Path:
    """Resolve a regular file beneath a declared input root, refusing symlinks."""
    path = Path(relative)
    if path.is_absolute() or not path.parts or ".." in path.parts:
        raise ArtifactError("Expected a relative file beneath its spec: " + relative)
    current = root.resolve()
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise ArtifactError("Symlink input is refused: " + relative)
    if not current.is_file():
        raise ArtifactError("Input is not a regular file: " + relative)
    return current


def checked_file(root: Path, ref: dict[str, Any]) -> Path:
    """Resolve an input and verify its declared content digest."""
    path = contained(root, ref["path"])
    if digest(path) != ref["sha256"]:
        raise ArtifactError("SHA-256 mismatch: " + ref["path"])
    return path


@contextmanager
def output_directory(out: Path) -> Iterator[Path]:
    """Publish a complete new bundle; parallel workers must use distinct destinations."""
    out = Path(out)
    if out.exists() or out.is_symlink():
        raise ArtifactError("Output directory already exists; choose a new destination")
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="." + out.name + "-", dir=out.parent) as temporary:
        staged = Path(temporary)
        yield staged
        if out.exists() or out.is_symlink():
            raise ArtifactError("Output destination appeared during the operation")
        os.rename(staged, out)


@contextmanager
def output_file(out: Path) -> Iterator[Path]:
    """Replace the destination only after the entire operation succeeds."""
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix="." + out.stem + "-", suffix=out.suffix, dir=out.parent)
    os.close(descriptor)
    staged = Path(name)
    try:
        yield staged
        with staged.open("rb") as source:
            os.fsync(source.fileno())
        os.replace(staged, out)
    finally:
        staged.unlink(missing_ok=True)


def canonical_zip(path: Path) -> None:
    """Normalize ZIP containers, including embedded chart workbooks, for reproducibility."""

    def normalize(data: bytes) -> bytes:
        output = io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(data)) as source, zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as target:
            for name in sorted(source.namelist()):
                payload = source.read(name)
                if name.endswith(".xlsx"):
                    payload = normalize(payload)
                # Created packages and embedded chart workbooks use a fixed document time.
                # Updates do not call this normalizer, so their metadata stays intact.
                if name == "docProps/core.xml":
                    from xml.etree import ElementTree

                    root = ElementTree.fromstring(payload)
                    for node in root:
                        if node.tag.rsplit("}", 1)[-1] in {"created", "modified"}:
                            node.text = "2000-01-01T00:00:00Z"
                    payload = ElementTree.tostring(root, encoding="utf-8", xml_declaration=True)
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o100644 << 16
                target.writestr(info, payload)
        return output.getvalue()

    path.write_bytes(normalize(path.read_bytes()))


def finite(value: Any, name: str, positive: bool = False) -> float:
    """Require a finite number, optionally greater than zero."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ArtifactError(name + " must be a finite number")
    if positive and value <= 0:
        raise ArtifactError(name + " must be positive")
    return float(value)


def report(path: Path, media_type: str, **details: Any) -> dict[str, Any]:
    """Describe bytes that were actually written."""
    return {"status": "passed", "path": str(path), "sha256": digest(path), "media_type": media_type, **details}


def check_report(**details: Any) -> dict[str, Any]:
    """Keep structural evidence separate from rendering and recalculation."""
    return {"status": "passed", "structural": "passed", "rendered": "unavailable", **details}
