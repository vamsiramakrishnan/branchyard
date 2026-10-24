# Branchyard

This repository runs work on Branchyard branches (`by`). Copy the section for your harness into the repository's own AGENTS.md (or the instructions file your harness reads) when the harness has no hook that can tell a session what changed; docs/harness-hooks.md has the harnesses that do.

<!-- harness sections below; one ## per harness -->

## Goose and the rest

Before you start a task, and again before you report back, run `printf '{"cwd": "%s"}' "$PWD" | by hook --harness generic user-prompt-submit 2>/dev/null` and act on the `context` of what it prints: branches that finished, failed, are blocked or ask a question since the last run. When it prints nothing, nothing changed. `by ls` lists every branch; `by inbox --unread` shows the questions.
