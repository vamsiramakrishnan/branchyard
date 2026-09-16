#!/usr/bin/env python3
"""Execute one installed Branchyard binary without interpreting shell input."""
import json
import os
import shutil
import sys


def main():
    executable = os.environ.get("BRANCHYARD_BIN") or shutil.which("branchyard")
    if not executable:
        print(json.dumps({"error": "missing_cli", "message": "Install the Branchyard CLI or set BRANCHYARD_BIN to its path."}), file=sys.stderr)
        return 2
    try:
        os.execv(executable, [executable, *sys.argv[1:]])
    except OSError:
        print(json.dumps({"error": "unavailable_cli", "message": "Cannot execute the configured Branchyard binary."}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
