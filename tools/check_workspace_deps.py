#!/usr/bin/env python3
"""Every third-party dependency used by more than one workspace member is
declared once in the root [workspace.dependencies] and inherited with
`workspace = true`. Versions spelled out in several members drift (serde "1"
next to "1.0.229", sha2 "0.11" next to "0.11.0"); a version in one place
cannot. Members may still add their own `features` and `optional`.

Run: python3 tools/check_workspace_deps.py [root]
"""

import sys
import tomllib
from pathlib import Path

DEP_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def member_tables(manifest):
    """Yield (table name, {dep: spec}) for plain and target-specific tables."""
    for name in DEP_TABLES:
        if name in manifest:
            yield name, manifest[name]
    for target, body in manifest.get("target", {}).items():
        for name in DEP_TABLES:
            if name in body:
                yield f'target."{target}".{name}', body[name]


def check(root):
    root = Path(root)
    ws = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    hoisted = set(ws.get("dependencies", {}))
    pinned = {}  # dep -> members that spell a version themselves
    errors = []
    for path in sorted(root.glob("crates/*/Cargo.toml")):
        member = path.parent.name
        for table, deps in member_tables(tomllib.loads(path.read_text())):
            for name, spec in deps.items():
                if isinstance(spec, dict) and "path" in spec:
                    continue  # first-party crates are wired by path
                if isinstance(spec, dict) and spec.get("workspace") is True:
                    if name not in hoisted:
                        errors.append(f"{member}: {name} inherits a workspace dependency that is not declared")
                    continue
                pinned.setdefault(name, []).append((member, table))
    for name, users in sorted(pinned.items()):
        members = sorted({m for m, _ in users})
        if name in hoisted:
            errors.append(
                f"{name}: declared in [workspace.dependencies] but {', '.join(members)} "
                "still spell a version; use workspace = true"
            )
        elif len(members) > 1:
            errors.append(
                f"{name}: versioned separately in {', '.join(members)}; "
                "move it to [workspace.dependencies] and use workspace = true"
            )
    return errors


def main(argv):
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parents[1]
    errors = check(root)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if not errors:
        print("workspace dependencies are declared once")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
