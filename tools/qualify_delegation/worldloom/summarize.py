"""Summarize one campaign's graded results: score, pass, calls per case; cost and wall time."""

import json
import pathlib
import re
import sys

out = pathlib.Path(sys.argv[1])
docs = [json.loads(line) for line in (out / "scores.jsonl").read_text().splitlines() if line.strip()]
ls = (out / "ls.txt").read_text() if (out / "ls.txt").exists() else ""
cost = sum(float(c) for c in re.findall(r"\$(\d+\.\d+)", ls))
seconds = int((out / "t1").read_text()) - int((out / "t0").read_text())
sources = (out / "answer-source.txt").read_text().split("\n") if (out / "answer-source.txt").exists() else []
print(f"{'case':10} {'score':>6} {'passed':>6} {'calls':>5}")
for d in docs:
    s = d.get("score", {})
    print(f"{d.get('case_id', '')[:8]:10} {s.get('score', 0):6.3f} {s.get('passed')!s:>6} {d.get('calls', 0):5}")
if docs:
    mean = sum(d.get("score", {}).get("score", 0) for d in docs) / len(docs)
    calls = sum(d.get("calls", 0) for d in docs) / len(docs)
    print(f"mean score {mean:.3f}, mean calls {calls:.1f}, cost ${cost:.2f}, wall {seconds}s")
off_meta = [s for s in sources if s and not s.endswith("by/meta")]
if off_meta:
    print("answers not on by/meta (the meta did not integrate them):", ", ".join(off_meta))
