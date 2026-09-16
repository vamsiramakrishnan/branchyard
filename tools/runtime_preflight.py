#!/usr/bin/env python3
"""Record whether a Linux server can begin Microsandbox qualification. Never launches a VM."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys


def inspect():
    checks = {"linux": platform.system() == "Linux", "kvm_device": Path("/dev/kvm").exists()}
    checks["kvm_access"] = False
    if checks["kvm_device"]:
        try:
            fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
            os.close(fd)
            checks["kvm_access"] = True
        except OSError:
            pass
    binary = shutil.which("msb")
    version = None
    if binary:
        try:
            result = subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=5, check=True)
            version = result.stdout.strip()[:256]
        except (OSError, subprocess.SubprocessError):
            pass
    checks["runtime_installed"] = version is not None
    checks["pinned_runtime"] = version is not None and "0.7.0" in version.split()
    return {"schema": "branchyard/runtime-preflight/v1", "provider": "microsandbox", "required_runtime": "0.7.0", "system": platform.system(), "architecture": platform.machine(), "kernel": platform.release(), "checks": checks, "runtime_version": version, "ready_for_qualification": all(checks.values()), "qualified": False, "reason": "Preflight only. Lifecycle, streams, resource limits, networking, filesystem isolation and cleanup still need a live qualification run."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = inspect()
    text = json.dumps(result, indent=2) + "\n"
    if args.output:
        # Exclusive creation preserves previous evidence instead of overwriting it.
        with args.output.open("x") as output:
            output.write(text)
    print(text, end="")
    return 0 if result["ready_for_qualification"] else 2


if __name__ == "__main__":
    sys.exit(main())
