#!/usr/bin/env python3
"""Build reproducible plugin, standalone-skill and Python SDK archives from
one canonical source: `plugins/branchyard`, `plugins/branchyard/skills/delegate`
and `sdk/python`. See `docs/distribution.md`.

Determinism: inputs are read in sorted path order, every archive entry gets
a fixed timestamp and mode, and compression is deflate at a fixed level, so
two builds from the same source tree are byte-identical (`tests/test_distribution.py`
checks this).
"""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
SKILL = PLUGIN / "skills" / "delegate"
SDK = ROOT / "sdk" / "python"

# A fixed timestamp for every archive entry (ZIP's minimum representable
# date-time), so archives from different builds never differ only by mtime.
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
# Which build (kind, source root, archive root directory) produces which
# archive.
ARCHIVES = [
    ("plugin", PLUGIN, "branchyard"),
    ("skill", SKILL, "delegate"),
    ("sdk", SDK, "branchyard-sdk"),
]


def inputs(root: Path) -> dict:
    """Every regular file under `root`, by its path relative to it, sorted."""
    result = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"package inputs must not be symlinks: {path}")
        if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc":
            result[path.relative_to(root).as_posix()] = path.read_bytes()
    return result


def archive(path: Path, files: dict, root_name: str) -> None:
    """Write `files` (name -> bytes) under `root_name/` in a new zip at
    `path`, plus a `root_name/MANIFEST.sha256.json` of their digests."""
    manifest = {name: hashlib.sha256(data).hexdigest() for name, data in sorted(files.items())}
    files = dict(files)
    files["MANIFEST.sha256.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as output:
        for name, data in sorted(files.items()):
            info = zipfile.ZipInfo(f"{root_name}/{name}", date_time=FIXED_TIME)
            info.create_system = 3  # Unix, so external_attr below is meaningful.
            info.external_attr = 0o100644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            output.writestr(info, data)


def build(destination: Path) -> list[Path]:
    """Build every archive into `destination`; returns their paths, in the
    order of `ARCHIVES`."""
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    for host in [".codex-plugin", ".claude-plugin"]:
        manifest = json.loads((PLUGIN / host / "plugin.json").read_text())
        if manifest["name"] != "branchyard" or manifest["version"] != version:
            raise ValueError(
                f"{host}/plugin.json's name and version must be 'branchyard' and {version!r}, "
                f"the workspace version"
            )
    destination.mkdir(parents=True, exist_ok=True)
    license_bytes = (ROOT / "LICENSE").read_bytes()
    paths = []
    for kind, root, root_name in ARCHIVES:
        files = inputs(root)
        files["LICENSE"] = license_bytes
        path = destination / f"branchyard-{kind}-{version}.zip"
        archive(path, files, root_name)
        paths.append(path)
    return paths


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for built in build(args.output.resolve()):
        print(json.dumps({"path": str(built), "sha256": hashlib.sha256(built.read_bytes()).hexdigest()}))
