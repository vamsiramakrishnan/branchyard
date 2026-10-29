"""A producer-neutral list of generated files for after-transfer collection."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

from .common import ArtifactError, contained, digest, output_file

MEDIA_TYPES = {
    ".docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ".xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ".pptx": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ".pdf": "application/pdf",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".svg": "image/svg+xml",
    ".json": "application/json",
    ".txt": "text/plain",
    ".csv": "text/csv",
}


def create(files: list[Path], out: Path) -> dict[str, Any]:
    """Hash only explicit regular files beneath the manifest directory; never publish them."""
    root = out.parent.resolve()
    entries = []
    seen: set[str] = set()
    for file in files:
        absolute = Path(os.path.abspath(file))
        try:
            relative = absolute.relative_to(root).as_posix()
        except ValueError as error:
            raise ArtifactError("Captured files must be beneath the manifest directory: " + str(file)) from error
        path = contained(root, relative)
        if path == out.absolute() or relative in seen:
            raise ArtifactError("Manifest cannot capture itself or duplicate a file: " + relative)
        seen.add(relative)
        media_type = MEDIA_TYPES.get(path.suffix.lower())
        if media_type is None:
            raise ArtifactError("No known document media type for " + path.suffix)
        entries.append({"path": relative, "name": path.name, "media_type": media_type, "sha256": digest(path)})
    if not entries:
        raise ArtifactError("Capture manifest needs at least one file")
    value = {"schema_version": 1, "files": sorted(entries, key=lambda entry: entry["path"])}
    with output_file(out) as staged:
        staged.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return {"status": "passed", "path": str(out), "files": value["files"], "published": False}
