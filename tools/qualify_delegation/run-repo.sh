#!/usr/bin/env bash
# Run one delegation scenario (made by scenarios.py) with real Claude Code and collect its evidence.
# usage: run-repo.sh NAME    (QUALIFY_OUT, BY override the output directory and the by binary)
set -u
NAME=$1
# Run from inside a Branchyard turn, the inherited variables name the outer
# yard, and `by` refuses to act on it from another repository. Each scenario
# is a yard of its own.
unset $(env | grep -o '^BRANCHYARD_[A-Z_]*' | grep -vx BRANCHYARD_HOME)
# QUALIFY_MODEL runs every scenario's meta on that model (default: the harness's).
MODEL=()
[ -n "${QUALIFY_MODEL:-}" ] && MODEL=(--model "$QUALIFY_MODEL")
B=${QUALIFY_OUT:-$(git rev-parse --show-toplevel)/target/qualify-delegation}
BY=${BY:-$(command -v by || echo "$(git rev-parse --show-toplevel)/target/debug/by")}
REPO=$B/repos/$NAME
OUT=$B/out/$NAME
mkdir -p "$OUT"
export BRANCHYARD_HOME=$B/homes/$NAME
mkdir -p "$BRANCHYARD_HOME"
SPEC=$B/repos/$NAME.json
PROMPT=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["prompt"])' "$SPEC")
BUDGET=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["budget"])' "$SPEC")
DELEGATE=$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1])).get("delegate");print("--delegate=%s"%d if d else "--delegate")' "$SPEC")
mapfile -t EXTRA < <(python3 -c 'import json,sys;[print(a) for a in json.load(open(sys.argv[1])).get("extra",[])]' "$SPEC")
CRASH=$(python3 -c 'import json,sys;print(1 if json.load(open(sys.argv[1])).get("crash") else 0)' "$SPEC")
cd "$REPO"
date -u +%FT%TZ > "$OUT/started"
if [ "$CRASH" = 1 ]; then
  setsid "$BY" run --name meta --harness claude-code "${MODEL[@]}" "$DELEGATE" --yes --budget-usd "$BUDGET" \
    --check "python3 run_tests.py" "${EXTRA[@]}" "$PROMPT" > "$OUT/run.log" 2>&1 &
  RUNPID=$!
  # Kill the engine once two children are running.
  for _ in $(seq 1 600); do
    n=$("$BY" children meta --json 2>/dev/null | python3 -c 'import json,sys
try: print(sum(1 for b in json.load(sys.stdin)["descendants"] if b["status"]["state"]=="running"))
except Exception: print(0)')
    [ "${n:-0}" -ge 2 ] && break
    sleep 2
  done
  "$BY" ls > "$OUT/before-kill.txt" 2>&1
  kill -9 -- -"$RUNPID" 2>/dev/null; kill -9 "$RUNPID" 2>/dev/null
  echo "killed engine pgid $RUNPID at $(date -u +%T)" >> "$OUT/run.log"
  sleep 3
  pgrep -af 'claude' | grep -v grep > "$OUT/orphans-after-kill.txt" || true
  "$BY" ls > "$OUT/after-kill.txt" 2>&1
  timeout 2400 "$BY" send meta "Your previous turn was interrupted when Branchyard's engine was killed. Check the state of your children through Branchyard, recover whatever is needed, finish the task and integrate all four modules. Report what you found on recovery." --yes > "$OUT/resume.log" 2>&1
  echo "resume exit=$?" >> "$OUT/resume.log"
else
  timeout 2700 "$BY" run --name meta --harness claude-code "${MODEL[@]}" "$DELEGATE" --yes --budget-usd "$BUDGET" \
    --check "python3 run_tests.py" "${EXTRA[@]}" "$PROMPT" > "$OUT/run.log" 2>&1
  echo "exit=$?" >> "$OUT/run.log"
fi
date -u +%FT%TZ > "$OUT/ended"
"$BY" ls > "$OUT/ls.txt" 2>&1
"$BY" ls --json > "$OUT/ls.json" 2>&1
for b in $("$BY" ls --json 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin)
items=d if isinstance(d,list) else d.get("branches",[])
[print(x["name"]) for x in items]'); do
  "$BY" log "$b" > "$OUT/log.$b.txt" 2>&1
  "$BY" inspect "$b" --json > "$OUT/inspect.$b.json" 2>&1
done
"$BY" graph show meta --json > "$OUT/graph.json" 2>&1
git -C "$REPO" log --all --oneline --graph > "$OUT/git.txt" 2>&1
git -C "$REPO" branch -a > "$OUT/branches.txt" 2>&1
V=$(mktemp -d "$B/verify.$NAME.XXXX"); git -C "$REPO" worktree add -q --detach "$V" by/meta 2>>"$OUT/verify.txt" && (cd "$V" && python3 run_tests.py >> "$OUT/verify.txt" 2>&1; echo "verify exit=$?" >> "$OUT/verify.txt"); git -C "$REPO" worktree remove --force "$V" 2>/dev/null
echo done > "$OUT/DONE"
