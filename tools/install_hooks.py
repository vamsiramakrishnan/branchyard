#!/usr/bin/env python3
"""Point git at the committed pre-push hook, which runs `python3 tools/check.py --fast`.

    python3 tools/install_hooks.py               install (running it again changes nothing)
    python3 tools/install_hooks.py --uninstall   unset core.hooksPath

The hook is committed as .githooks/pre-push. Installing writes that file when it differs
from HOOK here, and sets `core.hooksPath` to `.githooks` in this clone's config; every
worktree of the clone shares that setting. --uninstall removes the setting and leaves the
committed file, so git ignores the hook. To push once without the checks: git push --no-verify
"""

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HOOKS = ".githooks"
HOOK = """#!/bin/sh
# pre-push, installed by tools/install_hooks.py: runs the fast checks before a push.
# To push once without them: git push --no-verify
set -eu
cat > /dev/null  # git sends the refs on stdin; the checks do not need them
if ! python3 tools/check.py --fast; then
    echo "pre-push: the checks failed, so the push was refused." >&2
    echo "pre-push: fix them, or push once without them: git push --no-verify" >&2
    exit 1
fi
"""


def git(root, *args):
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)


def configured(root):
    """This clone's core.hooksPath, or '' when it is not set here."""
    done = git(root, "config", "--local", "--get", "core.hooksPath")
    return done.stdout.strip() if done.returncode == 0 else ""


def install(root=ROOT):
    """Write the hook if it differs and point core.hooksPath at it. Returns what happened."""
    current = configured(root)
    if current not in ("", HOOKS):
        raise SystemExit(f"core.hooksPath is {current!r} in this clone; unset it before installing the hook")
    if current != HOOKS:
        # The setting first: outside a repository it fails, and then no file is written.
        done = git(root, "config", "core.hooksPath", HOOKS)
        if done.returncode != 0:
            raise SystemExit(f"git config core.hooksPath failed: {done.stderr.strip()}")
    hook = root / HOOKS / "pre-push"
    written = not hook.exists() or hook.read_text() != HOOK
    if written:
        hook.parent.mkdir(exist_ok=True)
        hook.write_text(HOOK)
        hook.chmod(0o755)
    return "installed" if written or current != HOOKS else "already installed"


def uninstall(root=ROOT):
    """Unset core.hooksPath when it points at the hook; the committed file stays."""
    current = configured(root)
    if current == "":
        return "not installed"
    if current != HOOKS:
        return f"core.hooksPath is {current!r}, not {HOOKS!r}; left alone"
    done = git(root, "config", "--unset", "core.hooksPath")
    if done.returncode != 0:
        raise SystemExit(f"git config --unset core.hooksPath failed: {done.stderr.strip()}")
    return "uninstalled"


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--uninstall", action="store_true", help="unset core.hooksPath")
    args = parser.parse_args(argv[1:])
    print(uninstall() if args.uninstall else install())
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
