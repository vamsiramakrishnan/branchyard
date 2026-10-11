#!/usr/bin/env python3
"""Pin upstream files into vendor/ and record them in vendor.lock.json.

`vendor/` is a pinned reference snapshot (docs/vendoring.md): every file
under it is a byte-for-byte copy of one upstream file at one commit, and
`vendor.lock.json` records the path, the upstream repository, the commit,
the upstream path, the Git blob ID and SHA-256 of the file as fetched, its
license and its use. This tool writes both, so a new upstream or a re-pin
is one command and the record cannot drift from the bytes:

    # Pin files of a new upstream, reading blobs from a local clone that
    # holds the commit (a blobless clone fetches each blob on demand).
    python3 tools/vendor_pin.py --dir vendor/httpjail \\
        --upstream https://github.com/coder/httpjail --commit <40 hex> \\
        --license CC0-1.0 --use reference-only --git /path/to/clone \\
        LICENSE README.md src/rules.rs

    # Re-pin every file already recorded under a directory at a new commit,
    # keeping each file's license and use; changed blobs are reported.
    python3 tools/vendor_pin.py --dir vendor/openrig --commit <40 hex> \\
        --git /path/to/clone --repin

Without `--git`, files are fetched from raw.githubusercontent.com at the
commit (the upstream must be a github.com repository). The vendored path is
always `<dir>/<upstream path>`. A local patch is never written here: a pin
describes upstream (`"modified": false`); see vendor.patches.json.

`tools/verify_vendor.py` checks the result; run it after this.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COMMIT = re.compile(r"^[0-9a-f]{40}$")
FIELDS = ("path", "upstream", "commit", "upstream_path", "git_blob", "sha256", "license", "use", "modified")


def digests(data: bytes) -> tuple[str, str]:
    """`(sha256, git blob ID)` of `data`, as the lock records them."""
    sha256 = hashlib.sha256(data).hexdigest()
    blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
    return sha256, blob


class GitSource:
    """Blobs read from a clone that holds the commit: exact, and offline
    once fetched."""

    def __init__(self, clone: Path):
        self.clone = Path(clone)

    def read(self, commit: str, path: str) -> bytes:
        done = subprocess.run(
            ["git", "-C", str(self.clone), "show", f"{commit}:{path}"],
            capture_output=True,
            check=False,
        )
        if done.returncode != 0:
            raise LookupError(f"{path} at {commit[:12]}: {done.stderr.decode(errors='replace').strip()}")
        return done.stdout

    def list(self, commit: str, prefix: str = "") -> list[str]:
        """Every file under `prefix` at `commit`."""
        args = ["git", "-C", str(self.clone), "ls-tree", "-r", "--name-only", commit]
        if prefix:
            args += ["--", prefix]
        done = subprocess.run(args, capture_output=True, check=False)
        if done.returncode != 0:
            raise LookupError(done.stderr.decode(errors="replace").strip())
        return [line for line in done.stdout.decode().splitlines() if line]


class RawSource:
    """Blobs fetched from raw.githubusercontent.com at the commit."""

    def __init__(self, upstream: str):
        parts = urllib.parse.urlsplit(upstream)
        segments = [s for s in parts.path.split("/") if s]
        if parts.netloc != "github.com" or len(segments) < 2:
            raise ValueError(f"{upstream}: without --git, the upstream must be https://github.com/OWNER/REPO")
        self.owner, self.repo = segments[0], segments[1].removesuffix(".git")

    def url(self, commit: str, path: str) -> str:
        return f"https://raw.githubusercontent.com/{self.owner}/{self.repo}/{commit}/{urllib.parse.quote(path)}"

    def read(self, commit: str, path: str) -> bytes:
        request = urllib.request.Request(self.url(commit, path), headers={"User-Agent": "branchyard-vendor-pin"})
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except OSError as error:
            raise LookupError(f"{path} at {commit[:12]}: {error}") from None


@dataclass
class Report:
    added: list[str] = field(default_factory=list)
    changed: list[str] = field(default_factory=list)
    unchanged: list[str] = field(default_factory=list)

    def summary(self) -> str:
        return f"{len(self.added)} added, {len(self.changed)} changed, {len(self.unchanged)} unchanged"


def load_lock(root: Path) -> dict:
    path = root / "vendor.lock.json"
    lock = json.loads(path.read_text()) if path.exists() else {"format": 1, "files": []}
    if lock.get("format") != 1:
        raise ValueError("vendor.lock.json: format must be 1")
    lock.setdefault("files", [])
    return lock


def save_lock(root: Path, lock: dict) -> None:
    lock["files"].sort(key=lambda e: e["path"])
    (root / "vendor.lock.json").write_text(json.dumps(lock, indent=2) + "\n")


def _check_commit(commit: str) -> None:
    if not COMMIT.match(commit):
        raise ValueError(f"commit {commit!r} must be a full 40-character lowercase hex id")


def _check_dir(directory: str) -> str:
    directory = directory.strip("/")
    if not directory.startswith("vendor/") or directory.count("/") != 1:
        raise ValueError(f"--dir {directory!r} must be vendor/<name>")
    return directory


def _write(root: Path, lock: dict, entry: dict, data: bytes, report: Report) -> None:
    dest = root / entry["path"]
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(data)
    existing = next((e for e in lock["files"] if e["path"] == entry["path"]), None)
    if existing is None:
        lock["files"].append(entry)
        report.added.append(entry["path"])
    else:
        if existing["git_blob"] != entry["git_blob"]:
            report.changed.append(entry["path"])
        else:
            report.unchanged.append(entry["path"])
        existing.clear()
        existing.update(entry)


def pin(
    root: Path, directory: str, upstream: str, commit: str, paths: list[str], license: str, use: str, source
) -> Report:
    """Write `paths` of `upstream` at `commit` under `directory` and record
    them. A path already pinned there is replaced, and reported as changed
    when its blob differs."""
    _check_commit(commit)
    directory = _check_dir(directory)
    if not paths:
        raise ValueError("give at least one upstream path")
    lock = load_lock(root)
    report = Report()
    for upstream_path in paths:
        upstream_path = upstream_path.strip("/")
        if not upstream_path or ".." in upstream_path.split("/"):
            raise ValueError(f"bad upstream path {upstream_path!r}")
        data = source.read(commit, upstream_path)
        sha256, blob = digests(data)
        entry = {
            "path": f"{directory}/{upstream_path}",
            "upstream": upstream,
            "commit": commit,
            "upstream_path": upstream_path,
            "git_blob": blob,
            "sha256": sha256,
            "license": license,
            "use": use,
            "modified": False,
        }
        _write(root, lock, entry, data, report)
    save_lock(root, lock)
    return report


def repin(
    root: Path, directory: str, commit: str, source, license: str | None = None, use: str | None = None
) -> Report:
    """Re-fetch every file pinned under `directory` at `commit`, keeping
    each file's license and use unless given; the upstream is the one the
    pins name (all must agree)."""
    _check_commit(commit)
    directory = _check_dir(directory)
    lock = load_lock(root)
    entries = [e for e in lock["files"] if e["path"].startswith(directory + "/")]
    if not entries:
        raise ValueError(f"nothing is pinned under {directory}")
    upstreams = {e["upstream"] for e in entries}
    if len(upstreams) != 1:
        raise ValueError(f"{directory} pins several upstreams: {sorted(upstreams)}")
    report = Report()
    for old in list(entries):
        data = source.read(commit, old["upstream_path"])
        sha256, blob = digests(data)
        entry = {
            "path": old["path"],
            "upstream": old["upstream"],
            "commit": commit,
            "upstream_path": old["upstream_path"],
            "git_blob": blob,
            "sha256": sha256,
            "license": license or old["license"],
            "use": use or old["use"],
            "modified": False,
        }
        _write(root, lock, entry, data, report)
    save_lock(root, lock)
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", default=str(ROOT), help="the repository root (default: this checkout)")
    parser.add_argument("--dir", required=True, help="vendor/<name>, where the files go")
    parser.add_argument("--upstream", help="https://github.com/OWNER/REPO (required unless --repin)")
    parser.add_argument("--commit", required=True, help="the full upstream commit id")
    parser.add_argument("--license", help="SPDX id as the lock records it, such as Apache-2.0, MIT, CC0-1.0")
    parser.add_argument(
        "--use", help="the lock's use: reference-only, upstream-data, port-source, upstream-control-source, ..."
    )
    parser.add_argument(
        "--git", help="a local clone holding the commit; else files come from raw.githubusercontent.com"
    )
    parser.add_argument(
        "--repin", action="store_true", help="re-fetch every file already pinned under --dir at --commit"
    )
    parser.add_argument("paths", nargs="*", help="upstream paths to pin (not with --repin)")
    args = parser.parse_args(argv)
    root = Path(args.root)
    try:
        if args.repin:
            if args.paths:
                parser.error("--repin takes no paths; it re-fetches the ones already pinned")
            lock = load_lock(root)
            entries = [e for e in lock["files"] if e["path"].startswith(args.dir.strip("/") + "/")]
            upstream = args.upstream or (entries[0]["upstream"] if entries else None)
            if upstream is None:
                parser.error(f"nothing is pinned under {args.dir}")
            source = GitSource(Path(args.git)) if args.git else RawSource(upstream)
            report = repin(root, args.dir, args.commit, source, args.license, args.use)
        else:
            if not (args.upstream and args.license and args.use):
                parser.error("--upstream, --license and --use are required to pin new files")
            source = GitSource(Path(args.git)) if args.git else RawSource(args.upstream)
            report = pin(root, args.dir, args.upstream, args.commit, args.paths, args.license, args.use, source)
    except (ValueError, LookupError) as error:
        print(f"vendor_pin: {error}", file=sys.stderr)
        return 1
    print(f"{args.dir} at {args.commit[:12]}: {report.summary()}")
    for path in report.changed:
        print(f"  changed  {path}")
    for path in report.added:
        print(f"  added    {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
