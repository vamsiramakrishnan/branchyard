#!/usr/bin/env python3
"""The release version, kept in one place: the workspace's `Cargo.toml`.

    python3 tools/release_version.py                  check every version field; print the version
    python3 tools/release_version.py --tag v0.1.0     and check that the tag names it
    python3 tools/release_version.py --pep440         print it as the Python SDK spells it
    python3 tools/release_version.py --set 0.1.0      rewrite every field, Cargo.lock included

Every file that carries Branchyard's own version must carry the workspace's:
the crates' path dependencies on one another and their Cargo.lock entries,
each host's plugin manifest, both marketplaces, the Herdr plugin, and the
Python SDK, which spells it the PEP 440 way (`0.1.0-rc.1` is `0.1.0rc1`).
A release version is `X.Y.Z` or `X.Y.Z-alpha.N`, `-beta.N` or `-rc.N`: the
forms both Cargo and Python can say.

`.github/workflows/release.yml` runs the check before it builds, with `--tag`
on a tag; `tests/test_distribution.py` runs it on every `tools/check.py`.
Standard library only. See docs/distribution.md.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:-(alpha|beta|rc)\.(\d+))?")
PEP440_PRE = {"alpha": "a", "beta": "b", "rc": "rc"}

# JSON files and the fields in them that hold the workspace version.
JSON_FIELDS: dict[str, list[tuple[str | int, ...]]] = {
    ".claude-plugin/marketplace.json": [("metadata", "version"), ("plugins", 0, "version")],
    ".cursor-plugin/marketplace.json": [("metadata", "version")],
    "plugins/branchyard/.claude-plugin/plugin.json": [("version",)],
    "plugins/branchyard/.codex-plugin/plugin.json": [("version",)],
    "plugins/branchyard/.cursor-plugin/plugin.json": [("version",)],
    "plugins/branchyard/.github/plugin/plugin.json": [("version",)],
    "plugins/branchyard/harnesses/gemini-cli/gemini-extension.json": [("version",)],
    "plugins/branchyard/harnesses/opencode/package.json": [("version",)],
    "plugins/branchyard/harnesses/pi/package.json": [("version",)],
}
# TOML files: the table (None for the top level) whose `version` it is, and
# whether the value is spelled the PEP 440 way.
TOML_FIELDS: dict[str, tuple[str | None, bool]] = {
    "plugins/herdr/herdr-plugin.toml": (None, False),
    "sdk/python/pyproject.toml": ("project", True),
}


def parse(version: str) -> re.Match:
    """`version` as a release version, or ValueError naming the forms allowed."""
    match = VERSION.fullmatch(version)
    if not match:
        raise ValueError(f"{version!r} is not a release version: use X.Y.Z or X.Y.Z-alpha.N, -beta.N or -rc.N")
    return match


def pep440(version: str) -> str:
    """`version` as Python packaging spells it: `0.1.0-rc.1` is `0.1.0rc1`."""
    major, minor, patch, pre, number = parse(version).groups()
    return f"{major}.{minor}.{patch}" + (f"{PEP440_PRE[pre]}{number}" if pre else "")


def workspace_version(root: Path = ROOT) -> str:
    """`[workspace.package] version` in `root`'s Cargo.toml."""
    return tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def members(root: Path = ROOT) -> dict[str, Path]:
    """Each workspace crate's package name, with its Cargo.toml."""
    found = {}
    for member in tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["members"]:
        manifest = root / member / "Cargo.toml"
        found[tomllib.loads(manifest.read_text())["package"]["name"]] = manifest
    return found


def _dependency_tables(manifest: dict):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        yield manifest.get(key, {})
    for target in manifest.get("target", {}).values():
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            yield target.get(key, {})


def _field(data, path):
    for key in path:
        data = data[key]
    return data


def fields(root: Path = ROOT) -> list[tuple[str, str, str]]:
    """Every (file, field, value) that must equal the workspace version (in
    PEP 440's spelling for the Python SDK), less the workspace's own."""
    found = []
    crates = members(root)
    for _name, manifest in sorted(crates.items()):
        relative = manifest.relative_to(root).as_posix()
        data = tomllib.loads(manifest.read_text())
        package_version = data["package"].get("version")
        if package_version != {"workspace": True}:
            found.append((relative, "package.version", str(package_version)))
        for table in _dependency_tables(data):
            for dependency, spec in sorted(table.items()):
                if dependency in crates and isinstance(spec, dict) and "version" in spec:
                    found.append((relative, f"{dependency} version", spec["version"]))
    for package in tomllib.loads((root / "Cargo.lock").read_text())["package"]:
        if package["name"] in crates and "source" not in package:
            found.append(("Cargo.lock", package["name"], package["version"]))
    for relative, paths in JSON_FIELDS.items():
        data = json.loads((root / relative).read_text())
        for path in paths:
            found.append((relative, ".".join(map(str, path)), _field(data, path)))
    for relative, (table, _python) in TOML_FIELDS.items():
        data = tomllib.loads((root / relative).read_text())
        found.append(
            (relative, f"{table}.version" if table else "version", (data[table] if table else data)["version"])
        )
    return found


def disagreements(root: Path = ROOT) -> list[str]:
    """One line per field that is not the workspace version; empty when all agree."""
    version = workspace_version(root)
    try:
        python = pep440(version)
    except ValueError as error:
        return [f"Cargo.toml workspace.package.version: {error}"]
    problems = []
    for relative, field, value in fields(root):
        wanted = python if TOML_FIELDS.get(relative, (None, False))[1] else version
        if value != wanted:
            problems.append(f"{relative} {field} is {value!r}, not {wanted!r}")
    return problems


def check_tag(tag: str, version: str) -> None:
    """A release tag is `v` and the workspace version, exactly."""
    if tag != f"v{version}":
        raise ValueError(f"the tag {tag!r} does not name the workspace version: tag v{version}, or set the version")


def _replace_version_line(text: str, table: str | None, old: str, new: str, where: str) -> str:
    """`version = "old"` in `table` (the top level when None) as `new`."""
    header = re.compile(r"^\[[^\n]*\]$", re.M)
    start = 0
    if table is not None:
        match = re.search(rf"^\[{re.escape(table)}\]$", text, re.M)
        if not match:
            raise ValueError(f"{where} has no [{table}]")
        start = match.end()
    following = header.search(text, start)
    end = following.start() if following else len(text)
    section, count = re.subn(
        rf'^version = "{re.escape(old)}"$', f'version = "{new}"', text[start:end], count=1, flags=re.M
    )
    if count != 1:
        raise ValueError(f'{where}: no version = "{old}" in {f"[{table}]" if table else "its top level"}')
    return text[:start] + section + text[end:]


def set_version(new: str, root: Path = ROOT) -> list[str]:
    """Rewrite every field (and the workspace's own) to `new`; returns the files changed."""
    python = pep440(new)
    old = workspace_version(root)
    changed: dict[str, str] = {}

    def text(relative: str) -> str:
        return changed.get(relative) or (root / relative).read_text()

    changed["Cargo.toml"] = _replace_version_line(text("Cargo.toml"), "workspace.package", old, new, "Cargo.toml")
    crates = members(root)
    names = "|".join(re.escape(name) for name in crates)
    for manifest in crates.values():
        relative = manifest.relative_to(root).as_posix()
        updated = re.sub(rf'^((?:{names}) = \{{ version = ")[^"]*(")', rf"\g<1>{new}\g<2>", text(relative), flags=re.M)
        if updated != text(relative):
            changed[relative] = updated
    # Cargo.lock: the entries of workspace crates, which have no `source`.
    blocks = text("Cargo.lock").split("\n[[package]]\n")
    for index, block in enumerate(blocks):
        name = re.search(r'^name = "([^"]+)"$', block, re.M)
        if index and name and name.group(1) in crates and not re.search(r"^source = ", block, re.M):
            blocks[index] = re.sub(r'^version = "[^"]*"$', f'version = "{new}"', block, count=1, flags=re.M)
    changed["Cargo.lock"] = "\n[[package]]\n".join(blocks)
    for relative, paths in JSON_FIELDS.items():
        data = json.loads(text(relative))
        updated = text(relative)
        for value in {_field(data, path) for path in paths}:
            updated = re.sub(rf'("version":\s*"){re.escape(value)}(")', rf"\g<1>{new}\g<2>", updated)
        rewritten = json.loads(updated)
        if any(_field(rewritten, path) != new for path in paths):
            raise ValueError(f"{relative}: could not rewrite every version field")
        changed[relative] = updated
    for relative, (table, is_python) in TOML_FIELDS.items():
        data = tomllib.loads(text(relative))
        current = (data[table] if table else data)["version"]
        changed[relative] = _replace_version_line(
            text(relative), table, current, python if is_python else new, relative
        )
    written = []
    for relative, updated in sorted(changed.items()):
        if updated != (root / relative).read_text():
            (root / relative).write_text(updated)
            written.append(relative)
    return written


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tag", help="a release tag (v<version>) that must name the workspace version")
    parser.add_argument("--set", dest="new", metavar="VERSION", help="rewrite every version field to VERSION")
    parser.add_argument("--pep440", action="store_true", help="print the version as Python packaging spells it")
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    try:
        if args.new:
            for relative in set_version(args.new.removeprefix("v"), args.root):
                print(f"rewrote {relative}", file=sys.stderr)
        version = workspace_version(args.root)
        problems = disagreements(args.root)
        if problems:
            print(f"release_version.py: these do not carry the workspace version {version}:", file=sys.stderr)
            for problem in problems:
                print(f"  {problem}", file=sys.stderr)
            print(f"next: python3 tools/release_version.py --set {version}", file=sys.stderr)
            return 1
        if args.tag is not None:
            check_tag(args.tag, version)
    except ValueError as error:
        print(f"release_version.py: {error}", file=sys.stderr)
        return 1
    print(pep440(version) if args.pep440 else version)
    return 0


if __name__ == "__main__":
    sys.exit(main())
