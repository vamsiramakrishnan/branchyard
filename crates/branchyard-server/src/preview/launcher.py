"""Apply process limits before loading the trusted document-preview helper."""

import importlib
import json
import os
import resource
import sys
from pathlib import Path

resource.setrlimit(resource.RLIMIT_CPU, (35, 35))
resource.setrlimit(resource.RLIMIT_FSIZE, (32 * 1024 * 1024, 32 * 1024 * 1024))
resource.setrlimit(resource.RLIMIT_AS, (2 * 1024 * 1024 * 1024, 2 * 1024 * 1024 * 1024))
resource.setrlimit(resource.RLIMIT_NOFILE, (128, 128))
os.umask(0o077)
sys.path.insert(0, str(Path(__file__).parent))

# The server, pipe reader and cache use this exact encoded-byte budget. Apply
# it before stdout so a valid wide table remains usable through pagination.
artifact_cli = importlib.import_module("branchyard_artifacts.__main__")

byte_limit = int(sys.argv[1]) - 1  # Reserve the final newline from print().
execute = artifact_cli.execute


def bounded_response(value):
    """Keep whole table rows within the response budget without changing cells."""

    def encoded(item):
        return len(json.dumps(item, ensure_ascii=False, allow_nan=False).encode("utf-8"))

    if encoded(value) <= byte_limit:
        return value
    if value.get("status") != "passed" or value.get("kind") != "table":
        raise ValueError("Preview metadata exceeds the response byte limit")
    rows = value["rows"]
    result = {**value, "rows": [], "warnings": list(value.get("warnings", []))}
    sheets = result.get("sheets", [])
    if len(sheets) > 1000:
        result["sheets"] = sheets[:1000]
        selected = result.get("sheet")
        if selected and selected not in result["sheets"]:
            result["sheets"].append(selected)
        result["warnings"].append("Worksheet navigation is limited to 1000 names")
    result["warnings"].append("Rows limited by preview response bytes; continue at next_offset")
    offset = value.get("offset", 0)
    result["next_offset"] = offset + len(rows)
    used = encoded(result)
    kept = []
    for row in rows:
        addition = encoded(row) + (2 if kept else 0)
        if used + addition > byte_limit:
            break
        kept.append(row)
        used += addition
    if not kept and rows:
        raise ValueError("One row or its table metadata exceeds the preview response byte limit")
    result["rows"] = kept
    result["next_offset"] = offset + len(kept)
    return result


def execute_bounded(args):
    """Adapt the canonical CLI only at its response boundary."""
    return bounded_response(execute(args))


artifact_cli.execute = execute_bounded
raise SystemExit(artifact_cli.main(["preview", *sys.argv[2:]]))
