#!/usr/bin/env python3
"""Report whether a host is ready to run `by serve` (or `by` locally) and,
optionally, its Microsandbox provider and PostgreSQL store. Never starts a
sandbox, a server or a database migration; see docs/deploy.md.

Always checked, and required (nonzero exit if missing): `git`, since every
served repository is a git work tree.

Checked and reported, but not required unless asked for:

- `--require-sandbox`: `/dev/kvm` exists and is openable, and the host uses
  cgroup v2 (`/sys/fs/cgroup/cgroup.controllers` exists) — what the
  Microsandbox provider needs (`--allow-provider microsandbox`,
  `docs/providers.md`). This does not launch or qualify a microVM.
- `--postgres-url URL`: the host and port from a `postgres://` URL accept a
  TCP connection (a reachability probe, not authentication or schema
  checks) — what `by serve --database URL` needs.
- Harness executables: which of the profiles in
  `crates/branchyard-harness/src/profiles.rs` have their command on `PATH`.
  Always informational: a served repository only needs the harnesses its
  requests actually name.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
from urllib.parse import urlsplit

# Kept in sync by hand with crates/branchyard-harness/src/profiles.rs's
# PROFILES table; nothing here reads Rust source.
HARNESS_COMMANDS = {
    "claude-code": "claude",
    "codex": "codex",
    "antigravity": "agy",
    "oh-my-pi": "omp",
    "deepseek-harness": "dsh",
    "gemini-cli": "gemini",
    "opencode": "opencode",
    "pi": "pi",
    "goose": "goose",
    "cursor": "agent",
    "github-copilot": "copilot",
    "amp": "amp",
    "qwen-code": "qwen",
    "kimi-cli": "kimi",
    "hermes": "hermes",
}


def check_git():
    path = shutil.which("git")
    version = None
    if path:
        try:
            result = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=5, check=True)
            version = result.stdout.strip()[:128]
        except (OSError, subprocess.SubprocessError):
            pass
    return {"found": path is not None, "path": path, "version": version}


def check_kvm():
    device = Path("/dev/kvm")
    exists = device.exists()
    accessible = False
    if exists:
        try:
            fd = os.open(str(device), os.O_RDWR | os.O_CLOEXEC)
            os.close(fd)
            accessible = True
        except OSError:
            pass
    return {"device_exists": exists, "accessible": accessible}


def check_cgroup_v2():
    return {"unified": Path("/sys/fs/cgroup/cgroup.controllers").exists()}


def check_harnesses():
    return {harness: shutil.which(command) is not None for harness, command in sorted(HARNESS_COMMANDS.items())}


def check_postgres(url: str, timeout: float = 3.0):
    parsed = urlsplit(url)
    if parsed.scheme not in ("postgres", "postgresql"):
        return {"url": url, "reachable": False, "error": f"not a postgres:// URL: {parsed.scheme!r}"}
    host = parsed.hostname or "localhost"
    port = parsed.port or 5432
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return {"host": host, "port": port, "reachable": True}
    except OSError as error:
        return {"host": host, "port": port, "reachable": False, "error": str(error)}


def inspect(require_sandbox: bool, postgres_url):
    checks = {
        "git": check_git(),
        "kvm": check_kvm(),
        "cgroup_v2": check_cgroup_v2(),
        "harnesses": check_harnesses(),
    }
    required_failures = []
    if not checks["git"]["found"]:
        required_failures.append("git is not on PATH")
    if require_sandbox:
        if not (checks["kvm"]["device_exists"] and checks["kvm"]["accessible"]):
            required_failures.append("/dev/kvm is missing or not accessible (--require-sandbox)")
        if not checks["cgroup_v2"]["unified"]:
            required_failures.append("cgroup v2 (the unified hierarchy) is not mounted (--require-sandbox)")
    if postgres_url:
        checks["postgres"] = check_postgres(postgres_url)
        if not checks["postgres"]["reachable"]:
            required_failures.append(f"PostgreSQL at {postgres_url!r} is not reachable")
    return {
        "schema": "branchyard/runtime-preflight/v1",
        "system": platform.system(),
        "architecture": platform.machine(),
        "kernel": platform.release(),
        "checks": checks,
        "required_failures": required_failures,
        "ready": not required_failures,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--require-sandbox",
        action="store_true",
        help="Fail if KVM or cgroup v2 is missing (needed for --allow-provider microsandbox)",
    )
    parser.add_argument(
        "--postgres-url",
        help="Fail if this postgres:// URL's host and port do not accept a TCP connection",
    )
    parser.add_argument("--output", type=Path, help="Also write the report here (refuses to overwrite)")
    args = parser.parse_args()
    result = inspect(args.require_sandbox, args.postgres_url)
    text = json.dumps(result, indent=2) + "\n"
    if args.output:
        # Exclusive creation preserves previous evidence instead of overwriting it.
        with args.output.open("x") as output:
            output.write(text)
    print(text, end="")
    return 0 if result["ready"] else 2


if __name__ == "__main__":
    sys.exit(main())
