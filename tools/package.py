#!/usr/bin/env python3
"""Build reproducible plugin, standalone-skill and Python SDK archives from
one canonical source: `plugins/branchyard`, `plugins/branchyard/skills/delegate`,
`plugins/branchyard/skills/setup` and `sdk/python`. See `docs/distribution.md`.
The plugin archive is the whole of `plugins/branchyard`, so it carries each
harness's files (`harnesses/`) and the install manifests `by init plugin`
reads (`install/`); executable files keep their executable bit.

Determinism: inputs are read in sorted path order, every archive entry gets
a fixed timestamp and mode, and compression is deflate at a fixed level, so
two builds from the same source tree are byte-identical (`tests/test_distribution.py`
checks this).
"""

import argparse
import hashlib
import json
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugins" / "branchyard"
SKILL = PLUGIN / "skills" / "delegate"
SETUP_SKILL = PLUGIN / "skills" / "setup"
SDK = ROOT / "sdk" / "python"
MARKETPLACE = ROOT / ".claude-plugin" / "marketplace.json"
# Each host's manifest for the plugin, relative to it: each names it
# `branchyard` at the workspace version.
HOST_MANIFESTS = [
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".github/plugin/plugin.json",
    "harnesses/gemini-cli/gemini-extension.json",
]

# A fixed timestamp for every archive entry (ZIP's minimum representable
# date-time), so archives from different builds never differ only by mtime.
FIXED_TIME = (1980, 1, 1, 0, 0, 0)
# Which build (kind, source root, archive root directory) produces which
# archive.
ARCHIVES = [
    ("plugin", PLUGIN, "branchyard"),
    ("skill", SKILL, "delegate"),
    ("setup-skill", SETUP_SKILL, "setup"),
    ("sdk", SDK, "branchyard-sdk"),
]


def _generated(relative: Path) -> bool:
    """Whether `relative` is a build's leftover rather than a source: bytecode,
    or what `pip install ./sdk/python` writes into the tree it builds from."""
    parts = relative.parts
    return (
        "__pycache__" in parts
        or relative.suffix == ".pyc"
        or parts[0] == "build"
        or any(part.endswith(".egg-info") for part in parts)
    )


def inputs(root: Path) -> dict:
    """Every regular file under `root`, by its path relative to it, sorted,
    less build leftovers."""
    result = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if _generated(relative):
            continue
        if path.is_symlink():
            raise ValueError(f"package inputs must not be symlinks: {path}")
        if path.is_file():
            result[relative.as_posix()] = path.read_bytes()
    return result


def executables(root: Path) -> set:
    """The files under `root` with an executable bit, by relative path: they
    are archived 0755 (a harness's hook script must stay runnable), every
    other file 0644."""
    return {
        path.relative_to(root).as_posix()
        for path in root.rglob("*")
        if path.is_file() and not path.is_symlink() and path.stat().st_mode & 0o111
    }


def sdk_version() -> str:
    """`sdk/python/pyproject.toml`'s `project.version`."""
    return tomllib.loads((SDK / "pyproject.toml").read_text())["project"]["version"]


def archive(path: Path, files: dict, root_name: str, executable: frozenset = frozenset()) -> None:
    """Write `files` (name -> bytes) under `root_name/` in a new zip at
    `path`, plus a `root_name/MANIFEST.sha256.json` of their digests; the
    names in `executable` get mode 0755."""
    manifest = {name: hashlib.sha256(data).hexdigest() for name, data in sorted(files.items())}
    files = dict(files)
    files["MANIFEST.sha256.json"] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as output:
        for name, data in sorted(files.items()):
            info = zipfile.ZipInfo(f"{root_name}/{name}", date_time=FIXED_TIME)
            info.create_system = 3  # Unix, so external_attr below is meaningful.
            info.external_attr = (0o100755 if name in executable else 0o100644) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            output.writestr(info, data)


def check_marketplace(version: str) -> None:
    """`.claude-plugin/marketplace.json` at the repository root, which
    `/plugin marketplace add` reads from a clone: it must list this plugin
    by name, at its directory, at the workspace version."""
    marketplace = json.loads(MARKETPLACE.read_text())
    entries = [p for p in marketplace.get("plugins", []) if p.get("name") == "branchyard"]
    if (
        len(entries) != 1
        or not marketplace.get("owner", {}).get("name")
        or entries[0].get("source") != "./" + PLUGIN.relative_to(ROOT).as_posix()
        or entries[0].get("version") != version
        or marketplace.get("metadata", {}).get("version") != version
    ):
        raise ValueError(
            f"{MARKETPLACE.relative_to(ROOT)} must name an owner and list the branchyard plugin once, "
            f"at ./plugins/branchyard, with version {version!r}, the workspace version"
        )


def build(destination: Path) -> list[Path]:
    """Build every archive into `destination`; returns their paths, in the
    order of `ARCHIVES`."""
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    for host in HOST_MANIFESTS:
        manifest = json.loads((PLUGIN / host).read_text())
        if manifest["name"] != "branchyard" or manifest["version"] != version:
            raise ValueError(f"{host}'s name and version must be 'branchyard' and {version!r}, the workspace version")
    if sdk_version() != version:
        raise ValueError(f"sdk/python/pyproject.toml's version must be {version!r}, the workspace version")
    check_marketplace(version)
    destination.mkdir(parents=True, exist_ok=True)
    license_bytes = (ROOT / "LICENSE").read_bytes()
    paths = []
    for kind, root, root_name in ARCHIVES:
        files = inputs(root)
        files["LICENSE"] = license_bytes
        path = destination / f"branchyard-{kind}-{version}.zip"
        archive(path, files, root_name, frozenset(executables(root)))
        paths.append(path)
    return paths


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for built in build(args.output.resolve()):
        print(json.dumps({"path": str(built), "sha256": hashlib.sha256(built.read_bytes()).hexdigest()}))
