# Dogfood round 5

Branchyard (as `by/meta5`) delegating to real Claude Code children, measured on the fixed
real-harness battery (`tools/qualify_delegation/`, twelve scenarios, evaluator unchanged).

Files: `baseline.json` (round 4's after-battery, reused: it measured this branch's starting build),
`after.json` (the final measurement of the integrated branch), `after-run1.json`, `after-run2.json`,
`after-run3.json` (the earlier measurements, each of an earlier head), `checks.md` (verification
report, one row per check and improvement), `progress.md` (handoff).

## Result

Totals (`score.py`), baseline against each after-battery:

| run | build | passed | cost | median wall | violations | refusals | friction |
|---|---|---|---|---|---|---|---|
| baseline | `cff2b26` (round 4 final) | 12/12 | $9.27 | 196 s | 0 | 7 | 22 |
| after 1 | + r0-hold, r0-surface, r0-docs-gate | 11/12 (pysdk) | $10.52 | 193 s | 0 | 11 | 19 |
| after 2 | + r0-fastbg | 11/12 (compete) | $9.34 | 190 s | 0 | 4 | 28 |
| after 3 | + r0-taskstop | 12/12 | $10.70 | 222 s | 0 | 6 | 21 |
| **after (final, `after.json`)** | + r0-wakestale | **12/12** | **$10.39** | 224 s | 0 | **9** | **24** |
| after 5 (repeat of the final code) | same | 12/12 | $12.51 | 237 s | 0 | 12 | 22 |

Acceptance: C1 (`df-check`) passes on every integration; C2 passes (12 of 12 in the final run);
**C3 is not met by the final run**: protocol violations equal (0), but refusals 9 > 7, friction
24 > 22, and cost is 112% (limit 110%). The runs 2 and 3 each met part of it (run 3: refusals 6 and
friction 21, both lower, cost 115%; run 2: refusals 4, cost 101%, friction 28 and one failure).
No single run met C2 and C3 together. A repeat of the final code (after 5, `after-run5.json`) was worse still on cost and refusals (`crash` $2.68, `artifacts` 4 refusals, `envelope` 5), which is the measure of the noise: the spread between two runs of the same code is as large as any effect seen. **C3 is reported as failed.**

Reading the numbers: the swing between runs of near-identical code (refusals 4 to 11, friction 19
to 28, cost $9.3 to $10.7) is larger than the changes we made. Most refusals are provoked by design
(`conflict3`, `envelope`, `depth2`: the inherited whole-suite check and the budget envelope).
`depth2` alone moved from $1.02 / 0 refusals to $2.79 / 5 (run 1) and $2.21 / 5 (final) and back to
$1.0 / 0 (runs 2, 3), with no hold, wake or `wait` mechanism in its logs. A single baseline draw is
a thin reference for a "no higher" test.

## What changed (kept, all reviewed by a separate reviewer and integrated through the gate)

- `r0-hold`: a cut hold keeps the held outcome (Completed, with a warning) so the branch parks and
  wakes; notifications are matched to follow-up cycles by the CLI's echoed `user` frames; the
  harness dies with the engine thread (parent-death tie).
- `r0-surface`: a merged branch that delegates is parked and restored; `by graph apply` accepts the
  flat limits; `by steer`'s sender; `--deny` repeats; `by wait` prints each settled branch
  (`--json` lines on stderr); `by inspect` shows the enforced harness list in `envelope.harnesses`.
- `r0-docs-gate`: docs for the above, a recipe for resolving a conflict between siblings, the
  artifact-share fix; `by integrate` refuses a dirty target (including a file in the way of the
  merge) before its check runs.
- `r0-fastbg` (found by after 1): a background command that ends before the turn's result still
  holds the turn for its follow-up cycle.
- `r0-taskstop` (found by after 2): a task the model stopped owes no follow-up; a hold waiting only
  for a notification ends after a 10 s grace; a cut hold whose children settled wakes the parent.
- `r0-wakestale` (found by after 3): that wake skips children the parent already merged or discarded.

Not done: K1 (engine-owned integrations), K3/K12 (check concurrency), K4-K6, R0 item 6's
other parts. No battery evidence this round and each is large.

## The procedure (reusable)

1. **Baseline.** Reuse the previous round's after-battery if it measured the same build, else run
   the battery once (`run-battery.sh` with a fresh `QUALIFY_OUT`, `BY` = the built binary); copy
   its `scores.json` to `baseline.json` and say so in `progress.md`.
2. **Select.** Review findings on the previous round first, then the backlog, by expected effect
   on the acceptance checks and by size. Write each requirement into `checks.md` before a child starts.
3. **Build.** One child per improvement (at most two building Rust at once), with the child rules
   verbatim and a fixed foreground-wait recipe (`by wait`, each command under 10 minutes).
4. **Review.** A separate reviewer child with `--deny Edit,Write` on the improvement's branch,
   given the requirement and the diff command, answering pass / fail / unresolved with evidence.
   Give it a base that contains the code the change is described against (see friction below).
5. **Integrate** what passed, together where possible, through the gate; inspect before any retry.
6. **Measure.** An evaluator child builds `by`, runs one scenario as a smoke test, then the full
   battery, waiting in the foreground, and writes `after.json` and a report with `score.py --compare`.
7. **Keep or undo.** Read the failing scenario's logs, not the counts; find the mechanism; fix or
   revert it with a child, review, integrate, and measure the whole battery again. Discard
   evaluator branches whose measurement was superseded (`by discard`); keep their scores as
   `after-runN.json`.
8. **Report.** `checks.md`, `progress.md`, this file.

Lessons for the next round: (a) measure variance first: run the baseline twice, since a single
draw decides "no higher" checks; (b) an improvement that changes when a turn ends needs a
real-recording test, not a reconstructed fixture (three of this round's four follow-ups came from
shapes only the real harness produces); (c) the battery pays for itself here: it found a missed
background-task shape, a 30-minute stall and a stale wake that no test had.
