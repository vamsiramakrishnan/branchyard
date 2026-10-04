#!/usr/bin/env python3
"""Install a pinned release binary of a CI cargo tool into a directory on PATH.

    python3 tools/install_ci_tool.py cargo-deny [--dest ~/.cargo/bin]

The release asset's SHA-256 is pinned here, so a replaced asset fails the
job rather than running. These are separate downloads (not `cargo install`)
to keep CI fast, and they are made only by the jobs that need them, never by
the offline build-and-test job. To bump a tool, change its version, URL and
digest together (`sha256sum` of the downloaded asset) and say why.
"""

import argparse
import hashlib
import io
import os
import stat
import sys
import tarfile
import urllib.request
from pathlib import Path

RELEASES = "https://github.com"

# name -> (asset URL, sha256 of the asset, path of the binary inside the tarball)
TOOLS = {
    "cargo-deny": (
        f"{RELEASES}/EmbarkStudios/cargo-deny/releases/download/0.18.9/cargo-deny-0.18.9-x86_64-unknown-linux-musl.tar.gz",
        "491d04e4c05d7c92582e3d40ec94126c52472a546326a6d29473a5a4e73babd2",
        "cargo-deny-0.18.9-x86_64-unknown-linux-musl/cargo-deny",
    ),
    "cargo-machete": (
        f"{RELEASES}/bnjbvr/cargo-machete/releases/download/v0.9.2/cargo-machete-v0.9.2-x86_64-unknown-linux-musl.tar.gz",
        "48200087f54c55aabcd4db4af1e25742b49846c02a1b1bfa134711945b35b2e9",
        "cargo-machete-v0.9.2-x86_64-unknown-linux-musl/cargo-machete",
    ),
    "cargo-llvm-cov": (
        f"{RELEASES}/taiki-e/cargo-llvm-cov/releases/download/v0.9.1/cargo-llvm-cov-x86_64-unknown-linux-gnu.tar.gz",
        "b3f68e625481fed9b16444174f3fa5ebcdbde4a1878803a35eabe2dcefcdc41a",
        "cargo-llvm-cov",
    ),
}


def extract(archive: bytes, member: str) -> bytes:
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        handle = tar.extractfile(member)
        if handle is None:
            raise SystemExit(f"{member} is not a file in the archive")
        return handle.read()


def install(name: str, dest: Path, fetch=None) -> Path:
    url, digest, member = TOOLS[name]
    fetch = fetch or (lambda u: urllib.request.urlopen(u, timeout=60).read())
    archive = fetch(url)
    actual = hashlib.sha256(archive).hexdigest()
    if actual != digest:
        raise SystemExit(f"{name}: {url} has sha256 {actual}, expected {digest}; refusing to install it")
    dest.mkdir(parents=True, exist_ok=True)
    target = dest / Path(member).name
    target.write_bytes(extract(archive, member))
    target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return target


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("tool", choices=sorted(TOOLS))
    parser.add_argument("--dest", type=Path, default=Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "bin")
    args = parser.parse_args(argv[1:])
    print(f"installed {install(args.tool, args.dest)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
