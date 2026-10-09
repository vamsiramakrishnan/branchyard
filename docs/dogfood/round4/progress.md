# Round 4 progress

**Task:** raise Branchyard's measured quality on the real-harness battery (12 scenarios), keep
`df-check` green, leave `baseline.json`, `after.json`, `checks.md`, `progress.md`, `README.md` here
on `by/meta4`. Limits: $150 yard budget, 6 h wall time (started 2026-10-08), at most two children
building Rust at once, no edits to the evaluator, `.github/` or generated files.

**Outputs:**
- `docs/dogfood/round4/checks.md`: checklist C1–C6 and per-improvement rows.
- After battery: `QUALIFY_OUT=/home/user/df/eval/after/run`; `docs/dogfood/round4/after.json`; `docs/dogfood/round4/README.md` (table and reusable procedure).
- Baseline battery: `QUALIFY_OUT=/home/user/df/eval/baseline/run` (2026-10-08 15:58:23Z–16:13:47Z), binary `/home/user/df/eval/baseline/by` built from `346b968`; smoke run `/home/user/df/eval/baseline/smoke2` (steer, passed). Evaluator report `/home/user/df/eval/baseline/report.md`. `docs/dogfood/round4/baseline.json` (integrated).

**Completed:**
- Read the backlog, the battery README and the delegate skill. Wrote the C1–C6 checklist.
- Baseline measured: 12/12 passed, cost $10.07, median wall 216 s, protocol violations 0, refusals 11 (conflict3 4, pysdk 3, depth2 2, envelope 2), friction 26.
- Six improvements built, each passed a separate reviewer, and all six are integrated into `by/meta4` through the gate (df-check passed on each integration):
  - 17:22: `eval-baseline`, `k0-engine`, `docs-skill` (e7c1bad86b..b3e63b2141);
  - ~17:50: `k0-driver` (c52c72201c..464ca12922);
  - ~19:10: `k0-tests`, `k7-surface` (0287f21b7e..dd18526e89);
  - 19:30: `k2-hold` (df95d775f3..3ed8527197).
- Review rounds: k0-engine 2, k0-driver 2, k2-hold 3, k0-tests 1, k7-surface 1, docs-skill 1.
- C5 verified at 19:31: `git diff 346b968 -- tools/qualify_delegation/{scenarios.py,run-repo.sh,run-battery.sh,score.py}` is empty, and so is the diff of `.github`, `docs/facts.json`, `schema/contract.json`, `tools/missing_docs_ratchet.json` and `docs/validation.md`.
- 19:31: `eval-after` spawned on `3ed8527` (model medium). After-battery `QUALIFY_OUT=/home/user/df/eval/after/run` (about 19:33–19:47Z; smoke run `/home/user/df/eval/after/smoke`, steer, passed), binary `/home/user/df/eval/after/by`, report `/home/user/df/eval/after/report.md`.
- 20:30: final `by check` on `by/meta4` at `617626b` (all of round 4, docs included): `outcome passed`.
- Usage recorded by Branchyard (`by inspect meta4`, 20:30): meta $5.00, subtree $36.47 of the $150 budget. The batteries' own scenario costs, as `score.py` reports them, were $10.07 (baseline) and $9.27 (after), plus smoke runs of $0.47 and $0.51. They ran as separate yards (`run-repo.sh` clears `BRANCHYARD_*`).
- After-battery: 12/12 passed, cost $9.27, median wall 196 s, protocol violations 0, refusals 7, friction 22. C2 and C3 pass; nothing discarded.

**Decisions:**
- Base of `by/meta4`: `346b968`.
- Session settings: `by inspect meta4` shows harness claude-code (stream-json), model "the harness's default" (Opus 5.5 per the session), envelope depth 1, 4 live children. Runtime effort is not visible to Branchyard.
- K0.1 and K0.2 go to one child (`k0-driver`): both are in the same state machine in `claude_code.rs`.

- Selection (from the baseline): violations are already 0, so C3 has to come from refusals or friction. 7 of 11 refusals are a lone branch failing a whole-suite check, which the scenarios (conflict3, pysdk, depth2) provoke by design; 2 are envelope refusals the `envelope` scenario provokes. Friction is the lever: background work dying with the turn (compete, pysdk, and two of our own children) → K2; `mcponly` MCP `budget_usd`, `steer` backtick quoting, our own `send --prompt-file` → K7/K11 surface; artifacts share and graph `check` docs → docs. K1 (engine-owned integration), K3–K6 have no battery evidence this round and are large: deferred.
- `score.py` counts every top-level bullet under the friction heading, including "Otherwise none" bullets, so friction is noisy (±3 run to run is plausible).
- Evaluator and docs children run on `--model medium`; builders and reviewers on the default model.
- Reviewers are spawned with `--deny Edit,Write` (`--deny` takes a comma list; passing it twice is refused).

- Gate flake `graph_commands_print_what_local_ones_do` (operator note): given its own checks.md row and handed to `k0-tests`; a check failing only on it is re-run once.

- Builder and reviewer children use the default model (Opus 5.5 per the session; `by inspect` shows "the harness's default"). Evaluators and the docs builder and reviewer used `--model medium`, recorded in their `model` field. Runtime effort is not exposed by Branchyard and was not set. No same-task comparison of configurations was run (`QUALIFY_MODEL` against the default): a second pair of batteries did not fit in the wall time after the main loop.
- The rule of at most two Rust builders was read as: a child waiting for its `by check` in the gate's lock does not count as building, since the gate's lock runs one check at a time in its own `check-target`. `k2-hold` was started at 17:58 while `k0-tests` and `k7-surface` both waited for their checks.

**Open issues:**
- C4's weakest point: K2's engine use of the 30-minute hold cap (`engine.rs` `Stop::Hold`) is covered only through the unit-tested `hold_cut`. K2 is not yet observed against a real, idle Claude Code answering an interrupt.
- Leftover `fake-acp-agent` processes from older gate runs (PIDs listed in `k0-tests`' report, e.g. 2438, 4806 in `/home/user/df/check-target/debug`) were not killed (not ours to signal); the new watchdog stops new ones.
- K2 live (operator note, 15:48): `eval-baseline` (model medium) ended its first turn "Waiting for the smoke test to finish — no action needed from me until the notification arrives"; `k0-driver` ended its first turn "The gate is still running; I'll wait for its notification." In print mode the session closes and kills the background job. The smoke run (`/home/user/df/eval/baseline/smoke`) died with no scores. Both were resent with an explicit foreground-wait recipe (`timeout 540 bash -c 'until …; do sleep 10; done'`, repeated); the recipe is now part of every child prompt. Strong evidence for K2.
- `by send` has no `--prompt-file` (K11): `by send eval-baseline --prompt-file x` → `error: unexpected argument '--prompt-file' found  tip: a similar argument exists: '--ca-file'`. Long follow-ups had to be inlined.
- Gate fault (operator note, ~16:05): `/home/user/df/checks/160522-25878.log` failed branchyard-recipe's provider tests (`python3: can't open file '/tmp/branchyard-integrate-30251-…/crates/branchyard-recipe/tests/fixtures/fake-ssh'`): a check queued on the gate's lock reused test binaries built for another, deleted checkout, which embed that checkout's paths. The operator fixed the gate (file times refreshed). Backlog evidence: checks sharing a build cache across checkouts, and tests embedding compile-time paths (b13761e fixed testkit; the recipe tests do the same). The children were told to re-run, not to change code for it.

- Disk: 11 GB free at 17:20 (`df -h`); `rev-docs-skill` saw `target/` builds fail under disk pressure.

**Next action:** none: round 4 is complete. For round 5, start from the 'Not attempted' list in checks.md, from the operator's gate notes in Open issues (shared build cache across checkouts; recipe tests embedding compile-time paths; a whole-binary leftover-process check in `df-check`), and from the after-run's friction themes in `/home/user/df/eval/after/report.md` (conflict resolution between siblings, `by inspect`'s `envelope.harnesses` vs `allowed_harnesses`, the budget reservation, a spawn lost in an engine crash).
