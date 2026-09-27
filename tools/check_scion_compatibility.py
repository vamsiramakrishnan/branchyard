#!/usr/bin/env python3
"""Require the pinned Claude suite to match its recorded compatibility baseline.

At Scion 54b9387 the Claude suite failed 11 of its 12 tests against its own
provisioner; at the current pin, d9b9e6a, all 13 pass and the baseline records
no incompatibility. A change in either direction fails this check until the
baseline and docs/validation.md are updated together.
"""
import io
import json
import runpy
import sys
import types
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def observe():
    path = ROOT / "vendor/scion/harnesses/claude/provision_test.py"
    module = types.ModuleType("branchyard_scion_test")
    module.__dict__.update(runpy.run_path(str(path), run_name=module.__name__))
    suite = unittest.defaultTestLoader.loadTestsFromModule(module)
    output = io.StringIO()
    result = unittest.TextTestRunner(stream=output).run(suite)
    observations = []
    for kind, failures in [("failure", result.failures), ("error", result.errors)]:
        for test, traceback in failures:
            # Compare the test and exception, not unstable temporary paths or timing.
            last = traceback.strip().splitlines()
            exception = next((line for line in last
                              if line.startswith(("AssertionError:", "AttributeError:",
                                                  "FileNotFoundError:", "TypeError:"))),
                             last[-1] if last else "unknown")
            observations.append({"test": test.id(), "kind": kind, "exception": exception})
    return {"tests_run": result.testsRun,
            "incompatibilities": sorted(observations, key=lambda x: x["test"])}, output.getvalue()


def main():
    actual, output = observe()
    expected = json.loads((ROOT / "tests/compatibility/scion-claude.expected.json").read_text())
    if actual != expected:
        print(output, file=sys.stderr)
        print(json.dumps(actual, indent=2), file=sys.stderr)
        raise SystemExit("Upstream compatibility changed; inspect it and update the recorded qualification.")
    if actual["incompatibilities"]:
        print(f"KNOWN INCOMPATIBILITY: {actual['tests_run']} Claude tests reproduced "
              f"{len(actual['incompatibilities'])} expected failure/error observations. "
              "See docs/validation.md.")
    else:
        print(f"COMPATIBLE: all {actual['tests_run']} Claude tests pass against the pinned "
              "provisioner, as recorded; see docs/validation.md.")


if __name__ == "__main__":
    main()
