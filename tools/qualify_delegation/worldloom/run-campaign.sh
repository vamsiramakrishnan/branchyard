#!/usr/bin/env bash
# One knowledge-work campaign: Branchyard supervising real Claude Code against a served
# Worldloom company, graded by Worldloom. See ../README.md.
#
# usage: run-campaign.sh NAME MODE CASE8...     MODE: solo | tools | code
# env:   WL_URL           the served cases (`worldloom enterprise-evals serve CASES --port N`), e.g. http://127.0.0.1:8771/mcp
#        WORLDLOOM_PY     a python with worldloom and mcp installed
#        WORLDLOOM_CASES  the case directory the server serves
#        QUALIFY_OUT, BY  output directory and by binary (optional)
set -u
NAME=$1; MODE=$2; shift 2; CASES8=("$@")
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
export WL_URL=${WL_URL:?set WL_URL}
PY=${WORLDLOOM_PY:?set WORLDLOOM_PY}
CASES=${WORLDLOOM_CASES:?set WORLDLOOM_CASES}
BY=${BY:-$(command -v by || echo "$ROOT/target/debug/by")}
K=${QUALIFY_OUT:-$ROOT/target/qualify-delegation}/worldloom
OUT=$K/out/$NAME; REPO=$K/repos/$NAME
export QUALIFY_SCORES=$OUT/scores.jsonl
rm -rf "$REPO" "$OUT"; mkdir -p "$OUT" "$REPO"
export BRANCHYARD_HOME=$K/homes/$NAME; rm -rf "$BRANCHYARD_HOME"; mkdir -p "$BRANCHYARD_HOME"

# The campaign's workspace. Approvals keep the evaluator's own tools away from every agent.
(
  cd "$REPO" && git init -q -b main && mkdir answers && touch answers/.keep
  echo "# Campaign workspace" > README.md
  cat > branchyard.toml <<'TOML'
[approvals]
rules = { "mcp__wl__eval_grade" = "block", "mcp__wl__eval_trace" = "block", "mcp__wl__eval_begin" = "block", "mcp__wl__eval_score" = "block", "mcp__wl__eval_end" = "block", "mcp__wl__eval_list" = "block" }
TOML
  git add -A && git -c user.email=op@example.com -c user.name=op commit -qm seed
)

# Open one graded run per case.
QIDS=$("$PY" - "$CASES" "${CASES8[@]}" <<'E'
import json, sys
cases, prefixes = sys.argv[1], sys.argv[2:]
for line in open(cases + "/queries.jsonl"):
    q = json.loads(line)
    if any(q["id"].startswith(p) for p in prefixes):
        print(q["id"])
E
)
(cd "$HERE" && "$PY" kw.py begin $QIDS) > "$OUT/runs.json" || exit 1
TASKS=$("$PY" - "$OUT/runs.json" "$CASES" <<'E'
import json, sys
runs = json.load(open(sys.argv[1]))
queries = {json.loads(l)["id"]: json.loads(l)["query"] for l in open(sys.argv[2] + "/queries.jsonl")}
for i, (qid, rid) in enumerate(runs.items(), 1):
    print(f"Task {i} (case {qid[:8]}, run_id {rid}): {queries[qid]}")
E
)

COMMON="You work for the company in these tasks. Its systems are reachable as connector tools. EVERY connector call must pass the task's run_id exactly as given; never mix run_ids across tasks. If something is ambiguous you may call eval_ask(run_id, question). Never call eval_grade, eval_trace, eval_begin, eval_score, eval_end or eval_list. Each task's final answer is a short report (what you read, what you changed with record IDs, what you verified, and anything inaccessible); it must be written to answers/<case8>.md and committed on your branch."
CODE="Work in CODE MODE: instead of calling connector tools one by one, write and run Python programs with $PY, the environment variable WORLDLOOM_MCP_URL=$WL_URL and PYTHONPATH=$HERE, using the module wlsdk (\`from wlsdk import call, find, tools\`; see its docstring). One program can do many calls, branch on results and print only what you need. Keep the programs out of the commit (write them under \$TMPDIR)."
FRICTION=" Finally, under a heading 'Branchyard friction', list every error, refusal, confusing message, missing capability or workaround you hit with Branchyard itself (delegation, MCP tools, permissions), with exact commands and messages, or 'none'."
MCP=(--mcp "wl=$PY $HERE/bridge.py $WL_URL")
case $MODE in
  solo)  DEL=(); BUDGET=6
         PROMPT="$COMMON Do every task yourself, in order, using the connector tools.
$TASKS
$FRICTION" ;;
  tools) DEL=(--delegate); BUDGET=10
         PROMPT="You are the meta-harness for this campaign. $COMMON Do not do the connector work yourself: delegate each task to its own child (by spawn, --harness claude-code, at most 1.5 USD each) and give the child the task text and its run_id verbatim, plus the rules above. When the children finish, integrate them so every answers/<case8>.md is on your branch, then write answers/SUMMARY.md: one paragraph per task with its outcome, and commit.
$TASKS
$FRICTION" ;;
  code)  DEL=(--delegate); BUDGET=10; MCP=()
         PROMPT="You are the meta-harness for this campaign. $COMMON $CODE Do not do the connector work yourself: delegate each task to its own child (by spawn, --harness claude-code, at most 1.5 USD each) and give each child the task text, its run_id and the CODE MODE instructions verbatim. When the children finish, integrate them so every answers/<case8>.md is on your branch, then write answers/SUMMARY.md: one paragraph per task with its outcome, and commit.
$TASKS
$FRICTION" ;;
  *) echo "MODE is solo, tools or code" >&2; exit 2 ;;
esac
echo "$PROMPT" > "$OUT/prompt.txt"
date -u +%s > "$OUT/t0"
(cd "$REPO" && timeout 3600 "$BY" run --name meta --harness claude-code "${DEL[@]}" --yes \
  --budget-usd "$BUDGET" "${MCP[@]}" "$PROMPT") > "$OUT/run.log" 2>&1
echo "exit=$?" >> "$OUT/run.log"
date -u +%s > "$OUT/t1"

(cd "$REPO" && "$BY" ls > "$OUT/ls.txt" 2>&1
 for b in $("$BY" ls --json 2>/dev/null | "$PY" -c 'import json,sys
d=json.load(sys.stdin); items=d if isinstance(d,list) else d.get("branches",[])
[print(x["name"]) for x in items]'); do
   "$BY" log "$b" > "$OUT/log.$b.txt" 2>&1
   "$BY" inspect "$b" --json > "$OUT/inspect.$b.json" 2>&1
 done)

# Grade each case from its answer on by/meta. A missing answer there is a finding: it is
# taken from the first child branch that has it, and noted.
"$PY" - "$OUT/runs.json" "$REPO" "$OUT" "$HERE" <<'E'
import json, os, subprocess, sys
runs, repo, out, here = json.load(open(sys.argv[1])), sys.argv[2], sys.argv[3], sys.argv[4]
def show(ref, path):
    return subprocess.run(["git", "-C", repo, "show", f"{ref}:{path}"], capture_output=True, text=True).stdout
for qid, rid in runs.items():
    path = f"answers/{qid[:8]}.md"
    answer, source = show("by/meta", path), "by/meta"
    if not answer:
        refs = subprocess.run(["git", "-C", repo, "for-each-ref", "--format=%(refname:short)", "refs/heads/by/"],
                              capture_output=True, text=True).stdout.split()
        for ref in refs:
            answer = show(ref, path)
            if answer:
                source = ref
                break
    answer_file = f"{out}/answer.{qid[:8]}.md"
    with open(answer_file, "w") as f:
        f.write(answer or "(no answer)")
    with open(f"{out}/answer-source.txt", "a") as f:
        f.write(f"{qid[:8]} {source if answer else 'none'}\n")
    for verb, args in (("score", [rid, answer_file]), ("end", [rid])):
        r = subprocess.run([sys.executable, os.path.join(here, "kw.py"), verb, *args], capture_output=True, text=True, cwd=here)
        if verb == "score":
            with open(f"{out}/score.{qid[:8]}.log", "w") as f:
                f.write(r.stdout + r.stderr)
E
"$PY" "$HERE/summarize.py" "$OUT" > "$OUT/summary.txt" 2>&1
echo done > "$OUT/DONE"
