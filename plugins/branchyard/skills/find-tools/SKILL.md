---
name: find-tools
description: "Search the tools mounted for this turn (skills, connector operations, helpers) with BM25, then show one or print an operation's schema."
---
# Find a tool

Your tools are at `$BRANCHYARD_TOOLS` (`/tools`) in a sandbox, else `.branchyard/tools/` in your home. Search before you read:

- `tools search WORDS` ranks skills, their references, connector operations and helpers.
- `tools show ID` prints a skill, `SKILL/references/FILE.md`, a connector, `CONNECTOR.OP` or a helper.
- `tools schema CONNECTOR.OP` prints an operation's input schema; else run `bin/<id> --schema`.

`tools` is `bin/tools` there. Open a skill's `SKILL.md`, or one reference, only after a search names it; a connector's `reference/` only for the operation you need.
