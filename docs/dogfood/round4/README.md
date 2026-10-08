# Dogfood round 4

Round 4 raised Branchyard's quality as a delegation substrate, measured on the real-harness battery
(`tools/qualify_delegation/`). A Claude Code meta-harness on `by/meta4` delegated every change to
Claude Code children. A separate read-only reviewer checked each change before it was integrated
through the `df-check` gate.

## Results

Both runs used `tools/qualify_delegation/run-battery.sh`, which runs twelve scenarios, three at a
time. `baseline.json` was measured on `346b968`. `after.json` was measured on `by/meta4` with every
kept improvement integrated.

| | baseline | after |
|---|---|---|
| scenarios run / passed | 12 / 12 | 12 / 12 |
| cost (USD) | 10.07 | 9.27 |
| median wall (s) | 216 | 196 |
| protocol violations | 0 | 0 |
| refusals | 11 | 7 |
| friction items | 26 | 22 |

Runs: baseline `/home/user/df/eval/baseline/run` (15:58–16:13Z), after `/home/user/df/eval/after/run` (2026-10-08, about 19:33–19:47Z).

What moved, from the evaluator's reading of the friction lists (`/home/user/df/eval/after/report.md`):
- The background job dying with the session (compete, pysdk) is gone (K2).
- The MCP `budget_usd` errors (mcponly) are gone (K7 surface).
- The backtick quoting in a steer is gone: the steer meta used `--prompt-file`.
- Two scorer artifacts work against the after-run. `envelope`'s meta quoted its two refusals with
  `refused:` in its friction prose, so they count twice: there are 5 distinct refusal events, not 7.
  `crash`'s friction rose 0→3 because its recovery turn used a `##` heading this time, which the
  scorer counts.
- One run per side: models vary run to run (see `tools/qualify_delegation/README.md`).

See `checks.md` for the verification report and `progress.md` for the handoff.

## What changed

- **K0.1, K0.2 (`k0-driver`).** The Claude Code driver ends a turn only on a definite signal: the
  turn's own result, or `command_lifecycle completed`/`cancelled` for its message. It no longer ends
  on `queued_turn_count == 0`. A message that joins a foreign cycle keeps its text and tool calls.
- **K0.3, K0.4, K0.5, K0.9 (`k0-engine`).** Fixed three things: the sibling advice on a failed
  shared check, the recorded cost falling after a rewind, and a merged child turning `ready`. Also
  documented the Python `own_work` property.
- **K0.6–K0.8, K9, K10, and a gate flake (`k0-tests`).**
  - The fake agent exits when its directory disappears, and the leaking test's cleanup order is
    fixed.
  - `testing::capture` no longer loses a callsite to tracing's global interest cache.
  - The `graph_commands_print_what_local_ones_do` race is fixed.
  - Two test gaps are closed.
- **K2 (`k2-hold`).** A Claude Code turn whose result arrives while its background tasks are still
  running is held open until they finish and Claude Code answers their notification. The hold is
  bounded by the turn's duration limit, or by 30 minutes from the result if the turn has none. If
  the limit ends the hold, the parent gets a warning naming the tasks.
- **K7, K11 (`k7-surface`).**
  - `by send --prompt-file` and `by steer` were added.
  - The MCP `spawn` tool and `apply_graph` accept flat `budget_usd`, `max_turns` and
    `max_minutes`.
  - An invalid branch name gets a suggested valid one.
- **K8 docs (`docs-skill`).** Documented three things: who may share an artifact, that
  `graph apply`'s `check` is an argv array, and that a background shell job is not a way to wait.

## The procedure (reusable)

Run it again with a new backlog. Every step leaves an observable output.

1. **Baseline.** Spawn an evaluator child from the round's base. It:
   - builds `by` (`seed-target`, then `cargo build -p branchyard-cli --locked --offline`) and
     copies `target/debug/by` to `eval/baseline/by`;
   - deletes its `target`;
   - runs one scenario (`run-battery.sh steer` into a scratch `QUALIFY_OUT`) to confirm the
     battery works;
   - runs the full battery with
     `QUALIFY_OUT=eval/baseline/run BY=eval/baseline/by run-battery.sh`;
   - copies `scores.json` to `docs/dogfood/roundN/baseline.json`;
   - reports the totals, every refusal and violation, and the most frequent friction themes, each
     with its scenarios.

   Start long runs with `nohup … &`. Then wait in the foreground with repeated
   `timeout 540 bash -c 'until [ -f …/scores.json ]; do sleep 10; done'` calls. Never end a turn to
   wait for a background job.
2. **Select.** Review findings on the previous round come first. Then rank the backlog by the
   baseline's evidence. Protocol violations, refusals and friction are counted by `score.py`; read
   its rules. A refusal the scenario provokes by design (for example, a lone branch failing a
   whole-suite check) is not a lever. Before a child starts, write its requirement as a row in
   `checks.md`.
3. **Build.** One child per improvement, on the default model. Its prompt holds:
   - the requirement and its evidence;
   - the child rules: build hygiene, `by check` waited for in the foreground, a test per behaviour
     change, the operations registry, and files that are off limits.

   Keep at most two children building or testing Rust at once.
4. **Review.** Before integrating, spawn a separate reviewer with `--deny Edit,Write` (one flag,
   comma list). Give it the requirement, `git diff by/<meta>...by/<child>` and the acceptance
   checks. It runs each new test with and without its fix in a scratch `git worktree` under
   `$TMPDIR`, and answers with a verdict, evidence and corrections. For a fail, send the builder
   back with the corrections, then send the same reviewer the revision; `by send` keeps its
   context. A docs-only change can use `--model medium`.
5. **Integrate.** Merge what passed review, several branches at once (`by integrate a b c`), with
   the integration running in the background. Commit your own notes first, and edit nothing in your
   worktree while it runs: a dirty worktree is refused only after the 15-minute gate has run.
6. **Measure.** Spawn the after-evaluator on the integrated branch, the same way, into
   `eval/after`. It writes `after.json` and reports
   `score.py --compare` against the baseline, plus the friction themes that changed.
7. **Keep or undo.** If the pass count drops, or violations, refusals or friction rise, or cost
   goes over 110%: find the responsible change from the per-scenario scores and friction text.
   Have a child revert it, measure again, and record it in `checks.md` as discarded.
8. **Report.** Update `checks.md`, `progress.md` and this README.
