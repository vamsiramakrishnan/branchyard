# Branchyard

This repository runs work on Branchyard branches (`by`). Copy the section for your harness into the repository's own AGENTS.md (or the instructions file your harness reads) when the harness has no hook that can tell a session what changed; docs/harness-hooks.md has the harnesses that do.

<!-- harness sections below; one ## per harness -->

## Goose and the rest

Before you start a task, and again before you report back, run this check:

```sh
printf '{"cwd": "%s"}' "$PWD" | by hook --harness generic user-prompt-submit 2>/dev/null
```

It prints nothing when nothing changed since the last time you ran it. Otherwise it prints `{"context": "..."}`, and the lines in `context` name the branches that finished, failed, are blocked, wait for a plan's approval or ask a question, each with the `by` command that deals with it.

Tell the person each question or failure in one line, with the branch's name. Do not answer, merge, discard or cancel anything unless they ask you to. `by ls` lists every branch and `by show NAME` shows one.

Questions from a branch's children sit in the parent's inbox. A line that says `(under PARENT)` names it: read it with `by inbox --as PARENT --unread`.
