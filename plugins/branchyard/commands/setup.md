---
description: Set up Branchyard by interview (project defaults, a server, a rig, a deployment, or these skills)
argument-hint: "[project|server|rig|deploy|plugin]"
---

Set up Branchyard with the `setup` skill for the topic "$ARGUMENTS" (if
empty, run `by init --json` and ask the user which topic).

Drive `by init <topic> --json --next`, ask the user every batch with
AskUserQuestion, show the dry-run diff and validation, and apply only
after the user confirms. Never ask for or write a secret's value.
