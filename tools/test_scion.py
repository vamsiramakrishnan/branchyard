#!/usr/bin/env python3
"""Run the selected upstream provisioning tests in separate Python processes."""
import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualified", action="store_true",
                        help="Run the six passing suites; report Claude's upstream mismatch as excluded")
    args = parser.parse_args()
    base = ROOT / "vendor/scion/harnesses"
    suites = [base / "scion_harness_test.py", *sorted(base.glob("*/provision_test.py"))]
    if args.qualified:
        print("EXCLUDED: Claude model-alias suite is inconsistent at the pinned upstream revision. "
              "See docs/validation.md; use the default command to reproduce.", flush=True)
        suites = [s for s in suites if s.parent.name != "claude"]
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    failures = []
    for suite in suites:
        print(f"Testing {suite.relative_to(ROOT)}", flush=True)
        result = subprocess.run([sys.executable, str(suite)], cwd=suite.parent, env=env)
        if result.returncode:
            failures.append(str(suite.relative_to(ROOT)))
    if failures:
        raise SystemExit("Failed: " + ", ".join(failures))
    print(f"Passed {len(suites)} upstream test suites.")


if __name__ == "__main__":
    main()
