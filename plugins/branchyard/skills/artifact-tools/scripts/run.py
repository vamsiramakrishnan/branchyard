#!/usr/bin/env python3
"""Run installed document tools; setup explicitly installs the pinned Python runtime."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import stat
import subprocess
import sys
import venv
from pathlib import Path

HERE = Path(__file__).resolve().parent


def receipt() -> dict:
    """Bind readiness to the installed dependency lock and Python compatibility."""
    return {
        "schema_version": 1,
        "requirements_sha256": hashlib.sha256((HERE / "requirements.lock").read_bytes()).hexdigest(),
        "python_version": list(sys.version_info[:2]),
    }


def runtime() -> Path:
    """Keep incompatible Python versions and dependency sets in separate environments."""
    digest = receipt()["requirements_sha256"][:16]
    version = f"{sys.version_info.major}.{sys.version_info.minor}"
    return Path.home().resolve() / ".cache/branchyard/artifacts" / f"py{version}-{digest}"


def plain_directory(path: Path) -> None:
    """Check the runtime cache boundary without following links inside it."""
    for directory in (path, *path.parents):
        if directory.is_symlink() or (directory.exists() and not directory.is_dir()):
            raise ValueError(f"runtime cache {directory} is not a plain directory; refusing symbolic links")


def ready(target: Path) -> bool:
    """Select only a complete runtime with the current structured receipt."""
    plain_directory(target)
    plain_directory(target / "bin")
    marker = target / ".ready"
    if marker.is_symlink():
        raise ValueError(f"runtime receipt {marker} is a symbolic link")
    if not marker.exists():
        return False
    metadata = marker.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 4096:
        raise ValueError(f"runtime receipt {marker} is not a bounded regular file")
    try:
        installed = json.loads(marker.read_text())
    except (ValueError, UnicodeError):
        return False
    return installed == receipt() and (target / "bin/python").is_file()


def setup(target: Path) -> int:
    """Reuse a ready runtime; repair partial installs with hash-pinned pip."""
    plain_directory(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    lock_path = target.parent / (target.name + ".lock")
    if lock_path.is_symlink():
        raise ValueError(f"runtime lock {lock_path} is a symbolic link")
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "a") as lock:
        if not stat.S_ISREG(os.fstat(lock.fileno()).st_mode):
            raise ValueError(f"runtime lock {lock_path} is not a regular file")
        fcntl.flock(lock, fcntl.LOCK_EX)
        python = target / "bin/python"
        if not ready(target):
            (target / ".ready").unlink(missing_ok=True)
            venv.EnvBuilder(with_pip=True, clear=True).create(target)
            result = subprocess.run(
                [
                    str(python),
                    "-m",
                    "pip",
                    "install",
                    "--disable-pip-version-check",
                    "--require-hashes",
                    "-r",
                    str(HERE / "requirements.lock"),
                ],
                stdout=sys.stderr,
                check=False,
            )
            if result.returncode:
                return result.returncode
            checked = subprocess.run([str(python), "-m", "pip", "check"], stdout=sys.stderr, check=False)
            if checked.returncode:
                return checked.returncode
            (target / ".ready").write_text(json.dumps(receipt(), sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                "status": "passed",
                "python": str(python),
                "next": "Run doctor. Native rendering and pivots also need LibreOffice, poppler, fonts "
                "and python3-uno; use the artifact runtime image.",
            }
        )
    )
    return 0


def main() -> int:
    """Bootstrap only on setup; ordinary operations never access the package index."""
    target = runtime()
    if sys.argv[1:] == ["setup"]:
        return setup(target)
    python = target / "bin/python"
    if ready(target) and Path(sys.prefix).resolve() != target.resolve():
        os.execv(str(python), [str(python), str(HERE / "run.py"), *sys.argv[1:]])
    sys.path.insert(0, str(HERE))
    from branchyard_artifacts.__main__ import main as toolkit_main

    return toolkit_main()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(
            json.dumps(
                {
                    "status": "failed",
                    "kind": "runtime",
                    "message": str(error),
                    "next": "Check python3-venv and package-index access, then rerun setup.",
                }
            )
        )
        raise SystemExit(1) from None
