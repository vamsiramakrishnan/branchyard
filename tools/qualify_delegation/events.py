#!/usr/bin/env python3
"""Print every event a branch recorded, one JSON object per line, oldest first.

    python3 tools/qualify_delegation/events.py BY BRANCH

`BY` is the `by` binary, run in the scenario's repository; it pages through
`by events BRANCH --json` from the first event.
"""

import json
import subprocess
import sys

PAGE = 200


def main(argv):
    by, branch = argv
    cursor = 0
    while True:
        shown = subprocess.run(
            [by, "events", branch, "--cursor", str(cursor), "--limit", str(PAGE), "--json"],
            capture_output=True,
            text=True,
            check=True,
        )
        page = json.loads(shown.stdout)
        for event in page["events"]:
            print(json.dumps(event))
        if not page["events"] or page["next_cursor"] >= page["total"]:
            return 0
        cursor = page["next_cursor"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
