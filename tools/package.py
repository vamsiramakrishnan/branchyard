#!/usr/bin/env python3
"""Build reproducible plugin and standalone-skill archives from one canonical source."""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
SKILL = PLUGIN / "skills" / "branchyard"


def inputs(root):
    result = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("Package inputs must not be symlinks")
        if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc":
            result[path.relative_to(root).as_posix()] = path.read_bytes()
    return result


def archive(path, files):
    manifest = {name: hashlib.sha256(data).hexdigest() for name, data in sorted(files.items())}
    files = dict(files)
    files["MANIFEST.sha256.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
        for name, data in sorted(files.items()):
            info = zipfile.ZipInfo("branchyard/" + name, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            output.writestr(info, data)


def build(destination):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    for host in [".codex-plugin", ".claude-plugin"]:
        manifest = json.loads((PLUGIN / host / "plugin.json").read_text())
        if manifest["name"] != "branchyard" or manifest["version"] != version:
            raise ValueError("Plugin and Cargo versions must match")
    destination.mkdir(parents=True, exist_ok=True)
    license_bytes = (ROOT / "LICENSE").read_bytes()
    paths = []
    for kind, root in [("plugin", PLUGIN), ("skill", SKILL)]:
        files = inputs(root)
        files["LICENSE"] = license_bytes
        path = destination / f"branchyard-{kind}-{version}.zip"
        archive(path, files)
        paths.append(path)
    return paths


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for path in build(args.output.resolve()):
        print(json.dumps({"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}))
