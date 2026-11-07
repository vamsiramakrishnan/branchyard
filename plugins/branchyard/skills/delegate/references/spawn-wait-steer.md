# Delegate reference: Decompose, spawn, wait and steer by hand

Read when: you drive children yourself: spawn with a model, wait or end the turn, steer or send a correction, retry.

Steps 1 to 3 of "A managed lead coordinates code by hand"; steps 4 and 5 are in integrate-and-cleanup.md.

## A managed lead coordinates code by hand

1. Decompose. Give each child a self-contained prompt (see "Writing a
   child's prompt"): the goal, the files involved, the constraints, and how
   to check the result.
2. Spawn. Pick a harness per subtask and a budget that fits in what you
   have left (`by inspect` shows `remaining_usd`). A child starts from your
   current work: your uncommitted changes are committed to your branch
   first.
   Pick a model too when the work is mechanical: a rename across files, a
   format or lint fix, a mechanical port with a test to check it, or
   gathering facts for you to judge. `by spawn --model small` (or a model
   name) gives such a child a cheaper, faster model, so the same budget
   goes further; keep your own model, the default, for design, debugging
   and anything you would have to redo. A harness that cannot choose a model
   refuses the spawn and says why; `by inspect <child>` shows its model.
3. Wait. Either way works:
   - **End a managed turn when native work is finished.** When every child
     you delegated has settled, Branchyard starts your next turn by itself
     with a summary of each child (status, diffstat, cost, last message,
     why it failed) and what you can do next. Do not leave a background
     task or monitor polling for them. A turn with background work still
     running is held until it finishes (within your turn's limits, and
     at most 30 minutes without a duration limit), so a poll that never
     ends keeps your turn open until that bound stops it. A bound that
     cuts the hold does not cut your answer: the turn still ends
     completed, with a warning naming the background tasks still running
     (those are stopped), so you are parked and woken as usual. Never run `by wait` or an
     orchestration script as a backgrounded shell command and end your
     turn to wait for it: a backgrounded shell command belongs to your own
     session, and once your turn's limits end the hold it dies with the
     session, unlike your children. Either wait with `by wait` in the
     foreground, or just end your turn; Branchyard itself wakes you.
   - **Wait in this turn.** `by wait` blocks until your running children
     have settled (`by wait a b`, `--any` for the first, `--timeout S`);
     it is woken by Branchyard, not a poll. It prints each child as it
     settles (with `--json`, a JSON line on stderr; stdout keeps the one
     result). Keep `--timeout S` under any limit of your own (a shell
     `timeout`, your tool call's time limit): a wait stopped from outside
     prints no result, only the children that settled before. `by spawn
     ... --wait` waits for one child.
   `by inspect <child>` shows status, diffstat, cost and its last message;
   `by events <child>` shows its activity. To correct a child that is still
   running, `by steer <child> "<text>"` (the same as
   `by send <child> --steer "<text>"`) adds to its running turn
   without stopping it. For long text, or text with backticks or quotes a
   shell would mangle, write it to a file and pass `--prompt-file <path>`
   (`-` for standard input) to `by send` or `by steer` instead. Its answer starts with `accepted:` once the running
   turn took it, and says when the child reads it (for Claude Code, before
   its next model call; never in a later turn), or with `written:` if the
   harness has not confirmed it yet. Some harnesses cannot
   take it, and the refusal says so.
   A child whose turn was cut off because the engine running it stopped
   says so in its status; `by send <child> --retry` submits that turn's
   prompt again.
