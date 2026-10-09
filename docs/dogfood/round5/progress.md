# Round 5 progress

**Task:** raise Branchyard's score on the real-harness battery (12 scenarios) and keep `df-check` green; leave `baseline.json`, `after.json`, `checks.md`, `progress.md`, `README.md` here on `by/meta5`. Limits: $150 yard budget, 6 h wall, at most two Rust builders at once, no edits to the evaluator, `.github/` or generated files. No question for the operator.

**Outputs:**
- `docs/dogfood/round5/`: `baseline.json` (copy of round 4's `after.json`, reused: it measured this branch's starting build, so it was not re-run), `after.json` (final head, run 4), `after-run1.json`, `after-run2.json`, `after-run3.json`, `after-run5.json`, `checks.md`, `README.md`.
- Batteries (`QUALIFY_OUT`, binary `by` beside them): baseline `/home/user/df/eval/after/run` (2026-10-08 19:34–19:47Z); run 1 `/home/user/df/eval5/after/run` (22:45–23:00Z); run 2 `/home/user/df/eval5/after2/run` (23:38–00:10Z); run 3 `/home/user/df/eval5/after3/run` (~00:55–01:10Z); run 4 = `after.json`, `/home/user/df/eval5/after4/run` (~01:50–02:08Z); run 5 (repeat of run 4's code) `/home/user/df/eval5/after5/run` (~02:10–02:33Z). Each directory has `report.md` (the evaluator's).
- Prompts used for the children: `/home/user/df/prompts5/`.

**Completed:**
- Baseline: 12/12, $9.27, violations 0, refusals 7, friction 22.
- Six builders, each reviewed by a separate `--deny Edit,Write` reviewer and integrated through the gate: r0-hold + r0-surface (fccfc71), r0-docs-gate (9d9761e), r0-fastbg (ed9777c), r0-taskstop (d5a5f96), r0-wakestale (5781a04); eval-after4 (`after.json`).
- Runs: 1: 11/12, $10.52, refusals 11, friction 19 (pysdk failed: a background command ending before the result, fixed by r0-fastbg). 2: 11/12, $9.34, 4, 28 (compete failed: model TaskStop, a 30 min hold, fixed by r0-taskstop). 3: 12/12, $10.70, 6, 21 (crash cost spike: stale wake, fixed by r0-wakestale). 4 (final): 12/12, $10.39, 9, 24. 5 (repeat): 12/12, $12.51, 12, 22.
- C1, C2, C4, C5, C6 pass; **C3 fails** (see checks.md).
- Usage recorded by Branchyard: see `by inspect meta5` (subtree cost). Battery costs are the scenarios' own (separate yards).

**Decisions:**
- Selected R0 and F1/F2/F3, the `envelope.harnesses` naming and a conflict recipe because violations were already 0 and friction/refusals are the lever; K1/K3-K6 had no battery evidence and are large.
- Session: harness claude-code (stream-json), model "the harness's default" (Sonnet 5.5 per the session) for builders and reviewers; evaluators and the two docs reviewers `--model medium`; runtime effort not visible to Branchyard and not set. No same-task `QUALIFY_MODEL` comparison was run (time).
- `--deny Edit,Write` (a comma list): `--deny` repeats were refused by the `by` in use; F1 fixes it for later.
- Each follow-up (r0-fastbg, r0-taskstop, r0-wakestale) was a defect exposed by the previous fix on a real run; none was reverted.
- A reviewer given a base without the code under review judged the docs false (`rev-r0-docs-gate`); redone as `rev2-r0-docs-gate` on the integrated head.

**Open issues:**
- C3: the final run has refusals 9, friction 24, cost 112%; the repeat is worse; the noise between runs of the same code is larger than the effect. Unresolved whether round 5's hold changes add cost: `crash` costs $1.80 / $0.62 / $2.68 in runs 3-5 (baseline $0.50), through a held turn with steered inputs, a 10 s grace and sometimes a second turn; not diagnosed.
- Run 5 `artifacts`: the meta reported that `by fork gen "..." --name gen2` run from the harness created a free-standing root branch, not a descendant, so `by discard gen2` was refused ("not a descendant of meta"). Not verified; worth a look.
- The 10 s grace costs 10 s each time a meta backgrounds `by wait` then ends its turn (it ends "held"). The skill still tells metas not to background `by wait`.
- Fixtures for R0.2, R0.7 and TaskStop's input shape are reconstructed, not recorded.
- `branchyard-recipe` `the_recipe_provider_passes_sandbox_conformance` flakes with `Text file busy` (`/tmp/meta5-int5.log`).
- Docs: `docs/lifecycle.md` does not yet mention the 10 s grace. Facts and the generated files are for the operator to regenerate (new tests change the test count).

**Next action:** the operator regenerates `docs/facts.json` and the other generated files, reads `checks.md` C3, and decides whether round 6 starts by measuring the baseline's variance (several runs of the same build) before any change.
