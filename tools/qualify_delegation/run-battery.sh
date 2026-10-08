#!/usr/bin/env bash
# Run the repository scenarios with real Claude Code and score them.
# usage: run-battery.sh [NAME...]    (all scenarios by default)
# QUALIFY_OUT (a fresh directory), BY (the by under test), QUALIFY_JOBS
# (scenarios at once, default 3) and QUALIFY_MODEL (see run-repo.sh) apply.
set -u
here=$(cd "$(dirname "$0")" && pwd)
export QUALIFY_OUT=${QUALIFY_OUT:-$(git rev-parse --show-toplevel)/target/qualify-delegation}
if [ -e "$QUALIFY_OUT/out" ]; then
  echo "run-battery: $QUALIFY_OUT already holds a run; give a fresh QUALIFY_OUT" >&2
  exit 2
fi
python3 "$here/scenarios.py" > /dev/null || exit 1
if [ $# -eq 0 ]; then
  set -- $(python3 -c 'import json,sys,glob,os;[print(os.path.basename(p)[:-5]) for p in sorted(glob.glob(sys.argv[1]+"/repos/*.json"))]' "$QUALIFY_OUT")
fi
printf '%s\n' "$@" | xargs -P "${QUALIFY_JOBS:-3}" -I{} sh -c '"$1" "$2" > "$QUALIFY_OUT/driver.$2.log" 2>&1' _ "$here/run-repo.sh" {}
python3 "$here/score.py" "$QUALIFY_OUT" --json "$QUALIFY_OUT/scores.json"
