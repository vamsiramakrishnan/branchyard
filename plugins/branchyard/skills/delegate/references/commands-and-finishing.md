# Delegate reference: Commits, scratch files, finishing and the by cheatsheet

Read when: you need the full by command list, the commit and scratch rules, or what to run before you end a turn.

## Commits and scratch files

On a managed branch, Branchyard commits edited files at the end of your
turns, and in each child's, as commits named `by/<branch>: turn <n>`: you need
not commit, and those commits are Branchyard's. Ending your turn to wait for
your children commits your work the same way. Write scratch files under
`$TMPDIR`, a directory private to your branch, not in `/tmp`, which other
branches share.

`by skills ls` shows the skills your branch runs with: the built-in ones and,
when a person trusted them or a server's operator allows them, the
repository's `.agents/skills/` from the commit your branch started at. They are fixed when the branch is created and your
children inherit them, so a skill you edit or commit is not one until a person
merges it and trusts the new set with `by skills trust`, outside any branch.

## Before you finish

Finish your native work before returning a final answer. For a structured
deliverable, return the current turn's valid final JSON, or an explicit accepted
result. Do not manufacture Git changes or require a code check for findings.

For code with a configured check, run `by check` before ending a managed turn. It runs your branch's check (your
own, or the one you inherited) on your work as it is, uncommitted files
included, merged into your parent's branch, the way your parent's
integration will; nothing is committed or integrated. What fails here
fails your integration: a repository rule you never saw is in its output.
Fix it and run `by check` again until it passes. It exits 1 when the check
fails, times out or your work conflicts with your parent's branch, and
says so when you have no check to run. For a conflict, merge your
parent's branch into your work (`git merge by/<parent>`), resolve the
files it names, commit the merge, and run `by check` again.

## With `by`

```sh
by inspect                      # your own branch: budget left, children
by check                        # your work, checked as your integration will be
by spawn "Make tests/parser_test.rs deterministic; run cargo test -p parser" \
  --name parser-flake --harness codex --budget-usd 0.50 --max-turns 3
by spawn "Document the retry policy in docs/retries.md" --name retry-docs --budget-usd 0.20
by spawn "Rename parse_opts to parse_options everywhere; run cargo test" \
  --name rename --model small --budget-usd 0.10 \
  --why "mechanical rename with a test; the cheapest tier does these"   # recorded on the child
by spawn --prompt-file "$TMPDIR/port-task.md" --name port --budget-usd 0.50   # a long task
by children
by inspect parser-flake
by events parser-flake --cursor 0
by spawn "Add a regression test for issue 42" --name issue-42 --budget-usd 0.30 --wait
# Ask for the answer as JSON, in a shape (see "Asking for a result" below)
by spawn "List the files that call parse_opts" --name callers \
  --result-schema '{"type":"object","required":["files"],"properties":{"files":{"type":"array","items":{"type":"string"}}}}' \
  --result-required
by steer parser-flake "Use the fixture in tests/data, not a new one."
by wait parser-flake retry-docs --timeout 600
by send parser-flake "The seed must come from the test name, not the clock."
by send parser-flake --prompt-file "$TMPDIR/review.md"   # a long or quoted follow-up
by integrate parser-flake retry-docs
by cancel retry-docs
by discard retry-docs --reason "superseded by parser-flake"
```

`by inspect` does not wait. Use `by wait` in a managed branch, or end a
managed turn after your native work finishes.

Every command takes `--json` for a stable machine-readable result, and
exits non-zero with `{"error": {"kind", "message"}}` on refusal. Run `by`
by name or as `$BRANCHYARD_BY`; call it as a single command without pipes,
`&&` or substitutions, so a permission policy can recognize it.
