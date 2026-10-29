"""Transport one native preview through a credential-free container's tmpfs."""

import base64
import contextlib
import io
import json
import os
import runpy
import sys
from pathlib import Path

MAX_INPUT = 64 * 1024 * 1024
MAX_JSON = 2 * 1024 * 1024
MAX_IMAGE = 16 * 1024 * 1024
MAX_PACKET = 4 * ((MAX_INPUT + 2) // 3) + 4 * 1024 * 1024


def main():
    """Stage bounded private inputs, then invoke the canonical native launcher."""
    os.umask(0o077)
    raw = sys.stdin.buffer.read(MAX_PACKET + 1)
    if len(raw) > MAX_PACKET:
        raise ValueError("Preview input packet exceeds its bound")
    payload = json.loads(raw)
    source = base64.b64decode(payload["source"], validate=True)
    if len(source) > MAX_INPUT:
        raise ValueError("Preview source exceeds its bound")
    suffix = payload["suffix"]
    if suffix not in {"csv", "xlsx", "docx", "pptx", "pdf", "png", "jpg", "webp", "txt", "md", "json"}:
        raise ValueError("Unsupported preview suffix")
    root = Path(sys.argv[1])
    derived = Path(sys.argv[2])
    root.mkdir()
    helper = root / "helper"
    helper.mkdir()
    for name, encoded in payload["helpers"].items():
        relative = Path(name)
        if (
            relative.is_absolute()
            or ".." in relative.parts
            or not (name == "launcher.py" or name.startswith("branchyard_artifacts/"))
        ):
            raise ValueError("Invalid trusted helper path")
        target = helper / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(base64.b64decode(encoded, validate=True))
    path = root / ("source." + suffix)
    path.write_bytes(source)
    options = payload["options"]
    sys.argv = [
        str(helper / "launcher.py"),
        str(MAX_JSON),
        "--file",
        str(path),
        "--out",
        str(derived),
        "--view",
        options["view"],
        "--page",
        str(options["page"]),
        "--offset",
        str(options["offset"]),
        "--limit",
        str(options["limit"]),
    ]
    if options["sheet"] is not None:
        sys.argv.extend(["--sheet", options["sheet"]])
    captured = io.StringIO()
    with contextlib.redirect_stdout(captured):
        try:
            runpy.run_path(str(helper / "launcher.py"), run_name="__main__")
        except SystemExit as exit_status:
            if exit_status.code not in (0, 1, 2):
                raise
    raw = captured.getvalue().encode("utf-8")
    if len(raw) > MAX_JSON:
        raise ValueError("Preview metadata exceeds its bound")
    preview = json.loads(raw)
    image = None
    if preview.get("status") == "passed" and preview.get("kind") == "pages":
        name = preview["image"]
        if not isinstance(name, str) or Path(name).name != name or ".." in name:
            raise ValueError("Invalid preview image name")
        fd = os.open(derived / name, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as stream:
            data = stream.read(MAX_IMAGE + 1)
        if len(data) > MAX_IMAGE:
            raise ValueError("Preview image exceeds its bound")
        image = base64.b64encode(data).decode("ascii")
    # Keep transport encoding separate from the native metadata byte budget.
    print(json.dumps({"preview": preview, "image": image}, ensure_ascii=False, allow_nan=False))


main()
