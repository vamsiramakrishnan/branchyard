#!/usr/bin/env python3
"""Prepare the bootstrap, checksum manifest and Homebrew formula for a release."""

import argparse
import hashlib
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def build_assets(artifacts: Path, output: Path) -> list[Path]:
    """Assemble entrypoint assets from the actual archives produced by dist."""
    output.mkdir(parents=True, exist_ok=True)
    bootstrap = output / "install.sh"
    shutil.copyfile(ROOT / "install.sh", bootstrap)
    inputs = {
        path.name: path
        for path in artifacts.iterdir()
        if path.is_file() and path.name.endswith((".tar.gz", "-installer.sh", ".json", ".zip", ".whl"))
    }
    inputs[bootstrap.name] = bootstrap
    sums = output / "SHA256SUMS"
    records = []
    for name, path in sorted(inputs.items()):
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        records.append(f"{digest}  {name}\n")
    sums.write_text("".join(records))
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    formula = output / "branchyard.rb"
    with formula.open("w") as stream:
        subprocess.run(
            [sys.executable, str(ROOT / "tools/homebrew_formula.py"), "--version", version, "--sums", str(sums)],
            stdout=stream,
            check=True,
        )
    python_version = tomllib.loads((ROOT / "sdk/python/pyproject.toml").read_text())["project"]["version"]
    sdk_assets = []
    for name, source in inputs.items():
        if name.endswith((".zip", ".whl")) or name == f"branchyard-{python_version}.tar.gz":
            destination = output / name
            shutil.copyfile(source, destination)
            sdk_assets.append(destination)
    return [bootstrap, sums, formula, *sdk_assets]


def main() -> None:
    """Write extra release assets after the platform archives are available."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for path in build_assets(args.artifacts, args.output):
        print(path)


if __name__ == "__main__":
    main()
