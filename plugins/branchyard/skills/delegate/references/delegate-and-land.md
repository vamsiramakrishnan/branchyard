# Delegate reference: A managed code lead can delegate and land

Read when: you split code by ownership and want the engine to create the children and integrate them (by delegate --plan --land).

## A managed code lead can delegate and land

When automatic code landing is authorized, say the split once and let the
engine run it. After your own native work finishes, you may end the managed
turn and receive the children's results on your next turn.

1. **Split by ownership.** Give each part the paths it owns (`owns`), so no
   two children touch the same file: siblings that own disjoint paths cannot
   conflict, and a change a child makes outside its paths is refused when it
   lands. Files every part needs, such as a lockfile, belong to no child:
   regenerate them yourself afterwards.
2. **Settle the interfaces first, in a brief.** The brief is what every
   child shares and no prompt should repeat: the goal, the conventions, and
   the contracts between the parts (a type, a function signature, a file
   format). Children are given it each turn. A decision you make later, such
   as an interface settled after a child's question, is one more entry:
   `by brief "The lexer returns Spanned<Token>."`; children running now are
   sent it at once.
3. **Delegate the plan, left to land.** One call creates every child, all
   or none:

   ```sh
   by delegate --plan "$TMPDIR/plan.json" --land
   ```

   ```json
   {"brief": "Tokens are the Token type in src/token.rs. Add no crates.",
    "review": {"harness": "codex", "budget_usd": 0.50},
    "tasks": [
      {"name": "lexer", "prompt": "Write the lexer in src/lexer; run cargo test -p lexer",
       "owns": ["src/lexer"]},
      {"name": "parser", "prompt": "Write the parser in src/parser; run cargo test -p parser",
       "owns": ["src/parser"], "depends_on": ["lexer"], "after": "integrated"}]}
   ```

   With `--land` (or `"land": true` in the plan), the engine integrates each
   child into your branch when it is ready, as `by integrate` would, its
   check on the exact merge included. A child that conflicts with your
   branch is sent a turn to merge your branch and resolve it; one whose
   check fails on the merge is sent the failure to fix; one that changed
   paths it does not own is told to undo them. At most two such turns; then
   it is left for you, with why. With `review` (a harness you may delegate
   to, ideally another vendor's than the children's), a reviewer reads each
   child's work before it lands; its findings go back to the child as a turn
   to fix them. Give the review a `budget_usd` when you have a cost limit:
   each review is a child of yours and spends from your budget. A task may
   name `"harness": "auto"`: the router places it on the harness you may run
   whose recorded outcomes for that kind of task are best, preferring one
   with a free slot, and records why. A child whose harness is full waits
   for a free slot and starts by itself (`waiting_for` says so).
4. **End your turn.** Do not wait, poll or integrate. When everything has
   settled, Branchyard starts your next turn with a summary: what landed in
   your branch (it is in your worktree and your candidate already), what
   was left for you and why, and each child's status, cost and last message.
5. **Finish.** Read what landed, run `by check` on the whole, fix what is
   left (`by send <child> "<fix>"`, or do it yourself; integrate a child
   left for you with `by integrate <child>` once it is fixed), and end your
   turn.

`by delegate` is also the `delegate` tool's arguments (`{"tasks", "brief",
"land", "review"}`), `branchyard.delegate(tasks, brief=..., land=True,
review="codex")` in Python, and `Delegate::delegate` in Rust. A single child
works the same way: `by spawn "..." --owns src/x --land --review codex`.
`by inspect <child>` shows its `landing` (where it landed, or why not, and
the turns sent to fix it) and its `review`.
