#!/usr/bin/env python3
"""Fetch the checksum-pinned standalone agy binary for the release actor image."""

import argparse
import hashlib
import json
import shutil
import tarfile
import tempfile
import urllib.request
from pathlib import Path


def fetch(manifest: Path, destination: Path) -> None:
    """Verify the complete archive before extracting its standalone executable."""
    release = json.loads(manifest.read_text())
    if not release["url"].startswith("https://storage.googleapis.com/antigravity-public/"):
        raise ValueError("agy payload must use the official HTTPS distribution")
    with tempfile.TemporaryDirectory(prefix="branchyard-agy-") as temporary:
        archive_path = Path(temporary) / "agy.tar.gz"
        urllib.request.urlretrieve(release["url"], archive_path)
        if hashlib.sha512(archive_path.read_bytes()).hexdigest() != release["sha512"]:
            raise ValueError("agy archive checksum mismatch; no binary installed")
        with tarfile.open(archive_path) as archive:
            candidates = [
                member
                for member in archive.getmembers()
                if member.isfile() and Path(member.name).name in {"antigravity", "agy"}
            ]
            if len(candidates) != 1:
                raise ValueError("agy release must contain exactly one standalone executable")
            source = archive.extractfile(candidates[0])
            if source is None:
                raise ValueError("agy release has no executable data")
            with source, destination.open("wb") as output:
                shutil.copyfileobj(source, output)
        destination.chmod(0o755)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path("deploy/agy-release.json"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    fetch(args.manifest, args.output)


if __name__ == "__main__":
    main()
