#!/usr/bin/env python3
"""Install a canonical Branchyard skill and its dependencies into a host's skill directory.

The packaged capability catalog defines atomic-skill dependencies. Setup and
delegation remain available independently. Preview is the default; --apply
writes only after every source and destination in the closure passes preflight.
"""

import argparse
import json
import sys
import tempfile
from pathlib import Path


class InstallError(ValueError):
    """An installation refusal with the existing JSON error classification."""

    def __init__(self, kind: str, message: str):
        super().__init__(message)
        self.kind = kind


def _catalog(plugin: Path) -> dict[str, list[str]]:
    """Read the packaged graph and validate IDs, references and cycles."""
    graph: dict[str, list[str]] = {"delegate": [], "setup": []}
    catalog = plugin / "capabilities.json"
    if catalog.is_symlink():
        raise ValueError("capabilities.json must not be a symlink")
    data = json.loads(catalog.read_text())
    if not isinstance(data, dict) or data.get("schema_version") != 1 or not isinstance(data.get("skills"), list):
        raise ValueError("capabilities.json must contain schema_version 1 and a skills list")
    for entry in data["skills"]:
        if not isinstance(entry, dict):
            raise ValueError("capability entries must be objects")
        name = entry.get("id")
        dependencies = entry.get("requires", [])
        if (
            not isinstance(name, str)
            or not name
            or name.startswith("-")
            or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789-" for c in name)
            or name in graph
            or not isinstance(dependencies, list)
            or any(not isinstance(child, str) for child in dependencies)
        ):
            raise ValueError("capabilities.json has an invalid or repeated skill ID or dependency list")
        graph[name] = dependencies
    for name, dependencies in graph.items():
        if any(child not in graph for child in dependencies):
            raise ValueError(f"unknown dependency for {name}")
    for name in graph:
        _closure(name, graph)
    return graph


def _closure(name: str, graph: dict[str, list[str]]) -> list[str]:
    """Return each dependency before its consumer, exactly once."""
    ordered = []
    visiting = set()

    def visit(skill: str) -> None:
        if skill in visiting:
            raise ValueError(f"cyclic skill dependency at {skill}")
        if skill in ordered:
            return
        visiting.add(skill)
        for dependency in graph[skill]:
            visit(dependency)
        visiting.remove(skill)
        ordered.append(skill)

    visit(name)
    return ordered


def _files(directory: Path) -> dict[str, bytes]:
    """Snapshot regular files, ignoring Python caches and refusing links/devices."""
    if any(path.is_symlink() for path in (directory, *directory.parents)) or not directory.is_dir():
        raise ValueError(f"{directory} is not a plain skill directory")
    files = {}
    for path in sorted(directory.rglob("*")):
        relative = path.relative_to(directory)
        if "__pycache__" in relative.parts or relative.suffix == ".pyc":
            continue
        if path.is_symlink():
            raise ValueError(f"skill files must not be symlinks: {path}")
        if path.is_file():
            files[relative.as_posix()] = path.read_bytes()
        elif not path.is_dir():
            raise ValueError(f"skill files must be regular files: {path}")
    return files


def _plain_ancestors(directory: Path) -> None:
    """Refuse existing links and non-directory ancestors before writing anything."""
    for path in (directory, *directory.parents):
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise InstallError("destination_exists", f"{path} is not a plain directory; refusing to replace it.")


def _plan(plugin: Path, destination: Path, names: list[str], force: bool) -> list[dict]:
    """Preflight the full dependency closure and capture the exact bytes to install."""
    _plain_ancestors(destination)
    entries = []
    for name in names:
        source = plugin / "skills" / name
        try:
            files = _files(source)
            if "SKILL.md" not in files:
                raise ValueError(f"{source} has no SKILL.md")
        except (OSError, ValueError) as error:
            raise InstallError("invalid_source", str(error)) from error
        target = destination / name
        _plain_ancestors(target)
        try:
            unchanged = target.exists() and _files(target) == files
        except ValueError as error:
            raise InstallError("destination_exists", str(error)) from error
        replacing = target.exists() and not unchanged
        if replacing and not force:
            raise InstallError(
                "destination_exists",
                f"{target} already holds a different skill install; pass --force to replace it.",
            )
        entries.append(
            {
                "name": name,
                "target": target,
                "files": files,
                "modes": {relative: (source / relative).stat().st_mode & 0o777 for relative in files},
                "unchanged": unchanged,
                "replacing": replacing,
            }
        )
    return entries


def _apply(destination: Path, entries: list[dict]) -> None:
    """Stage all payloads before replacing any install; keep forced originals until rename succeeds."""
    changed = [entry for entry in entries if not entry["unchanged"]]
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".branchyard-install-", dir=destination) as temporary:
        stage = Path(temporary)
        for entry in changed:
            directory = stage / entry["name"]
            directory.mkdir()
            for relative, data in entry["files"].items():
                path = directory / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                path.chmod(entry["modes"][relative])
        for entry in changed:
            target = entry["target"]
            _plain_ancestors(target)
            previous = stage / (".previous-" + entry["name"])
            if entry["replacing"]:
                target.rename(previous)
            elif target.exists():
                raise InstallError("destination_exists", f"{target} appeared after preflight; refusing to replace it.")
            try:
                (stage / entry["name"]).rename(target)
            except OSError:
                if previous.exists():
                    previous.rename(target)
                raise


def main() -> int:
    """Preview or apply one canonical skill's complete dependency closure."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True, help="Host's skill directory")
    parser.add_argument("--skill", default="delegate", help="Skill ID from capabilities.json, setup or delegate")
    parser.add_argument("--apply", action="store_true", help="Copy; otherwise print a preview")
    parser.add_argument(
        "--force",
        action="store_true",
        help="Replace differing skill directories in the dependency closure",
    )
    args = parser.parse_args()
    plugin = Path(__file__).resolve().parents[1]
    try:
        graph = _catalog(plugin)
        if args.skill not in graph:
            parser.error(f"unknown skill {args.skill!r}; choose one of {', '.join(sorted(graph))}")
        names = _closure(args.skill, graph)
        destination = args.destination.expanduser().absolute()
        _plain_ancestors(destination)
        destination = destination.resolve()
        entries = _plan(plugin, destination, names, args.force)
        selected = entries[-1]
        unchanged = all(entry["unchanged"] for entry in entries)
        result = {"applied": args.apply and not unchanged, "destination": str(selected["target"])}
        if unchanged:
            result["unchanged"] = True
        else:
            result["replaced"] = selected["replacing"]
        if len(entries) > 1:
            result["skills"] = names
            result["installations"] = [
                {
                    "skill": entry["name"],
                    "destination": str(entry["target"]),
                    "action": "unchanged" if entry["unchanged"] else "replace" if entry["replacing"] else "install",
                }
                for entry in entries
            ]
        if args.apply and not unchanged:
            _apply(destination, entries)
    except (OSError, ValueError) as error:
        print(json.dumps({"error": getattr(error, "kind", "invalid_source"), "message": str(error)}), file=sys.stderr)
        return 2
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
