#!/usr/bin/env python3
"""Validate the vendored Herdr detection data; never treat it as authorization."""
import json
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    seen = set()
    entries = []
    for path in sorted((ROOT / "vendor/herdr/src/detect/manifests").glob("*.toml")):
        manifest = tomllib.loads(path.read_text())
        agent = manifest["id"]
        if agent in seen:
            raise ValueError(f"duplicate harness ID: {agent}")
        seen.add(agent)
        rules = manifest.get("rules", [])
        ids = [r["id"] for r in rules]
        if len(ids) != len(set(ids)):
            raise ValueError(f"duplicate rule ID: {agent}")
        entries.append({"id": agent, "version": manifest["version"],
                        "rules": len(rules), "source": str(path.relative_to(ROOT))})
    print(json.dumps({"kind": "terminal-observation-catalog", "agents": entries}, indent=2))


if __name__ == "__main__":
    main()
