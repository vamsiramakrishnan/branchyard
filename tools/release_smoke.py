#!/usr/bin/env python3
"""Install actual release artifacts in a clean home and exercise model-free setup."""

import argparse
import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path


def run_smoke(artifacts: Path, runtime: bool) -> dict:
    """Check install, repeat install, setup and checksum rejection on this host."""
    with tempfile.TemporaryDirectory(prefix="branchyard-release-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        agy = fake_bin / "agy"
        agy.write_text("#!/bin/sh\nprintf '%s\\n' '1.4.9'\n")
        agy.chmod(0o755)
        env = dict(
            os.environ,
            HOME=str(home),
            XDG_CONFIG_HOME=str(home / ".config"),
            PATH=f"{fake_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
            GEMINI_API_KEY="distribution-test-fixture",
            BRANCHYARD_USER_CONFIG=str(home / "user.toml"),
        )
        prefix = root / "prefix with spaces"
        installer = artifacts / "install.sh"
        args = ["sh", str(installer), "--base-url", artifacts.as_uri(), "--prefix", str(prefix), "--no-setup"]
        if runtime:
            args.append("--runtime")
        for _attempt in range(2):
            subprocess.run(args, env=env, check=True, timeout=180)
        env["PATH"] = f"{prefix / 'bin'}:{env['PATH']}"
        for name in ["by", "branchyard-server", "branchyard-bridge"]:
            subprocess.run([str(prefix / "bin" / name), "--version"], env=env, check=True, timeout=30)
        repo = root / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], env=env, check=True)
        (repo / "README.md").write_text("Distribution smoke fixture\n")
        subprocess.run(["git", "add", "README.md"], cwd=repo, env=env, check=True)
        subprocess.run(
            [
                "git",
                "-c",
                "user.name=Distribution smoke",
                "-c",
                "user.email=distribution@example.invalid",
                "commit",
                "-qm",
                "Create fixture",
            ],
            cwd=repo,
            env=env,
            check=True,
        )
        by = str(prefix / "bin/by")
        subprocess.run(
            [by, "setup", "--yes", "--harness", "antigravity", "--up"], cwd=repo, env=env, check=True, timeout=120
        )
        subprocess.run([by, "up", "--plan", "--json"], cwd=repo, env=env, check=True, timeout=60)
        before = (repo / "branchyard.toml").read_bytes()
        subprocess.run(
            [by, "setup", "--yes", "--harness", "antigravity", "--up"], cwd=repo, env=env, check=True, timeout=120
        )
        if (repo / "branchyard.toml").read_bytes() != before:
            raise RuntimeError("repeating setup changed the project configuration")
        # Mutate the archive for the current host; the embedded dist checksum must refuse it.
        bad = root / "tampered"
        bad.mkdir()
        for path in artifacts.iterdir():
            if path.is_file():
                (bad / path.name).symlink_to(path)
        machine = {"arm64": "aarch64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
        target = machine + (
            "-apple-darwin"
            if platform.system() == "Darwin"
            else "-unknown-linux-gnu"
            if runtime
            else "-unknown-linux-musl"
        )
        package = "branchyard-runtime" if runtime else "branchyard-cli"
        archive = bad / f"{package}-{target}.tar.gz"
        source = archive.resolve()
        archive.unlink()
        shutil.copyfile(source, archive)
        with archive.open("ab") as stream:
            stream.write(b"tampered")
        refused_prefix = root / "refused"
        rejected = subprocess.run(
            [
                "sh",
                str(installer),
                "--base-url",
                bad.as_uri(),
                "--prefix",
                str(refused_prefix),
                "--no-setup",
                *(["--runtime"] if runtime else []),
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=180,
        )
        if rejected.returncode == 0 or (refused_prefix / "bin/by").exists():
            raise RuntimeError("the packaged installer accepted a tampered archive")
        return {
            "runtime": runtime,
            "installation": "passed",
            "repeat": "passed",
            "setup": "passed",
            "checksum_rejection": "passed",
        }


def main() -> None:
    """Parse the artifact directory and run the native package smoke."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--runtime", choices=["true", "false"], default="false")
    args = parser.parse_args()
    print(json.dumps(run_smoke(args.artifacts.resolve(), args.runtime == "true")))


if __name__ == "__main__":
    main()
